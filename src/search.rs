//! Regex compilation、logical-line scan、incremental input、非同期検索の世代を所有する。
//!
//! scanner は document model に依存せず、呼び出し側が stable line ID と text の snapshot を渡す。
//! 結果の span は UTF-8 byte offset であり、renderer は同じ sanitized text に対してだけ適用する。
//! 16 MiB を超える表示textは表示対象のまま検索から除外し、除外数を結果へ積算する。Scannerは
//! `SearchSource`から1行ずつ読み、background workerの世代変更をline/match境界で検出した場合は
//! 部分resultを破棄する。Source I/O failureはcancelと別のoutcomeとしてcallerへ返す。

use std::num::NonZeroUsize;
use std::ops::Range;

use regex::{Regex, RegexBuilder};
use thiserror::Error;

use crate::text::MAX_SEARCH_LINE_BYTES;

/// 確定検索と反復検索が進む向き。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchDirection {
    Forward,
    Backward,
}

impl SearchDirection {
    /// `N` command が使う逆向きを返す。
    #[must_use]
    pub const fn reversed(self) -> Self {
        match self {
            Self::Forward => Self::Backward,
            Self::Backward => Self::Forward,
        }
    }
}

/// smart-case 設定を含む、再利用可能な regex。
#[derive(Clone, Debug)]
pub struct CompiledSearch {
    pattern: String,
    regex: Regex,
    case_sensitive: bool,
}

impl CompiledSearch {
    #[must_use]
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    #[must_use]
    pub const fn is_case_sensitive(&self) -> bool {
        self.case_sensitive
    }

    #[must_use]
    pub const fn regex(&self) -> &Regex {
        &self.regex
    }
}

/// Pattern 内に Unicode uppercase が1文字でもあれば case-sensitive、それ以外は
/// case-insensitive として Rust regex を compile する。
///
/// 判定は regex の意味解析後ではなく入力文字列に対して行う。したがって character class や escape
/// に含まれる uppercase も明示的な case-sensitive 指定として扱う。
pub fn compile_smart_case(pattern: &str) -> Result<CompiledSearch, regex::Error> {
    let case_sensitive = pattern.chars().any(char::is_uppercase);
    let regex = RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .build()?;
    Ok(CompiledSearch {
        pattern: pattern.to_owned(),
        regex,
        case_sensitive,
    })
}

/// 1回の scan に渡す logical line。`id` は scan 後も document 内で同じ line を指す値を使う。
#[derive(Clone, Copy, Debug)]
pub struct SearchLine<'text, Id> {
    pub id: Id,
    pub text: &'text str,
}

/// Search sourceが1 logical lineを読んだ結果。
///
/// `Oversized`はmetadataだけで判定したlineを表し、scannerはregexを実行せずskip数へ1回だけ加える。
/// `Cancelled`はI/O failureではなく、世代変更によって途中のdecode結果を破棄した状態である。
#[derive(Clone, Copy, Debug)]
pub enum SearchLineRead<'text, Id> {
    Searchable(SearchLine<'text, Id>),
    Oversized { line_id: Id },
    Cancelled,
}

impl<'text, Id> SearchLine<'text, Id> {
    #[must_use]
    pub const fn new(id: Id, text: &'text str) -> Self {
        Self { id, text }
    }
}

/// Search対象をlogical-line indexで読むためのaccessor。
///
/// Scannerは全lineの参照配列を先に作らず、走査位置だけをこのtraitへ問い合わせる。`read_line`は
/// mutable accessなので、disk-backed実装は1つのdecode bufferを次の呼び出しまで再利用できる。
/// I/Oやdecode failureはassociated errorで返し、oversized/cancellationとは区別する。
pub trait SearchSource<Id> {
    /// Lineの読み出しに失敗した理由。
    type Error;

    /// 現在のimmutable snapshotに含まれるlogical-line数。
    fn line_count(&self) -> usize;

    /// `index`を検索可能text、oversized skip、cancellationのいずれかへ分類する。
    ///
    /// 本文のborrowは次のmutable accessより前に解放する。Disk-backed sourceはtext decodeより先に
    /// metadata capを判定し、長いraw rangeのdecode中もpredicateへ定期的に制御を戻す。
    fn read_line(
        &mut self,
        index: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<SearchLineRead<'_, Id>, Self::Error>;

    /// Snapshotが空かを返す。
    fn is_empty(&self) -> bool {
        self.line_count() == 0
    }
}

#[derive(Clone, Copy, Debug)]
struct SliceSearchSourceError;

struct SliceSearchSource<'lines, 'text, Id> {
    lines: &'lines [SearchLine<'text, Id>],
}

impl<Id: Clone> SearchSource<Id> for SliceSearchSource<'_, '_, Id> {
    type Error = SliceSearchSourceError;

    fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn read_line(
        &mut self,
        index: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<SearchLineRead<'_, Id>, Self::Error> {
        if cancelled() {
            return Ok(SearchLineRead::Cancelled);
        }
        let line = self
            .lines
            .get(index)
            .map(|line| SearchLine::new(line.id.clone(), line.text))
            .ok_or(SliceSearchSourceError)?;
        if line.text.len() > MAX_SEARCH_LINE_BYTES {
            Ok(SearchLineRead::Oversized { line_id: line.id })
        } else {
            Ok(SearchLineRead::Searchable(line))
        }
    }
}

/// Scan を開始する logical position。
///
/// `At` の byte offset は inclusive boundary である。forward は match の start が boundary 以上、
/// backward は match の end が boundary 以下の候補を使う。UTF-8 scalar の途中を指す offset は
/// forward では次、backward では前の scalar boundary へ丸める。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchAnchor {
    BeforeDocument,
    At {
        line_index: usize,
        byte_offset: usize,
    },
    AfterDocument,
}

impl SearchAnchor {
    #[must_use]
    pub const fn at(line_index: usize, byte_offset: usize) -> Self {
        Self::At {
            line_index,
            byte_offset,
        }
    }
}

/// Search result の source span。`byte_range` は折り返し前 line の UTF-8 byte range。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchMatch<Id> {
    pub line_id: Id,
    pub line_index: usize,
    pub byte_range: Range<usize>,
}

impl<Id> SearchMatch<Id> {
    #[must_use]
    pub fn is_zero_width(&self) -> bool {
        self.byte_range.is_empty()
    }
}

/// Viewport 内の弱い highlight を描くための1行分の match span。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineMatchSpans<Id> {
    pub line_id: Id,
    pub line_index: usize,
    pub byte_ranges: Vec<Range<usize>>,
    pub skipped_oversized: bool,
}

/// 1 logical line にある non-overlapping match を入力順で返す。
///
/// `regex::Regex::find_iter` は空 match の後に次の UTF-8 boundary へ進むため、zero-width pattern でも
/// loop しない。oversized line には regex を実行せず、`skipped_oversized` だけを立てる。
#[must_use]
pub fn match_spans_in_line<Id: Clone>(
    compiled: &CompiledSearch,
    line_index: usize,
    line: SearchLine<'_, Id>,
) -> LineMatchSpans<Id> {
    if line.text.len() > MAX_SEARCH_LINE_BYTES {
        return LineMatchSpans {
            line_id: line.id,
            line_index,
            byte_ranges: Vec::new(),
            skipped_oversized: true,
        };
    }

    let byte_ranges = compiled
        .regex
        .find_iter(line.text)
        .map(|found| found.range())
        .collect();
    LineMatchSpans {
        line_id: line.id,
        line_index,
        byte_ranges,
        skipped_oversized: false,
    }
}

/// N番目の候補を探す1回の scan request。`count` は数値 prefix を解決した後の非0値。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanRequest {
    pub direction: SearchDirection,
    pub anchor: SearchAnchor,
    pub count: NonZeroUsize,
}

impl ScanRequest {
    #[must_use]
    pub const fn new(
        direction: SearchDirection,
        anchor: SearchAnchor,
        count: NonZeroUsize,
    ) -> Self {
        Self {
            direction,
            anchor,
            count,
        }
    }
}

/// EOF 前の forward scan が読み込み済み末尾へ達した時の再開位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForwardResume {
    /// 次に append される line の snapshot index。
    pub next_line_index: usize,
    /// すでに見つけた候補を差し引いた残り回数。
    pub remaining_count: NonZeroUsize,
}

/// 1 snapshot に対する scan result。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanResult<Id> {
    pub found: Option<SearchMatch<Id>>,
    pub skipped_oversized_lines: usize,
    /// forward で候補が足りなかった時だけ存在する。EOF 済みかどうかは caller が判断する。
    pub forward_resume: Option<ForwardResume>,
}

/// Cooperative cancellationを含むscanの完了状態。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanOutcome<Id, Error> {
    /// Snapshot全体または要求countまで走査し、適用可能なresultを得た。
    Completed(ScanResult<Id>),
    /// Callerの世代が変わり、部分結果を破棄した。
    Cancelled,
    /// Sourceがlineの読み出しに失敗し、部分結果を破棄した。
    Failed(Error),
}

/// Logical line snapshot を document 順または逆順に scan する。
///
/// backward scan は各 line の候補数を一度数え、対象 ordinal だけを二度目の iterator で取得する。
/// これにより zero-width match が多数ある line でも候補数に比例する `Vec` を確保しない。
#[must_use]
pub fn scan_lines<Id: Clone>(
    compiled: &CompiledSearch,
    lines: &[SearchLine<'_, Id>],
    request: ScanRequest,
) -> ScanResult<Id> {
    let mut source = SliceSearchSource { lines };
    match scan_source(compiled, &mut source, request, &|| false) {
        ScanOutcome::Completed(result) => result,
        // Predicateはfalse固定で、sliceは`0..len`を必ず返す。このarmは型の全列挙性だけを満たす。
        ScanOutcome::Cancelled | ScanOutcome::Failed(_) => match request.direction {
            SearchDirection::Forward => exhausted_forward(lines.len(), request.count.get(), 0),
            SearchDirection::Backward => exhausted_backward(request.count.get(), 0),
        },
    }
}

/// Accessorからlineを逐次取得し、世代変更を検出したら部分結果を破棄する。
///
/// `cancelled`は各logical lineと各matchの間で呼ぶ。Regex engineが1件のmatch候補を探索している
/// 最中は割り込めないが、16 MiB超のlineはregex対象外なので、1回の非協調区間は1 logical lineに
/// 制限される。
#[must_use]
pub fn scan_source<Id: Clone, Source: SearchSource<Id> + ?Sized, Cancel: Fn() -> bool + ?Sized>(
    compiled: &CompiledSearch,
    source: &mut Source,
    request: ScanRequest,
    cancelled: &Cancel,
) -> ScanOutcome<Id, Source::Error> {
    if cancelled() {
        return ScanOutcome::Cancelled;
    }
    match request.direction {
        SearchDirection::Forward => {
            scan_forward(compiled, source, request.anchor, request.count, cancelled)
        }
        SearchDirection::Backward => {
            scan_backward(compiled, source, request.anchor, request.count, cancelled)
        }
    }
}

fn scan_forward<Id: Clone, Source: SearchSource<Id> + ?Sized, Cancel: Fn() -> bool + ?Sized>(
    compiled: &CompiledSearch,
    source: &mut Source,
    anchor: SearchAnchor,
    count: NonZeroUsize,
    cancelled: &Cancel,
) -> ScanOutcome<Id, Source::Error> {
    let line_count = source.line_count();
    let (start_line, first_offset) = match anchor {
        SearchAnchor::BeforeDocument => (0, 0),
        SearchAnchor::At {
            line_index,
            byte_offset,
        } if line_index < line_count => (line_index, byte_offset),
        SearchAnchor::At { .. } | SearchAnchor::AfterDocument => {
            return ScanOutcome::Completed(exhausted_forward(line_count, count.get(), 0));
        }
    };

    let mut remaining = count.get();
    let mut skipped = 0_usize;
    for line_index in start_line..line_count {
        if cancelled() {
            return ScanOutcome::Cancelled;
        }
        let read = match source.read_line(line_index, &cancelled) {
            Ok(read) => read,
            Err(error) => return ScanOutcome::Failed(error),
        };
        let line = match read {
            SearchLineRead::Searchable(line) if line.text.len() <= MAX_SEARCH_LINE_BYTES => line,
            SearchLineRead::Searchable(_) | SearchLineRead::Oversized { .. } => {
                skipped = skipped.saturating_add(1);
                continue;
            }
            SearchLineRead::Cancelled => return ScanOutcome::Cancelled,
        };

        let lower_bound = if line_index == start_line {
            ceil_char_boundary(line.text, first_offset)
        } else {
            0
        };
        for found in compiled.regex.find_iter(line.text) {
            if cancelled() {
                return ScanOutcome::Cancelled;
            }
            if found.start() < lower_bound {
                continue;
            }
            remaining -= 1;
            if remaining == 0 {
                return ScanOutcome::Completed(ScanResult {
                    found: Some(SearchMatch {
                        line_id: line.id,
                        line_index,
                        byte_range: found.range(),
                    }),
                    skipped_oversized_lines: skipped,
                    forward_resume: None,
                });
            }
        }
    }

    if cancelled() {
        ScanOutcome::Cancelled
    } else {
        ScanOutcome::Completed(exhausted_forward(line_count, remaining, skipped))
    }
}

fn exhausted_forward<Id>(
    next_line_index: usize,
    remaining: usize,
    skipped: usize,
) -> ScanResult<Id> {
    ScanResult {
        found: None,
        skipped_oversized_lines: skipped,
        // `remaining` は non-zero request から match 数を引いた値で、ここへ来る時は0にならない。
        forward_resume: NonZeroUsize::new(remaining).map(|remaining_count| ForwardResume {
            next_line_index,
            remaining_count,
        }),
    }
}

fn scan_backward<Id: Clone, Source: SearchSource<Id> + ?Sized, Cancel: Fn() -> bool + ?Sized>(
    compiled: &CompiledSearch,
    source: &mut Source,
    anchor: SearchAnchor,
    count: NonZeroUsize,
    cancelled: &Cancel,
) -> ScanOutcome<Id, Source::Error> {
    let line_count = source.line_count();
    if source.is_empty() || matches!(anchor, SearchAnchor::BeforeDocument) {
        return ScanOutcome::Completed(exhausted_backward(count.get(), 0));
    }

    let (start_line, first_offset) = match anchor {
        SearchAnchor::AfterDocument => (line_count - 1, None),
        SearchAnchor::At {
            line_index,
            byte_offset,
        } if line_index < line_count => (line_index, Some(byte_offset)),
        SearchAnchor::At { .. } | SearchAnchor::BeforeDocument => {
            return ScanOutcome::Completed(exhausted_backward(count.get(), 0));
        }
    };

    let mut remaining = count.get();
    let mut skipped = 0_usize;
    for line_index in (0..=start_line).rev() {
        if cancelled() {
            return ScanOutcome::Cancelled;
        }
        let read = match source.read_line(line_index, &cancelled) {
            Ok(read) => read,
            Err(error) => return ScanOutcome::Failed(error),
        };
        let line = match read {
            SearchLineRead::Searchable(line) if line.text.len() <= MAX_SEARCH_LINE_BYTES => line,
            SearchLineRead::Searchable(_) | SearchLineRead::Oversized { .. } => {
                skipped = skipped.saturating_add(1);
                continue;
            }
            SearchLineRead::Cancelled => return ScanOutcome::Cancelled,
        };

        let upper_bound = if line_index == start_line {
            first_offset.map_or(line.text.len(), |offset| {
                floor_char_boundary(line.text, offset)
            })
        } else {
            line.text.len()
        };
        let mut candidates = 0_usize;
        for found in compiled.regex.find_iter(line.text) {
            if cancelled() {
                return ScanOutcome::Cancelled;
            }
            if found.end() <= upper_bound {
                candidates = candidates.saturating_add(1);
            }
        }
        if candidates < remaining {
            remaining -= candidates;
            continue;
        }

        let ordinal_from_start = candidates - remaining;
        let mut ordinal = 0_usize;
        let mut selected = None;
        for found in compiled.regex.find_iter(line.text) {
            if cancelled() {
                return ScanOutcome::Cancelled;
            }
            if found.end() > upper_bound {
                continue;
            }
            if ordinal == ordinal_from_start {
                selected = Some(found);
                break;
            }
            ordinal = ordinal.saturating_add(1);
        }
        if let Some(found) = selected {
            return ScanOutcome::Completed(ScanResult {
                found: Some(SearchMatch {
                    line_id: line.id,
                    line_index,
                    byte_range: found.range(),
                }),
                skipped_oversized_lines: skipped,
                forward_resume: None,
            });
        }

        // 同じ deterministic iterator を二度使うため通常は到達しない。regex backend の契約が
        // 変わった場合も panic せず、残りの line を探索して not-found へ落とす。
        remaining = 1;
    }

    if cancelled() {
        ScanOutcome::Cancelled
    } else {
        ScanOutcome::Completed(exhausted_backward(remaining, skipped))
    }
}

fn exhausted_backward<Id>(_remaining: usize, skipped: usize) -> ScanResult<Id> {
    ScanResult {
        found: None,
        skipped_oversized_lines: skipped,
        forward_resume: None,
    }
}

/// 現在 match から `n` / `N` を開始する anchor を作る。
///
/// non-empty match は隣接する別 match を許すため start/end boundary をそのまま使う。zero-width
/// match は同じ span を再取得しないよう、指定方向へ1 Unicode scalar進める。line 端では隣の
/// logical line へ移り、document 端では `BeforeDocument` / `AfterDocument` を返す。
#[must_use]
pub fn repeat_anchor<Id>(
    current: &SearchMatch<Id>,
    direction: SearchDirection,
    lines: &[SearchLine<'_, Id>],
) -> SearchAnchor {
    let Some(line) = lines.get(current.line_index) else {
        return match direction {
            SearchDirection::Forward => SearchAnchor::AfterDocument,
            SearchDirection::Backward => SearchAnchor::BeforeDocument,
        };
    };

    if !current.is_zero_width() {
        return match direction {
            SearchDirection::Forward => {
                SearchAnchor::at(current.line_index, current.byte_range.end)
            }
            SearchDirection::Backward => {
                SearchAnchor::at(current.line_index, current.byte_range.start)
            }
        };
    }

    let offset = floor_char_boundary(line.text, current.byte_range.start);
    match direction {
        SearchDirection::Forward => {
            if let Some(next) = next_scalar_boundary(line.text, offset) {
                SearchAnchor::at(current.line_index, next)
            } else if current.line_index + 1 < lines.len() {
                SearchAnchor::at(current.line_index + 1, 0)
            } else {
                SearchAnchor::AfterDocument
            }
        }
        SearchDirection::Backward => {
            if let Some(previous) = previous_scalar_boundary(line.text, offset) {
                SearchAnchor::at(current.line_index, previous)
            } else if current.line_index > 0 {
                let previous_line = current.line_index - 1;
                SearchAnchor::at(previous_line, lines[previous_line].text.len())
            } else {
                SearchAnchor::BeforeDocument
            }
        }
    }
}

fn ceil_char_boundary(text: &str, offset: usize) -> usize {
    let mut boundary = offset.min(text.len());
    while boundary < text.len() && !text.is_char_boundary(boundary) {
        boundary += 1;
    }
    boundary
}

fn floor_char_boundary(text: &str, offset: usize) -> usize {
    let mut boundary = offset.min(text.len());
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

fn next_scalar_boundary(text: &str, offset: usize) -> Option<usize> {
    if offset >= text.len() {
        return None;
    }
    text.get(offset..)
        .and_then(|suffix| suffix.chars().next())
        .map(|character| offset + character.len_utf8())
}

fn previous_scalar_boundary(text: &str, offset: usize) -> Option<usize> {
    text.get(..offset)
        .and_then(|prefix| prefix.char_indices().next_back())
        .map(|(index, _)| index)
}

/// `/` と `?` が共有する process-local history。
///
/// browse 開始時の未確定 draft を保持し、newest より先へ進んだ時に復元する。disk への読み書きは
/// 行わない。caller は history entry を編集し始めた時に `reset_navigation` を呼ぶ。
#[derive(Clone, Debug, Default)]
pub struct SearchHistory {
    entries: Vec<String>,
    cursor: Option<usize>,
    draft: Option<String>,
}

impl SearchHistory {
    /// 確定した non-empty pattern を末尾へ追加する。
    pub fn record(&mut self, pattern: impl Into<String>) {
        let pattern = pattern.into();
        if pattern.is_empty() {
            return;
        }
        self.entries.push(pattern);
        self.reset_navigation();
    }

    #[must_use]
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// `Up` に対応し、古い entry へ1件進む。
    pub fn older(&mut self, current_draft: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }

        let index = match self.cursor {
            Some(index) => index.saturating_sub(1),
            None => {
                self.draft = Some(current_draft.to_owned());
                self.entries.len() - 1
            }
        };
        self.cursor = Some(index);
        self.entries.get(index).cloned()
    }

    /// `Down` に対応し、newest を越えた時は browse 前の draft を返す。
    pub fn newer(&mut self) -> Option<String> {
        let cursor = self.cursor?;
        if cursor + 1 < self.entries.len() {
            let next = cursor + 1;
            self.cursor = Some(next);
            return self.entries.get(next).cloned();
        }

        self.cursor = None;
        self.draft.take()
    }

    pub fn reset_navigation(&mut self) {
        self.cursor = None;
        self.draft = None;
    }
}

/// 入力中 query の compile 状態。不正 regex では直前の preview を画面に残す。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IncrementalQueryStatus {
    Empty,
    Valid,
    Invalid(String),
}

/// Incremental scan が表示へ渡す snapshot。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchPreview<Id> {
    pub found: Option<SearchMatch<Id>>,
    pub skipped_oversized_lines: usize,
}

/// `/` または `?` を開いてから Enter/Esc までの query state。
#[derive(Clone, Debug)]
pub struct IncrementalQuery<Id> {
    direction: SearchDirection,
    original_anchor: SearchAnchor,
    query: String,
    status: IncrementalQueryStatus,
    last_valid_compiled: Option<CompiledSearch>,
    displayed_preview: Option<SearchPreview<Id>>,
    preview_is_current: bool,
}

impl<Id> IncrementalQuery<Id> {
    fn new(direction: SearchDirection, original_anchor: SearchAnchor) -> Self {
        Self {
            direction,
            original_anchor,
            query: String::new(),
            status: IncrementalQueryStatus::Empty,
            last_valid_compiled: None,
            displayed_preview: None,
            preview_is_current: false,
        }
    }

    #[must_use]
    pub const fn direction(&self) -> SearchDirection {
        self.direction
    }

    #[must_use]
    pub const fn original_anchor(&self) -> SearchAnchor {
        self.original_anchor
    }

    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    #[must_use]
    pub const fn status(&self) -> &IncrementalQueryStatus {
        &self.status
    }

    /// 現在の query 自体が valid な場合だけ worker へ渡す regex を返す。
    #[must_use]
    pub fn compiled_for_current_query(&self) -> Option<&CompiledSearch> {
        if matches!(self.status, IncrementalQueryStatus::Valid) {
            self.last_valid_compiled.as_ref()
        } else {
            None
        }
    }

    /// 不正 query の入力中も、最後に適用済みの valid preview を返す。空 query は開始時の
    /// viewportへ戻るため、内部に旧previewが残っていても表示対象にはしない。
    #[must_use]
    pub fn displayed_preview(&self) -> Option<&SearchPreview<Id>> {
        (!matches!(self.status, IncrementalQueryStatus::Empty))
            .then_some(self.displayed_preview.as_ref())
            .flatten()
    }

    /// 画面に残すpreviewと対応するregexを返す。現在queryが不正でも直前のvalid regexを返し、
    /// rendererがcurrent matchだけでなくviewport内の弱いmatchも同じpatternで維持できるようにする。
    #[must_use]
    pub fn displayed_compiled(&self) -> Option<&CompiledSearch> {
        self.displayed_preview()?;
        self.last_valid_compiled.as_ref()
    }

    fn current_preview(&self) -> Option<&SearchPreview<Id>> {
        if self.preview_is_current && matches!(self.status, IncrementalQueryStatus::Valid) {
            self.displayed_preview.as_ref()
        } else {
            None
        }
    }

    fn update(&mut self, query: &str) -> IncrementalQueryStatus {
        self.query.clear();
        self.query.push_str(query);
        self.preview_is_current = false;

        if query.is_empty() {
            self.status = IncrementalQueryStatus::Empty;
            return self.status.clone();
        }

        match compile_smart_case(query) {
            Ok(compiled) => {
                self.last_valid_compiled = Some(compiled);
                self.status = IncrementalQueryStatus::Valid;
            }
            Err(error) => {
                self.status = IncrementalQueryStatus::Invalid(error.to_string());
            }
        }
        self.status.clone()
    }

    fn apply_preview(&mut self, preview: SearchPreview<Id>) -> bool {
        if !matches!(self.status, IncrementalQueryStatus::Valid) {
            return false;
        }
        self.displayed_preview = Some(preview);
        self.preview_is_current = true;
        true
    }
}

/// 非同期 request/result の対応に使う単調増加 token。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SearchGeneration(u64);

impl SearchGeneration {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// `n` と `N` の意味。向きは確定 pattern が開始された方向に対して決まる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepeatCommand {
    Next,
    Opposite,
}

/// Enter 済み pattern と、`n` が継続する方向。
#[derive(Clone, Debug)]
pub struct ActiveSearch {
    compiled: CompiledSearch,
    direction: SearchDirection,
}

impl ActiveSearch {
    #[must_use]
    pub const fn compiled(&self) -> &CompiledSearch {
        &self.compiled
    }

    #[must_use]
    pub const fn direction(&self) -> SearchDirection {
        self.direction
    }
}

/// Incoming data を待つ forward search intent。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingForward {
    pub generation: SearchGeneration,
    pub next_line_index: usize,
    pub remaining_count: NonZeroUsize,
    pub skipped_oversized_lines: usize,
}

impl PendingForward {
    /// Document に新しい line が append された後、同じ intent を再開する request を返す。
    #[must_use]
    pub const fn request(self) -> ScanRequest {
        ScanRequest::new(
            SearchDirection::Forward,
            SearchAnchor::at(self.next_line_index, 0),
            self.remaining_count,
        )
    }
}

/// Query edit が発行した worker generation と compile status。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncrementalUpdate {
    pub generation: SearchGeneration,
    pub status: IncrementalQueryStatus,
}

/// Incremental input の確定に失敗した理由。
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CommitIncrementalError {
    #[error("search input is not active")]
    NotActive,
    #[error("no previous search")]
    NoPreviousSearch,
    #[error("invalid regular expression: {0}")]
    InvalidRegex(String),
}

/// Enter により確定した search。caller は `origin` から scan を開始する。
#[derive(Clone, Debug)]
pub struct CommittedSearch<Id> {
    pub generation: SearchGeneration,
    pub compiled: CompiledSearch,
    pub direction: SearchDirection,
    pub origin: SearchAnchor,
    pub preview: Option<SearchPreview<Id>>,
    pub reused_previous_pattern: bool,
}

/// Search UI と background result の間にある pure session state。
///
/// 新しい navigation intent は generation を進めて pending-forward を破棄する。worker 自体がまだ
/// 実行中でも、旧 generation の結果は `apply_*` が拒否するため viewport を後から奪わない。
#[derive(Clone, Debug)]
pub struct SearchSession<Id> {
    generation: SearchGeneration,
    active: Option<ActiveSearch>,
    current_match: Option<SearchMatch<Id>>,
    history: SearchHistory,
    incremental: Option<IncrementalQuery<Id>>,
    pending_forward: Option<PendingForward>,
    last_skipped_oversized_lines: usize,
}

impl<Id> Default for SearchSession<Id> {
    fn default() -> Self {
        Self {
            generation: SearchGeneration::default(),
            active: None,
            current_match: None,
            history: SearchHistory::default(),
            incremental: None,
            pending_forward: None,
            last_skipped_oversized_lines: 0,
        }
    }
}

impl<Id> SearchSession<Id> {
    #[must_use]
    pub const fn generation(&self) -> SearchGeneration {
        self.generation
    }

    #[must_use]
    pub const fn active(&self) -> Option<&ActiveSearch> {
        self.active.as_ref()
    }

    #[must_use]
    pub const fn current_match(&self) -> Option<&SearchMatch<Id>> {
        self.current_match.as_ref()
    }

    #[must_use]
    pub const fn incremental(&self) -> Option<&IncrementalQuery<Id>> {
        self.incremental.as_ref()
    }

    #[must_use]
    pub const fn pending_forward(&self) -> Option<&PendingForward> {
        self.pending_forward.as_ref()
    }

    #[must_use]
    pub const fn last_skipped_oversized_lines(&self) -> usize {
        self.last_skipped_oversized_lines
    }

    #[must_use]
    pub const fn history(&self) -> &SearchHistory {
        &self.history
    }

    pub const fn history_mut(&mut self) -> &mut SearchHistory {
        &mut self.history
    }

    /// Testや非incremental caller が compile 済み pattern を直接確定する。
    pub fn activate(
        &mut self,
        compiled: CompiledSearch,
        direction: SearchDirection,
    ) -> SearchGeneration {
        let generation = self.advance_generation();
        self.history.record(compiled.pattern());
        self.active = Some(ActiveSearch {
            compiled,
            direction,
        });
        self.current_match = None;
        self.incremental = None;
        generation
    }

    /// `/` / `?` input を開始し、Esc で戻る anchor を保存する。
    pub fn begin_incremental(
        &mut self,
        direction: SearchDirection,
        original_anchor: SearchAnchor,
    ) -> SearchGeneration {
        let generation = self.advance_generation();
        self.incremental = Some(IncrementalQuery::new(direction, original_anchor));
        generation
    }

    /// Query edit ごとに generation を進め、有効なら worker が使う regex を更新する。
    pub fn update_incremental(&mut self, query: &str) -> Option<IncrementalUpdate> {
        self.incremental.as_ref()?;
        let generation = self.advance_generation();
        self.history.reset_navigation();
        let status = self.incremental.as_mut()?.update(query);
        Some(IncrementalUpdate { generation, status })
    }

    /// Current generation の incremental preview だけを受理する。preview scan は incoming data 待機を
    /// 開始せず、Enter 後の `apply_scan_result` だけが pending state を作る。
    pub fn apply_incremental_preview(
        &mut self,
        generation: SearchGeneration,
        result: ScanResult<Id>,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        let Some(incremental) = self.incremental.as_mut() else {
            return false;
        };
        incremental.apply_preview(SearchPreview {
            found: result.found,
            skipped_oversized_lines: result.skipped_oversized_lines,
        })
    }

    /// Esc で incremental input を破棄し、入力開始時の viewport anchor を返す。
    pub fn cancel_incremental(&mut self) -> Option<SearchAnchor> {
        let original_anchor = self.incremental.take()?.original_anchor;
        self.advance_generation();
        Some(original_anchor)
    }

    /// Enter で query を確定する。空 query は既存 regex を入力方向で再利用し、history へ空 entry を
    /// 追加しない。不正 regex では input mode を維持する。
    pub fn commit_incremental(&mut self) -> Result<CommittedSearch<Id>, CommitIncrementalError>
    where
        Id: Clone,
    {
        let incremental = self
            .incremental
            .as_ref()
            .ok_or(CommitIncrementalError::NotActive)?;
        let direction = incremental.direction;
        let origin = incremental.original_anchor;

        let (compiled, preview, reused_previous_pattern) = match &incremental.status {
            IncrementalQueryStatus::Empty => {
                let active = self
                    .active
                    .as_ref()
                    .ok_or(CommitIncrementalError::NoPreviousSearch)?;
                (active.compiled.clone(), None, true)
            }
            IncrementalQueryStatus::Valid => {
                let compiled = incremental
                    .compiled_for_current_query()
                    .cloned()
                    .ok_or_else(|| {
                        CommitIncrementalError::InvalidRegex(incremental.query.clone())
                    })?;
                (compiled, incremental.current_preview().cloned(), false)
            }
            IncrementalQueryStatus::Invalid(error) => {
                return Err(CommitIncrementalError::InvalidRegex(error.clone()));
            }
        };

        if !reused_previous_pattern {
            self.history.record(compiled.pattern());
        }
        self.active = Some(ActiveSearch {
            compiled: compiled.clone(),
            direction,
        });
        self.current_match = preview.as_ref().and_then(|value| value.found.clone());
        self.incremental = None;

        Ok(CommittedSearch {
            generation: self.generation,
            compiled,
            direction,
            origin,
            preview,
            reused_previous_pattern,
        })
    }

    /// `n` は確定時の方向、`N` はその逆向きを返す。確定 pattern がなければ `None`。
    #[must_use]
    pub fn direction_for_repeat(&self, command: RepeatCommand) -> Option<SearchDirection> {
        let direction = self.active.as_ref()?.direction;
        Some(match command {
            RepeatCommand::Next => direction,
            RepeatCommand::Opposite => direction.reversed(),
        })
    }

    /// Search、scroll、file jump のような新しい navigation intent の開始 token を返す。
    pub fn begin_navigation_intent(&mut self) -> SearchGeneration {
        self.advance_generation()
    }

    /// Worker result を current match または pending-forward へ反映する。
    ///
    /// `eof` が true の not-found は pending を作らない。既存 pending の継続 resultでは、それ以前に
    /// skip した oversized line 数を新しい result へ加算する。
    pub fn apply_scan_result(
        &mut self,
        generation: SearchGeneration,
        result: ScanResult<Id>,
        eof: bool,
    ) -> bool {
        if generation != self.generation || self.active.is_none() {
            return false;
        }

        let prior_skipped = self
            .pending_forward
            .filter(|pending| pending.generation == generation)
            .map_or(0, |pending| pending.skipped_oversized_lines);
        let skipped = prior_skipped.saturating_add(result.skipped_oversized_lines);
        self.last_skipped_oversized_lines = skipped;

        if let Some(found) = result.found {
            self.current_match = Some(found);
            self.pending_forward = None;
            return true;
        }

        self.pending_forward = if eof {
            None
        } else {
            result.forward_resume.map(|resume| PendingForward {
                generation,
                next_line_index: resume.next_line_index,
                remaining_count: resume.remaining_count,
                skipped_oversized_lines: skipped,
            })
        };
        true
    }

    fn advance_generation(&mut self) -> SearchGeneration {
        self.generation = SearchGeneration(self.generation.0.wrapping_add(1));
        self.pending_forward = None;
        self.last_skipped_oversized_lines = 0;
        self.generation
    }
}
