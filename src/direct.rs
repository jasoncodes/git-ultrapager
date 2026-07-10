//! Alternate screen を使わない direct renderer と共有 visual-row layout。
//!
//! Logical lineごとのlayoutと、上限付きline rangeのdirect outputを所有する。Document全体の
//! visual rowをmaterializeするAPIは持たず、interactive viewportとdirect writerのどちらも
//! [`layout_line`]または巨大行のstreaming layoutを使う。syntax/search/intraline styleはbyte span
//! layerとして後から合成するため、ここでは構造roleとstable IDだけを保持する。

use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::ops::Range;
use std::sync::Arc;

use ratatui::style::{Color, Modifier, Style};
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::decorations::{DecorationComputeError, DecorationSet, LineDecoration};
use crate::document::{
    DocumentError, DocumentLineMetadata, DocumentLineMetadataReader, DocumentLineReader,
    DocumentLineRef, DocumentSnapshot,
};
use crate::large_line::{LargeLineError, LargeLineStats, LargeLineTextReader};
use crate::model::{CommitId, FileId, HunkId, LineId, LineKind};
use crate::text::{MAX_SYNTAX_LINE_BYTES, wrap_hard};
use crate::theme::{SemanticRole, Theme};

/// Layout parameterの検証またはlogical lineのreflowに失敗した理由。
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum LayoutError {
    /// Tab stopを計算できない幅0が渡された。
    #[error("tab width must be at least 1, got {actual}")]
    InvalidTabWidth { actual: usize },
    /// 現在行の行番号が、呼び出し側から渡されたfile共通gutterへ収まらない。
    #[error(
        "gutter widths old={old_width}, new={new_width} are smaller than the line-number widths old={required_old_width}, new={required_new_width}"
    )]
    UndersizedGutter {
        old_width: usize,
        required_old_width: usize,
        new_width: usize,
        required_new_width: usize,
    },
    /// Gutterと本文1cellをterminal幅へ収められない。
    #[error("terminal width {actual} is smaller than the required width {required}")]
    TooNarrow { actual: usize, required: usize },
    /// 表示幅を消費しないtextが1 rowへ集中し、bounded row buffer上限を越えた。
    #[error("one visual row exceeds the bounded {limit}-byte streaming buffer")]
    PathologicalRowBytes { limit: usize },
    /// Bounded rowまたはmapping bufferの確保に失敗した。
    #[error("could not allocate {requested} bytes for a streaming visual row")]
    StreamingRowAllocation { requested: usize },
    /// Full per-line layoutが保持するrow数の保守的上限を越える。
    #[error(
        "estimated eager visual rows {estimated_rows} exceed the per-line limit {limit}; use streaming layout"
    )]
    EagerRowLimitExceeded { estimated_rows: usize, limit: usize },
    /// Full per-line layoutのtext、row、mapping概算がderived-data shareを越える。
    #[error(
        "estimated eager line-layout weight {estimated_bytes} bytes exceeds the per-line limit {limit} bytes; use streaming layout"
    )]
    EagerLayoutWeightExceeded {
        estimated_bytes: usize,
        limit: usize,
    },
    /// Viewport由来ではない件数でbounded windowを作ろうとした。
    #[error("streaming row window requested {requested} rows, exceeding the limit {limit}")]
    StreamingWindowRowLimit { requested: usize, limit: usize },
    /// Current row 1件をcallerのbyte limitへ収められない。
    #[error(
        "streaming row window needs {required} bytes for its current row, exceeding limit {limit}"
    )]
    StreamingWindowByteLimit { required: usize, limit: usize },
}

/// Snapshotのlogical lineを読む処理と、そのlineをvisual rowへ変換する処理のfailure。
///
/// Terminal geometryの不備は[`LayoutError`]、raw storeまたはline tableの読取失敗は
/// [`DocumentError`]として区別する。`MissingLine`はlayoutと異なるsnapshotをwriterへ渡した場合や、
/// snapshotのhigh-water mark内に対応recordがない場合を表す。`UnavailableLineIndex`はsnapshotの
/// line countとstable ID変換の不整合を表す。
#[derive(Debug, Error)]
pub enum DocumentLayoutError {
    #[error(transparent)]
    Layout(#[from] LayoutError),
    #[error(transparent)]
    Document(#[from] DocumentError),
    #[error(transparent)]
    LargeLine(#[from] LargeLineError),
    #[error("document snapshot does not contain logical line {line_id}")]
    MissingLine { line_id: LineId },
    #[error("document snapshot cannot represent logical line index {index} as a stable ID")]
    UnavailableLineIndex { index: usize },
    #[error("logical line {line_id} requires bounded streaming layout")]
    StreamingRequired { line_id: LineId },
    #[error("logical line {line_id} produced no visual rows")]
    MissingVisualRows { line_id: LineId },
}

/// Source行の左右の行番号欄に割り当てるcell数。
///
/// 各幅は`u64`の最大桁数20を含められる`u8`で保持する。2つの幅と固定separatorを
/// `usize`へ変換しても最大514cellなので、terminal幅との加減算前にoverflowしない。
/// Metadata行ではgutterを描かないため、この値は参照されない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GutterSpec {
    old_width: u8,
    new_width: u8,
}

impl GutterSpec {
    /// Parserがfile全体の最大行番号をまだ持たない場合に使う最小幅。
    pub const MINIMUM: Self = Self::new(3, 3);

    /// 左右それぞれの行番号欄を作る。
    ///
    /// 3cell未満では短いdiffのgutterがfileごとに揺れるため3へ切り上げる。呼び出し側は
    /// 同じfileの最大行番号を収める幅を渡し、file内の各行へ同じ値を再利用する。
    #[must_use]
    pub const fn new(old_width: u8, new_width: u8) -> Self {
        Self {
            old_width: if old_width < 3 { 3 } else { old_width },
            new_width: if new_width < 3 { 3 } else { new_width },
        }
    }

    /// 旧file側の行番号欄を占めるcell数を返す。
    #[must_use]
    pub const fn old_width(self) -> usize {
        self.old_width as usize
    }

    /// 新file側の行番号欄を占めるcell数を返す。
    #[must_use]
    pub const fn new_width(self) -> usize {
        self.new_width as usize
    }
}

impl Default for GutterSpec {
    fn default() -> Self {
        Self::MINIMUM
    }
}

/// 画面に描画する1 visual row。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualRow {
    pub line_id: LineId,
    pub commit_id: Option<CommitId>,
    pub file_id: Option<FileId>,
    pub hunk_id: Option<HunkId>,
    pub text: String,
    pub role: SemanticRole,
    /// Sanitized logical line 内で、このrowが表すbyte range。source lineではdiff markerも
    /// offset 0として数える。tabがrow境界で分割された場合は隣接rowと1 byte重なる。
    pub content_range: Range<usize>,
    /// `text` 内で、metadataまたはsource本文が占めるbyte range。sourceのdiff markerは含まない。
    pub rendered_content_range: Range<usize>,
    pub continuation: bool,
    span_map: Vec<SpanMapping>,
}

/// 1 logical lineから作ったvisual row群。
///
/// 内部の`Arc`はtab展開済みtextとlogical/rendered span mappingを含む`VisualRow`を共有する。
/// Viewport cacheが同じline layoutを複数frameから参照しても`String`とmappingを複製しない。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineLayout {
    rows: Arc<Vec<VisualRow>>,
}

impl LineLayout {
    /// このlogical lineを構成するvisual rowを表示順で返す。
    #[must_use]
    pub fn rows(&self) -> &[VisualRow] {
        self.rows.as_slice()
    }

    /// 折り返し後のvisual row数を返す。
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Visual rowが存在しないかを返す。正常なlayoutでは空logical lineも1 rowになる。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Cache値として保持するrow vector、text、span mappingの実capacityを返す。
    ///
    /// Cache keyやLRU bookkeepingはconsumerが加算する。`Arc`のstrong/weak counterと
    /// heap上の`Vec` headerはこのlayoutが所有するためここで計上する。
    #[must_use]
    pub(crate) fn retained_bytes(&self) -> usize {
        let row_storage = self
            .rows
            .capacity()
            .saturating_mul(std::mem::size_of::<VisualRow>());
        let payload = self.rows.iter().fold(0_usize, |total, row| {
            total.saturating_add(row.text.capacity()).saturating_add(
                row.span_map
                    .capacity()
                    .saturating_mul(std::mem::size_of::<SpanMapping>()),
            )
        });
        std::mem::size_of::<LineLayout>()
            .saturating_add(2 * std::mem::size_of::<usize>())
            .saturating_add(std::mem::size_of::<Vec<VisualRow>>())
            .saturating_add(row_storage)
            .saturating_add(payload)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpanMapping {
    pub logical: Range<usize>,
    pub rendered: Range<usize>,
    tab: bool,
}

impl SpanMapping {
    /// Mapping元が1 byteのtabで、rendered rangeがその展開cellかを返す。
    #[must_use]
    pub const fn is_tab(&self) -> bool {
        self.tab
    }
}

impl VisualRow {
    /// Logical/rendered byte対応を表示順で返す。Tab mappingは複数rowへ同じlogical rangeを持ち得る。
    #[must_use]
    pub fn span_mappings(&self) -> &[SpanMapping] {
        &self.span_map
    }

    /// Logical-line byte spanを、このvisual rowの`text` byte spanへ写す。
    ///
    /// Spanがtabを含み、その展開cellが複数rowへ分かれた場合は各rowで該当部分を返す。
    /// Regex、syntax、intralineのspanはこの関数を通し、tab展開後のbyte offsetを推測しない。
    #[must_use]
    pub fn rendered_ranges_for_logical(&self, logical: Range<usize>) -> Vec<Range<usize>> {
        if logical.start > logical.end {
            return Vec::new();
        }

        let mut ranges = Vec::<Range<usize>>::new();
        for mapping in &self.span_map {
            if logical.start >= mapping.logical.end || logical.end <= mapping.logical.start {
                continue;
            }

            let rendered = if mapping.logical.end - mapping.logical.start
                == mapping.rendered.end - mapping.rendered.start
            {
                let start = logical.start.max(mapping.logical.start) - mapping.logical.start;
                let end = logical.end.min(mapping.logical.end) - mapping.logical.start;
                mapping.rendered.start + start..mapping.rendered.start + end
            } else {
                // 1 byteのtabは複数spaceへ展開される。regex spanがtab byteと交差したら、
                // このrowに含まれる展開cellをまとめて強調する。
                mapping.rendered.clone()
            };

            if let Some(previous) = ranges.last_mut()
                && previous.end == rendered.start
            {
                previous.end = rendered.end;
            } else {
                ranges.push(rendered);
            }
        }
        ranges
    }

    /// Logical byte rangeの1 byte以上をこのrowが描画するかを返す。
    ///
    /// Tab mappingは同じ1 byteを複数rowへ写すため、単純な`content_range`比較ではなくmapping自体を
    /// 調べる。Range変換結果をallocateせず、whitespace markerの先頭row判定に使う。
    #[must_use]
    pub fn renders_logical_range(&self, logical: &Range<usize>) -> bool {
        self.span_map.iter().any(|mapping| {
            logical.start < mapping.logical.end && logical.end > mapping.logical.start
        })
    }

    /// 1 logical byte offsetをこのrowのcontent rangeが含むかを返す。
    ///
    /// Range末尾は次rowの先頭なので含めない。Line末尾だけはstream全体を読み終えたcallerが最後のrowへ
    /// fallbackする。
    #[must_use]
    pub fn renders_logical_offset(&self, offset: usize) -> bool {
        self.content_range.contains(&offset)
            || (self.content_range.is_empty() && self.content_range.start == offset)
    }
}

/// 1 rowを`Arc`で保持する時のowned text/mapping capacityを含むweight。
#[must_use]
pub fn visual_row_weight(row: &VisualRow) -> usize {
    std::mem::size_of::<VisualRow>()
        .saturating_add(2 * std::mem::size_of::<usize>())
        .saturating_add(std::mem::size_of::<NavigatorRow>())
        .saturating_add(std::mem::size_of::<Arc<VisualRow>>())
        .saturating_add(row.text.capacity())
        .saturating_add(
            row.span_map
                .capacity()
                .saturating_mul(std::mem::size_of::<SpanMapping>()),
        )
}

/// Sanitized済みの1 logical lineをterminal widthへhard-wrapする。
///
/// この関数は`DocumentSnapshot`や前後行を参照しない。Source行のgutter幅はfile全体で揃える必要が
/// あるため、呼び出し側が同じfileから算出した`GutterSpec`を明示的に渡す。`tab_width == 0`
/// は`InvalidTabWidth`、現在行の番号を収められないgutterは`UndersizedGutter`、本文を1cell
/// も確保できないterminal幅は`TooNarrow`を返す。
pub fn layout_line(
    line: &DocumentLineRef<'_>,
    width: usize,
    tab_width: usize,
    gutter: GutterSpec,
) -> Result<LineLayout, LayoutError> {
    let mut rows = Vec::new();
    append_line_rows(&mut rows, line, width, tab_width, gutter)?;
    Ok(LineLayout {
        rows: Arc::new(rows),
    })
}

/// Per-line eager layoutのrow生成経路。
///
/// 失敗したlineで追加済みのrowは残るためmodule内だけで使い、公開APIは構築途中の
/// bufferを外へ返さない。
fn append_line_rows(
    rows: &mut Vec<VisualRow>,
    line: &DocumentLineRef<'_>,
    width: usize,
    tab_width: usize,
    gutter: GutterSpec,
) -> Result<(), LayoutError> {
    validate_tab_width(tab_width)?;
    let source = is_source_line(line.kind);
    if source {
        validate_gutter(line, gutter)?;
    }
    let content_width = content_width_for(line.kind, width, gutter)?;
    let content_len = if source {
        line.content().len()
    } else {
        line.text.len()
    };
    let estimated_rows = estimated_eager_rows(content_len, content_width, tab_width);
    if estimated_rows > MAX_EAGER_VISUAL_ROWS_PER_LINE {
        return Err(LayoutError::EagerRowLimitExceeded {
            estimated_rows,
            limit: MAX_EAGER_VISUAL_ROWS_PER_LINE,
        });
    }
    let estimated_bytes =
        estimated_eager_layout_bytes(content_len, estimated_rows, line.kind, gutter, tab_width);
    if estimated_bytes > MAX_EAGER_LINE_LAYOUT_BYTES {
        return Err(LayoutError::EagerLayoutWeightExceeded {
            estimated_bytes,
            limit: MAX_EAGER_LINE_LAYOUT_BYTES,
        });
    }
    if source {
        append_source_rows(
            rows,
            line,
            width,
            tab_width,
            gutter.old_width(),
            gutter.new_width(),
        )?;
    } else {
        append_metadata_rows(rows, line, width, tab_width)?;
    }
    Ok(())
}

/// Fixed metadataとgeometryだけで、全文decode前にbounded streaming経路が必要かを返す。
///
/// Raw/displayのどちらかが1 MiBを越えるlineに加え、tabを全byteへ仮定した保守的cell数から
/// 5,000 rowsを越え得るlineを対象にする。False positiveはstreamingへ寄るだけで、eager allocationを
/// 過小評価しない。
pub fn line_requires_streaming_layout(
    metadata: DocumentLineMetadata,
    width: usize,
    tab_width: usize,
    gutter: GutterSpec,
) -> Result<bool, LayoutError> {
    let Some(estimated_bytes) =
        eager_layout_allocation_upper_bound(metadata, width, tab_width, gutter)?
    else {
        return Ok(true);
    };
    Ok(estimated_bytes > MAX_EAGER_LINE_LAYOUT_BYTES)
}

/// Fixed metadataだけから、eager decode後のlayout候補が同時保持するbyte数の上限を返す。
///
/// Rawまたはescaped textが1 MiBを越える場合は`None`で、callerはdecode前にstreamingへ
/// 回す。`Some`は全byteをtabとした展開、row/mapping vectorのgrow余白、source gutter、
/// 構築中のexpanded textと完成rowの共存を含む。Viewportはこの値でstreaming経路を決め、
/// decode後は[`eager_layout_decoded_allocation_upper_bound`]の狭い上限でlayout allocationを
/// admissionする。
pub(crate) fn eager_layout_allocation_upper_bound(
    metadata: DocumentLineMetadata,
    width: usize,
    tab_width: usize,
    gutter: GutterSpec,
) -> Result<Option<usize>, LayoutError> {
    validate_tab_width(tab_width)?;
    if is_source_line(metadata.kind) {
        validate_gutter_metadata(metadata, gutter)?;
    }
    let content_width = content_width_for(metadata.kind, width, gutter)?;
    if metadata.exceeds_eager_text_limit(MAX_SYNTAX_LINE_BYTES) {
        return Ok(None);
    }
    let Some(display_len) = metadata.display_len.exact() else {
        return Ok(None);
    };
    let content_len = display_len.saturating_sub(usize::from(is_source_line(metadata.kind)));
    let estimated_rows = estimated_eager_rows(content_len, content_width, tab_width);
    if estimated_rows > MAX_EAGER_VISUAL_ROWS_PER_LINE {
        return Ok(None);
    }
    Ok(Some(estimated_eager_layout_bytes(
        content_len,
        estimated_rows,
        metadata.kind,
        gutter,
        tab_width,
    )))
}

/// Decode済みtextのtab数を使い、eager layout候補が同時保持するbyte数の上限を狭める。
///
/// Fixed metadata側は全byteをtabとして安全側へ寄せるため、複数lineを同じframeへ準備すると
/// old-frame pinとの合計を大きく見積もる。この関数はtabを最大`tab_width`個のASCII spaceへ
/// 置換した長さを使う。UTF-8 graphemeの表示cell数は元byte数以下なので、この長さから求めた
/// row数も過小評価にならない。戻り値はtext readerのbounded bufferを含まず、layout allocation
/// とcache/prepared payloadだけを対象にする。
pub(crate) fn eager_layout_decoded_allocation_upper_bound(
    line: &DocumentLineRef<'_>,
    width: usize,
    tab_width: usize,
    gutter: GutterSpec,
) -> Result<usize, LayoutError> {
    validate_tab_width(tab_width)?;
    if is_source_line(line.kind) {
        validate_gutter(line, gutter)?;
    }
    let content_width = content_width_for(line.kind, width, gutter)?;
    let content = if is_source_line(line.kind) {
        line.content()
    } else {
        line.text
    };
    let tab_count = content.bytes().filter(|byte| *byte == b'\t').count();
    let expanded_bytes = content
        .len()
        .saturating_add(tab_count.saturating_mul(tab_width.saturating_sub(1)));
    let estimated_rows = estimated_eager_rows(expanded_bytes, content_width, 1);
    Ok(estimated_eager_layout_bytes_from_expanded(
        expanded_bytes,
        estimated_rows,
        line.kind,
        gutter,
    ))
}

/// Startup判定用にvisual row数を`limit`まで数える。
///
/// Logical lineは先頭から順にdecodeするが、row数が`limit`へ達した時点で止める。Auto pagingの
/// thresholdはterminal高なので、入力全体のvisual rowやtextを作らず判定できる。
/// 戻り値が`limit`なら実際のrow数はそれ以上、未満ならsnapshot全体の正確なrow数である。
pub fn count_rendered_rows_up_to(
    document: &DocumentSnapshot,
    width: usize,
    tab_width: usize,
    limit: usize,
) -> Result<usize, DocumentLayoutError> {
    validate_tab_width(tab_width)?;
    if limit == 0 {
        return Ok(0);
    }
    let mut reader = DocumentLineReader::new();
    let mut metadata_reader = DocumentLineMetadataReader::new();
    let mut count = 0_usize;
    for index in 0..document.line_count() {
        let line_id = document
            .line_id_at(index)
            .ok_or(DocumentLayoutError::UnavailableLineIndex { index })?;
        let metadata = metadata_reader
            .read(document, line_id)?
            .ok_or(DocumentLayoutError::MissingLine { line_id })?;
        let gutter = gutter_spec_for_metadata(metadata);
        if line_requires_streaming_layout(metadata, width, tab_width, gutter)? {
            let remaining = limit.saturating_sub(count);
            let (rows, _) = count_large_line_rows_up_to(
                document, line_id, width, tab_width, gutter, remaining,
            )?;
            count = count.saturating_add(rows).min(limit);
            if count == limit {
                break;
            }
            continue;
        }
        let line = reader
            .read(document, line_id)?
            .ok_or(DocumentLayoutError::MissingLine { line_id })?;
        let rows = layout_line(&line, width, tab_width, gutter_spec_for_line(&line))?.len();
        count = count.saturating_add(rows).min(limit);
        if count == limit {
            break;
        }
    }
    Ok(count)
}

/// Streaming row 1件のtext、mapping、structを合わせた保持上限。
pub const MAX_STREAMING_VISUAL_ROW_BYTES: usize = 512 * 1024;

/// Eager `LineLayout` 1件へ保持できるvisual row数の保守的上限。
///
/// 1 MiB未満でもterminal幅1ならrow objectが100万件近く生じる。Viewport/directはこの上限を越える
/// lineを[`LargeLineRows`]へ送り、全文row vectorの構築を避ける。
pub const MAX_EAGER_VISUAL_ROWS_PER_LINE: usize = 5_000;
/// Eager per-line layoutが一時expanded textと完成rowを同時に保持できる概算上限。
pub const MAX_EAGER_LINE_LAYOUT_BYTES: usize = 8 * 1024 * 1024;

/// Tabが複数rowへ分割されるlineで、同じlogical byteを何番目に描くrowへanchorするかを表す。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LargeRowAnchor {
    pub byte_offset: usize,
    pub fragment: usize,
}

/// 1回の巨大行移動で実際に進んだrow数とline境界到達を返す。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LargeLineStep {
    pub anchor: LargeRowAnchor,
    pub rows_moved: usize,
    /// Requested countを使い切る前に先頭または末尾へclampした時だけtrue。
    pub reached_line_boundary: bool,
}

/// Navigatorのcurrent rowから共有するbounded viewport window。
#[derive(Clone, Debug)]
pub struct LargeLineRowWindow {
    pub top: LargeRowAnchor,
    /// `Arc` cloneだけでframeへ渡せるため、row text/span mapをcache ledger外へdeep-copyしない。
    pub rows: Vec<Arc<VisualRow>>,
    pub reached_line_end: bool,
    /// Row countより先に`byte_limit`へ達し、windowを短くしたか。
    pub byte_limited: bool,
    pub retained_bytes: usize,
    pub stats: LargeLineStats,
}

/// 1回のwindowが保持できるrow数。Terminal heightから十分離れた値で、`usize::MAX` allocationを拒む。
pub const MAX_STREAMING_WINDOW_ROWS: usize = 5_000;
/// 1 navigatorのcurrent、look-ahead、budget境界の保留rowを合わせた上限。
pub const MAX_STREAMING_WINDOW_BYTES: usize = MAX_EAGER_LINE_LAYOUT_BYTES;

#[derive(Clone, Debug)]
struct NavigatorRow {
    index: usize,
    anchor: LargeRowAnchor,
    row: Arc<VisualRow>,
}

/// 1 logical line内のforward decode位置とbounded look-ahead windowを保持するnavigator。
///
/// `normalize`は任意anchorを先頭から探すが、正規化後の`step_forward`、`next_row`、`collect_window`は
/// 同じdecoder stateを再利用する。Repeated j/eは既読prefixを再読しない。Backwardはtarget indexを
/// 求めて新しいbounded passを開始し、countに比例する`Vec`を作らない。
#[derive(Debug)]
pub struct LargeLineNavigator {
    document: DocumentSnapshot,
    line_id: LineId,
    width: usize,
    tab_width: usize,
    gutter: GutterSpec,
    stream: LargeLineRows,
    accumulated_stats: LargeLineStats,
    current: Option<NavigatorRow>,
    forward: VecDeque<NavigatorRow>,
    blocked: Option<NavigatorRow>,
    reported_anchor: LargeRowAnchor,
    last_scanned: Option<(Range<usize>, LargeRowAnchor)>,
    next_index: usize,
    stream_ended: bool,
}

impl LargeLineNavigator {
    /// Lineを先頭から開き、`anchor`を現在geometryへ正規化する。
    pub fn new(
        document: &DocumentSnapshot,
        line_id: LineId,
        width: usize,
        tab_width: usize,
        gutter: GutterSpec,
        anchor: LargeRowAnchor,
    ) -> Result<Self, DocumentLayoutError> {
        let stream = LargeLineRows::new(document, line_id, width, tab_width, gutter)?;
        let mut navigator = Self {
            document: document.clone(),
            line_id,
            width,
            tab_width,
            gutter,
            stream,
            accumulated_stats: LargeLineStats::default(),
            current: None,
            forward: VecDeque::new(),
            blocked: None,
            reported_anchor: LargeRowAnchor::default(),
            last_scanned: None,
            next_index: 0,
            stream_ended: false,
        };
        let _ = navigator.normalize(anchor)?;
        Ok(navigator)
    }

    /// Requested byteを含むrowへ移し、byte/fragmentを実在範囲へclampする。
    pub fn normalize(
        &mut self,
        requested: LargeRowAnchor,
    ) -> Result<LargeRowAnchor, DocumentLayoutError> {
        if self.current.is_some() && self.reported_anchor == requested {
            return Ok(self.reported_anchor);
        }
        self.reset_stream()?;
        let mut last = None;
        let mut last_match = None;
        let mut first_end_candidate = None;
        let mut matching_count = 0_usize;
        let mut maximum_end = 0_usize;

        while let Some(entry) = self.read_stream_row()? {
            maximum_end = maximum_end.max(entry.row.content_range.end);
            if first_end_candidate.is_none() && entry.row.content_range.end >= requested.byte_offset
            {
                first_end_candidate = Some(entry.clone());
            }
            if entry.row.renders_logical_offset(requested.byte_offset) {
                if matching_count == requested.fragment {
                    let normalized = LargeRowAnchor {
                        byte_offset: requested.byte_offset,
                        fragment: matching_count,
                    };
                    self.set_current(entry, normalized);
                    return Ok(normalized);
                }
                matching_count = matching_count.saturating_add(1);
                last_match = Some(entry);
                continue;
            }
            if let Some(matched) = last_match.take() {
                self.push_forward(entry)?;
                let normalized = LargeRowAnchor {
                    byte_offset: requested.byte_offset,
                    fragment: matching_count.saturating_sub(1),
                };
                self.set_current(matched, normalized);
                return Ok(normalized);
            }
            last = Some(entry);
        }

        let Some(last) = last_match.or(last) else {
            return Err(DocumentLayoutError::MissingVisualRows {
                line_id: self.line_id,
            });
        };
        let normalized = LargeRowAnchor {
            byte_offset: requested.byte_offset.min(maximum_end),
            fragment: matching_count.saturating_sub(1),
        };
        let selected = if matching_count > 0 || requested.byte_offset >= maximum_end {
            last
        } else {
            first_end_candidate.unwrap_or(last)
        };
        if selected.index != self.next_index.saturating_sub(1) {
            self.position_at_index(selected.index)?;
            self.reported_anchor = normalized;
        } else {
            self.set_current(selected, normalized);
        }
        Ok(normalized)
    }

    #[must_use]
    pub const fn current_anchor(&self) -> LargeRowAnchor {
        self.reported_anchor
    }

    /// Current rowをtext/span-mapのcopyなしで共有する。
    #[must_use]
    pub fn current_row(&self) -> Option<Arc<VisualRow>> {
        self.current.as_ref().map(|entry| Arc::clone(&entry.row))
    }

    /// 次rowへ1件進み、line末尾なら`None`を返す。Decoder positionは次回呼び出しへ残る。
    pub fn next_row(
        &mut self,
    ) -> Result<Option<(LargeRowAnchor, Arc<VisualRow>)>, DocumentLayoutError> {
        let Some(next) = self.take_forward_row()? else {
            return Ok(None);
        };
        let anchor = next.anchor;
        let row = Arc::clone(&next.row);
        self.set_current(next, anchor);
        Ok(Some((anchor, row)))
    }

    /// Current anchorから最大`count` rows進み、末尾ではclampする。
    pub fn step_forward(&mut self, count: usize) -> Result<LargeLineStep, DocumentLayoutError> {
        if count == 0 {
            return Ok(LargeLineStep {
                anchor: self.reported_anchor,
                rows_moved: 0,
                reached_line_boundary: false,
            });
        }
        let mut moved = 0_usize;
        while moved < count {
            let Some(next) = self.take_forward_row()? else {
                break;
            };
            let anchor = next.anchor;
            self.set_current(next, anchor);
            moved = moved.saturating_add(1);
        }
        if moved == 0
            && let Some(current) = &self.current
        {
            self.reported_anchor = current.anchor;
        }
        Ok(LargeLineStep {
            anchor: self.reported_anchor,
            rows_moved: moved,
            reached_line_boundary: moved < count,
        })
    }

    /// Current anchorから最大`count` rows戻り、先頭ではclampする。
    pub fn step_backward(&mut self, count: usize) -> Result<LargeLineStep, DocumentLayoutError> {
        if count == 0 {
            return Ok(LargeLineStep {
                anchor: self.reported_anchor,
                rows_moved: 0,
                reached_line_boundary: false,
            });
        }
        let current_index = self
            .current
            .as_ref()
            .ok_or(DocumentLayoutError::MissingVisualRows {
                line_id: self.line_id,
            })?
            .index;
        let target = current_index.saturating_sub(count);
        let moved = current_index - target;
        self.position_at_index(target)?;
        Ok(LargeLineStep {
            anchor: self.reported_anchor,
            rows_moved: moved,
            reached_line_boundary: moved < count,
        })
    }

    /// Line末尾rowへ進む。既にdecode済みのforward positionから続行する。
    pub fn last(&mut self) -> Result<LargeRowAnchor, DocumentLayoutError> {
        let _ = self.step_forward(usize::MAX)?;
        Ok(self.reported_anchor)
    }

    /// Line末尾を`body_height` rowsへbottom-alignしたtop anchorへ移る。
    pub fn bottom_aligned_top(
        &mut self,
        body_height: usize,
    ) -> Result<LargeRowAnchor, DocumentLayoutError> {
        let _ = self.last()?;
        Ok(self.step_backward(body_height.saturating_sub(1))?.anchor)
    }

    /// Current rowから最大`row_limit`件をArc共有し、windowがline末尾を含むか返す。
    pub fn collect_window(
        &mut self,
        row_limit: usize,
    ) -> Result<LargeLineRowWindow, DocumentLayoutError> {
        self.collect_window_bounded(row_limit, MAX_STREAMING_WINDOW_BYTES)
    }

    /// Row countとretained byteの両方でlook-aheadを止める。
    ///
    /// `byte_limit`はmodule上限へclampする。Current rowが収まらなければ
    /// [`LayoutError::StreamingWindowByteLimit`]を返す。次rowを読む前に1 row分のheadroomを残し、
    /// 実測weightがprepared shareへ収まらなければ`blocked`へ1件だけ置くため、decoder positionを
    /// 巻き戻さない。
    pub fn collect_window_bounded(
        &mut self,
        row_limit: usize,
        byte_limit: usize,
    ) -> Result<LargeLineRowWindow, DocumentLayoutError> {
        if row_limit > MAX_STREAMING_WINDOW_ROWS {
            return Err(LayoutError::StreamingWindowRowLimit {
                requested: row_limit,
                limit: MAX_STREAMING_WINDOW_ROWS,
            }
            .into());
        }
        let byte_limit = byte_limit.min(MAX_STREAMING_WINDOW_BYTES);
        let mut rows = Vec::new();
        let current = self
            .current
            .as_ref()
            .ok_or(DocumentLayoutError::MissingVisualRows {
                line_id: self.line_id,
            })?;
        let current_row = Arc::clone(&current.row);
        let mut prepared_bytes = visual_row_weight(&current_row);
        if prepared_bytes > byte_limit {
            return Err(LayoutError::StreamingWindowByteLimit {
                required: prepared_bytes,
                limit: byte_limit,
            }
            .into());
        }
        prepared_bytes = self.forward.iter().fold(prepared_bytes, |total, entry| {
            total.saturating_add(visual_row_weight(&entry.row))
        });
        let prepared_limit = byte_limit.saturating_sub(MAX_STREAMING_VISUAL_ROW_BYTES);
        let mut byte_limited = prepared_bytes > prepared_limit;

        while row_limit > 0 && self.forward.len() < row_limit && !self.stream_ended {
            if let Some(blocked) = self.blocked.take() {
                let weight = visual_row_weight(&blocked.row);
                if prepared_bytes.saturating_add(weight) > prepared_limit {
                    self.blocked = Some(blocked);
                    byte_limited = true;
                    break;
                }
                prepared_bytes = prepared_bytes.saturating_add(weight);
                self.push_forward(blocked)?;
                continue;
            }
            if prepared_bytes.saturating_add(MAX_STREAMING_VISUAL_ROW_BYTES) > byte_limit {
                byte_limited = true;
                break;
            }
            let Some(next) = self.read_stream_row()? else {
                break;
            };
            let weight = visual_row_weight(&next.row);
            if prepared_bytes.saturating_add(weight) > prepared_limit {
                self.blocked = Some(next);
                byte_limited = true;
                break;
            }
            prepared_bytes = prepared_bytes.saturating_add(weight);
            self.push_forward(next)?;
        }

        if row_limit > 0 {
            rows.try_reserve_exact(1)
                .map_err(|_| LayoutError::StreamingRowAllocation {
                    requested: std::mem::size_of::<Arc<VisualRow>>(),
                })?;
            rows.push(current_row);
            for entry in self.forward.iter().take(row_limit.saturating_sub(1)) {
                rows.try_reserve_exact(1)
                    .map_err(|_| LayoutError::StreamingRowAllocation {
                        requested: rows
                            .len()
                            .saturating_add(1)
                            .saturating_mul(std::mem::size_of::<Arc<VisualRow>>()),
                    })?;
                rows.push(Arc::clone(&entry.row));
            }
        }
        let retained_bytes = prepared_bytes.saturating_add(
            self.blocked
                .as_ref()
                .map_or(0, |entry| visual_row_weight(&entry.row)),
        );
        let reached_line_end = row_limit > 0
            && self.stream_ended
            && self.blocked.is_none()
            && self.forward.len() < row_limit;
        Ok(LargeLineRowWindow {
            top: self.reported_anchor,
            rows,
            reached_line_end,
            byte_limited,
            retained_bytes,
            stats: self.stats(),
        })
    }

    /// Navigator生成後に行った全passのraw read合計と最大同時bufferを返す。
    #[must_use]
    pub fn stats(&self) -> LargeLineStats {
        let current = self.stream.stats();
        LargeLineStats {
            raw_bytes_read: self
                .accumulated_stats
                .raw_bytes_read
                .saturating_add(current.raw_bytes_read),
            peak_buffered_bytes: self
                .accumulated_stats
                .peak_buffered_bytes
                .max(current.peak_buffered_bytes),
        }
    }

    fn take_forward_row(&mut self) -> Result<Option<NavigatorRow>, DocumentLayoutError> {
        if let Some(row) = self.forward.pop_front() {
            return Ok(Some(row));
        }
        if let Some(row) = self.blocked.take() {
            return Ok(Some(row));
        }
        self.read_stream_row()
    }

    fn push_forward(&mut self, row: NavigatorRow) -> Result<(), DocumentLayoutError> {
        self.forward
            .try_reserve_exact(1)
            .map_err(|_| LayoutError::StreamingRowAllocation {
                requested: self
                    .forward
                    .len()
                    .saturating_add(1)
                    .saturating_mul(std::mem::size_of::<NavigatorRow>()),
            })?;
        self.forward.push_back(row);
        Ok(())
    }

    fn read_stream_row(&mut self) -> Result<Option<NavigatorRow>, DocumentLayoutError> {
        let Some(row) = self.stream.next_row()? else {
            self.stream_ended = true;
            return Ok(None);
        };
        let byte_offset = row.content_range.start;
        let fragment = self.last_scanned.as_ref().map_or(0, |(range, anchor)| {
            if range.contains(&byte_offset) {
                if anchor.byte_offset == byte_offset {
                    anchor.fragment.saturating_add(1)
                } else {
                    // Tabが前rowの途中で始まった場合、その前rowはcanonical anchorが別byteでも
                    // 同じtab byteを描くfragment 0である。現在rowはfragment 1から始める。
                    1
                }
            } else {
                0
            }
        });
        let anchor = LargeRowAnchor {
            byte_offset,
            fragment,
        };
        let index = self.next_index;
        self.next_index = self.next_index.saturating_add(1);
        self.last_scanned = Some((row.content_range.clone(), anchor));
        Ok(Some(NavigatorRow {
            index,
            anchor,
            row: Arc::new(row),
        }))
    }

    fn position_at_index(&mut self, target: usize) -> Result<(), DocumentLayoutError> {
        self.reset_stream()?;
        loop {
            let Some(entry) = self.read_stream_row()? else {
                return Err(DocumentLayoutError::MissingVisualRows {
                    line_id: self.line_id,
                });
            };
            if entry.index == target {
                let anchor = entry.anchor;
                self.set_current(entry, anchor);
                return Ok(());
            }
        }
    }

    fn set_current(&mut self, entry: NavigatorRow, reported: LargeRowAnchor) {
        self.current = Some(entry);
        self.reported_anchor = reported;
    }

    fn reset_stream(&mut self) -> Result<(), DocumentLayoutError> {
        let completed = self.stream.stats();
        self.accumulated_stats.raw_bytes_read = self
            .accumulated_stats
            .raw_bytes_read
            .saturating_add(completed.raw_bytes_read);
        self.accumulated_stats.peak_buffered_bytes = self
            .accumulated_stats
            .peak_buffered_bytes
            .max(completed.peak_buffered_bytes);
        self.stream = LargeLineRows::new(
            &self.document,
            self.line_id,
            self.width,
            self.tab_width,
            self.gutter,
        )?;
        self.current = None;
        self.forward.clear();
        self.blocked = None;
        self.last_scanned = None;
        self.next_index = 0;
        self.stream_ended = false;
        Ok(())
    }
}

/// 指定anchorからbounded shared windowを1回だけ収集する。
pub fn collect_large_line_row_window(
    document: &DocumentSnapshot,
    line_id: LineId,
    width: usize,
    tab_width: usize,
    gutter: GutterSpec,
    anchor: LargeRowAnchor,
    max_rows: usize,
) -> Result<LargeLineRowWindow, DocumentLayoutError> {
    validate_tab_width(tab_width)?;
    if max_rows == 0 {
        return Ok(LargeLineRowWindow {
            top: anchor,
            rows: Vec::new(),
            reached_line_end: false,
            byte_limited: false,
            retained_bytes: 0,
            stats: LargeLineStats::default(),
        });
    }
    let mut navigator =
        LargeLineNavigator::new(document, line_id, width, tab_width, gutter, anchor)?;
    navigator.collect_window(max_rows)
}

fn count_large_line_rows_up_to(
    document: &DocumentSnapshot,
    line_id: LineId,
    width: usize,
    tab_width: usize,
    gutter: GutterSpec,
    limit: usize,
) -> Result<(usize, LargeLineStats), DocumentLayoutError> {
    if limit == 0 {
        return Ok((0, LargeLineStats::default()));
    }
    let mut stream = LargeLineRows::new(document, line_id, width, tab_width, gutter)?;
    let mut count = 0_usize;
    while stream.next_row()?.is_some() {
        count = count.saturating_add(1).min(limit);
        if count == limit {
            break;
        }
    }
    Ok((count, stream.stats()))
}

/// 1巨大行を1 visual rowずつ構築するstate。
#[derive(Debug)]
struct LargeLineRows {
    reader: LargeLineTextReader,
    metadata: DocumentLineMetadata,
    content_width: usize,
    tab_width: usize,
    gutter: GutterSpec,
    source: bool,
    row_text: String,
    row_mappings: Vec<SpanMapping>,
    row_width: usize,
    row_started: bool,
    rows_emitted: usize,
    rendered_content_start: usize,
    queued_text: String,
    queued_range: Option<Range<usize>>,
    queued_width: usize,
    tab_remaining: usize,
    tab_range: Range<usize>,
    content_column: usize,
    input_finished: bool,
}

impl LargeLineRows {
    fn new(
        document: &DocumentSnapshot,
        line_id: LineId,
        width: usize,
        tab_width: usize,
        gutter: GutterSpec,
    ) -> Result<Self, DocumentLayoutError> {
        validate_tab_width(tab_width)?;
        let mut reader = LargeLineTextReader::new();
        let mut metadata_reader = DocumentLineMetadataReader::new();
        let metadata = metadata_reader
            .read(document, line_id)?
            .ok_or(DocumentLayoutError::MissingLine { line_id })?;
        let source = is_source_line(metadata.kind);
        if source {
            validate_gutter_metadata(metadata, gutter)?;
        }
        let content_width = content_width_for(metadata.kind, width, gutter)?;
        let started_metadata = reader.start(document, line_id, usize::from(source))?;
        debug_assert_eq!(metadata, started_metadata);
        Ok(Self {
            reader,
            metadata,
            content_width,
            tab_width,
            gutter,
            source,
            row_text: String::new(),
            row_mappings: Vec::new(),
            row_width: 0,
            row_started: false,
            rows_emitted: 0,
            rendered_content_start: 0,
            queued_text: String::new(),
            queued_range: None,
            queued_width: 0,
            tab_remaining: 0,
            tab_range: 0..0,
            content_column: 0,
            input_finished: false,
        })
    }

    fn next_row(&mut self) -> Result<Option<VisualRow>, DocumentLayoutError> {
        loop {
            if self.tab_remaining > 0 {
                self.ensure_row_started();
                self.append_piece(" ", self.tab_range.clone(), 1, true)?;
                self.tab_remaining -= 1;
                if self.row_width >= self.content_width {
                    return self.finish_row().map(Some);
                }
                continue;
            }

            if self.queued_range.is_some() {
                if self.row_started
                    && self.row_width > 0
                    && self.row_width.saturating_add(self.queued_width) > self.content_width
                {
                    return self.finish_row().map(Some);
                }
                self.ensure_row_started();
                let range = self.queued_range.take().unwrap_or(0..0);
                let text = std::mem::take(&mut self.queued_text);
                self.append_piece(&text, range, self.queued_width, false)?;
                self.queued_text = text;
                self.queued_text.clear();
                self.queued_width = 0;
                if self.row_width >= self.content_width {
                    return self.finish_row().map(Some);
                }
                continue;
            }

            if self.input_finished {
                if self.row_started || self.rows_emitted == 0 {
                    self.ensure_row_started();
                    return self.finish_row().map(Some);
                }
                return Ok(None);
            }

            let Some(unit) = self.reader.next_unit()? else {
                self.input_finished = true;
                continue;
            };
            if unit.text == "\t" {
                self.tab_range = unit.logical_range;
                self.tab_remaining = self.tab_width - (self.content_column % self.tab_width);
                self.content_column = self.content_column.saturating_add(self.tab_remaining);
                continue;
            }
            self.queued_text.clear();
            self.queued_text.try_reserve(unit.text.len()).map_err(|_| {
                LayoutError::StreamingRowAllocation {
                    requested: unit.text.len(),
                }
            })?;
            self.queued_text.push_str(unit.text);
            self.queued_range = Some(unit.logical_range);
            self.queued_width = UnicodeWidthStr::width(self.queued_text.as_str());
            self.content_column = self.content_column.saturating_add(self.queued_width);
        }
    }

    fn ensure_row_started(&mut self) {
        if self.row_started {
            return;
        }
        self.row_text.clear();
        self.row_mappings.clear();
        self.row_width = 0;
        let continuation = self.rows_emitted > 0;
        if self.source {
            let old_number = if continuation {
                String::new()
            } else {
                self.metadata
                    .old_line_number
                    .map(|value| value.to_string())
                    .unwrap_or_default()
            };
            let new_number = if continuation {
                String::new()
            } else {
                self.metadata
                    .new_line_number
                    .map(|value| value.to_string())
                    .unwrap_or_default()
            };
            let marker = if continuation {
                ' '
            } else {
                match self.metadata.kind {
                    LineKind::Addition => '+',
                    LineKind::Deletion => '-',
                    _ => ' ',
                }
            };
            self.row_text = format!(
                "{old_number:>old_width$} {new_number:>new_width$} │{marker}",
                old_width = self.gutter.old_width(),
                new_width = self.gutter.new_width(),
            );
            self.rendered_content_start = self.row_text.len();
            if !continuation {
                self.row_mappings.push(SpanMapping {
                    logical: 0..1,
                    rendered: self.rendered_content_start.saturating_sub(1)
                        ..self.rendered_content_start,
                    tab: false,
                });
            }
        } else {
            self.rendered_content_start = 0;
        }
        self.row_started = true;
    }

    fn append_piece(
        &mut self,
        text: &str,
        logical: Range<usize>,
        width: usize,
        tab: bool,
    ) -> Result<(), LayoutError> {
        let next_len = self.row_text.len().saturating_add(text.len());
        let merges_previous = self.row_mappings.last().is_some_and(|previous| {
            previous.logical == logical
                && previous.rendered.end == self.row_text.len()
                && previous.tab == tab
        });
        let mapping_count = self
            .row_mappings
            .len()
            .saturating_add(usize::from(!merges_previous));
        let prospective_weight = std::mem::size_of::<VisualRow>()
            .saturating_add(2 * std::mem::size_of::<usize>())
            .saturating_add(std::mem::size_of::<NavigatorRow>())
            .saturating_add(std::mem::size_of::<Arc<VisualRow>>())
            .saturating_add(next_len)
            .saturating_add(mapping_count.saturating_mul(std::mem::size_of::<SpanMapping>()));
        if prospective_weight > MAX_STREAMING_VISUAL_ROW_BYTES {
            return Err(LayoutError::PathologicalRowBytes {
                limit: MAX_STREAMING_VISUAL_ROW_BYTES,
            });
        }
        self.row_text.try_reserve_exact(text.len()).map_err(|_| {
            LayoutError::StreamingRowAllocation {
                requested: next_len,
            }
        })?;
        let rendered_start = self.row_text.len();
        self.row_text.push_str(text);
        let rendered = rendered_start..self.row_text.len();
        if merges_previous {
            let previous =
                self.row_mappings
                    .last_mut()
                    .ok_or(LayoutError::StreamingRowAllocation {
                        requested: prospective_weight,
                    })?;
            previous.rendered.end = rendered.end;
        } else {
            self.row_mappings.try_reserve_exact(1).map_err(|_| {
                LayoutError::StreamingRowAllocation {
                    requested: self
                        .row_mappings
                        .len()
                        .saturating_add(1)
                        .saturating_mul(std::mem::size_of::<SpanMapping>()),
                }
            })?;
            self.row_mappings.push(SpanMapping {
                logical,
                rendered,
                tab,
            });
        }
        self.row_width = self.row_width.saturating_add(width);
        Ok(())
    }

    fn finish_row(&mut self) -> Result<VisualRow, DocumentLayoutError> {
        let retained_weight = std::mem::size_of::<VisualRow>()
            .saturating_add(2 * std::mem::size_of::<usize>())
            .saturating_add(std::mem::size_of::<NavigatorRow>())
            .saturating_add(std::mem::size_of::<Arc<VisualRow>>())
            .saturating_add(self.row_text.capacity())
            .saturating_add(
                self.row_mappings
                    .capacity()
                    .saturating_mul(std::mem::size_of::<SpanMapping>()),
            );
        if retained_weight > MAX_STREAMING_VISUAL_ROW_BYTES {
            return Err(LayoutError::PathologicalRowBytes {
                limit: MAX_STREAMING_VISUAL_ROW_BYTES,
            }
            .into());
        }
        let continuation = self.rows_emitted > 0;
        let content_range = mapping_bounds(&self.row_mappings).unwrap_or_else(|| {
            let offset = usize::from(self.source);
            offset..offset
        });
        let text = std::mem::take(&mut self.row_text);
        let rendered_content_range = self.rendered_content_start..text.len();
        let row = VisualRow {
            line_id: self.metadata.id,
            commit_id: self.metadata.commit_id,
            file_id: self.metadata.file_id,
            hunk_id: self.metadata.hunk_id,
            text,
            role: role_for_line(self.metadata.kind),
            content_range,
            rendered_content_range,
            continuation,
            span_map: std::mem::take(&mut self.row_mappings),
        };
        self.row_started = false;
        self.row_width = 0;
        self.rows_emitted = self.rows_emitted.saturating_add(1);
        Ok(row)
    }

    fn stats(&self) -> LargeLineStats {
        self.reader.stats()
    }
}

fn validate_tab_width(tab_width: usize) -> Result<(), LayoutError> {
    if tab_width == 0 {
        return Err(LayoutError::InvalidTabWidth { actual: tab_width });
    }
    Ok(())
}

fn content_width_for(
    kind: LineKind,
    width: usize,
    gutter: GutterSpec,
) -> Result<usize, LayoutError> {
    if is_source_line(kind) {
        let prefix_width = gutter.old_width() + 1 + gutter.new_width() + 3;
        width
            .checked_sub(prefix_width)
            .filter(|value| *value >= 1)
            .ok_or(LayoutError::TooNarrow {
                actual: width,
                required: prefix_width + 1,
            })
    } else if width == 0 {
        Err(LayoutError::TooNarrow {
            actual: 0,
            required: 1,
        })
    } else {
        Ok(width)
    }
}

fn estimated_eager_rows(content_len: usize, content_width: usize, tab_width: usize) -> usize {
    if content_len == 0 {
        return 1;
    }
    let maximum_cells = content_len.saturating_mul(tab_width);
    maximum_cells.div_ceil(content_width).max(1)
}

fn estimated_eager_layout_bytes(
    content_len: usize,
    row_count: usize,
    kind: LineKind,
    gutter: GutterSpec,
    tab_width: usize,
) -> usize {
    let expanded_bytes = content_len.saturating_mul(tab_width);
    estimated_eager_layout_bytes_from_expanded(expanded_bytes, row_count, kind, gutter)
}

fn estimated_eager_layout_bytes_from_expanded(
    expanded_bytes: usize,
    row_count: usize,
    kind: LineKind,
    gutter: GutterSpec,
) -> usize {
    // Tabがrow境界で分かれると同じ1 byteが最大tab_width件のmappingになる。
    // ExpandedTextと完成VisualRowがloop中に同時生存する。それぞれのVecが
    // grow余白を持つため、展開後byte数をmapping件数とした2世代×2倍を足す。
    let mapping_count = expanded_bytes
        .saturating_add(row_count)
        .saturating_add(usize::from(is_source_line(kind)));
    let mapping_bytes = mapping_count
        .saturating_mul(std::mem::size_of::<SpanMapping>())
        .saturating_mul(4);
    // `Vec::new()`からの初回pushはVisualRowサイズで4件分を確保する。その後の倍増も
    // 含め、完成時capacityの上限を`max(4, 2 * row_count)`に置く。
    let row_capacity = row_count.saturating_mul(2).max(4);
    let row_metadata = row_capacity
        .saturating_mul(std::mem::size_of::<VisualRow>())
        .saturating_add(
            row_capacity.saturating_mul(std::mem::size_of::<crate::text::WrappedSegment>()),
        );
    let gutter_bytes = if is_source_line(kind) {
        gutter.old_width() + gutter.new_width() + 6
    } else {
        0
    };
    let rendered_bytes = expanded_bytes.saturating_add(row_count.saturating_mul(gutter_bytes));
    std::mem::size_of::<LineLayout>()
        .saturating_add(2 * std::mem::size_of::<usize>())
        .saturating_add(std::mem::size_of::<Vec<VisualRow>>())
        .saturating_add(expanded_bytes.saturating_mul(2))
        .saturating_add(rendered_bytes.saturating_mul(2))
        .saturating_add(mapping_bytes)
        .saturating_add(row_metadata)
}

fn validate_gutter(line: &DocumentLineRef<'_>, gutter: GutterSpec) -> Result<(), LayoutError> {
    let required_old_width = line
        .old_line_number
        .map_or(0, |number| usize::from(decimal_width(number)));
    let required_new_width = line
        .new_line_number
        .map_or(0, |number| usize::from(decimal_width(number)));
    let old_width = gutter.old_width();
    let new_width = gutter.new_width();
    if required_old_width > old_width || required_new_width > new_width {
        return Err(LayoutError::UndersizedGutter {
            old_width,
            required_old_width,
            new_width,
            required_new_width,
        });
    }
    Ok(())
}

fn validate_gutter_metadata(
    line: DocumentLineMetadata,
    gutter: GutterSpec,
) -> Result<(), LayoutError> {
    let required_old_width = line
        .old_line_number
        .map_or(0, |number| usize::from(decimal_width(number)));
    let required_new_width = line
        .new_line_number
        .map_or(0, |number| usize::from(decimal_width(number)));
    if required_old_width > gutter.old_width() || required_new_width > gutter.new_width() {
        return Err(LayoutError::UndersizedGutter {
            old_width: gutter.old_width(),
            required_old_width,
            new_width: gutter.new_width(),
            required_new_width,
        });
    }
    Ok(())
}

fn append_source_rows(
    rows: &mut Vec<VisualRow>,
    line: &DocumentLineRef<'_>,
    width: usize,
    tab_width: usize,
    old_width: usize,
    new_width: usize,
) -> Result<(), LayoutError> {
    // "old new │+ " の separator/markerを含む。source content自身に最低1cellを残す。
    let prefix_width = old_width + 1 + new_width + 3;
    let content_width = width
        .checked_sub(prefix_width)
        .filter(|value| *value >= 1)
        .ok_or(LayoutError::TooNarrow {
            actual: width,
            required: prefix_width + 1,
        })?;
    let expanded = expand_tabs_with_mapping(line.content(), tab_width);
    let segments = wrap_hard(&expanded.text, content_width);
    let marker = match line.kind {
        LineKind::Addition => '+',
        LineKind::Deletion => '-',
        _ => ' ',
    };

    for (index, segment) in segments.into_iter().enumerate() {
        let continuation = index > 0;
        let old_number = if continuation {
            String::new()
        } else {
            line.old_line_number
                .map(|value| value.to_string())
                .unwrap_or_default()
        };
        let new_number = if continuation {
            String::new()
        } else {
            line.new_line_number
                .map(|value| value.to_string())
                .unwrap_or_default()
        };
        let row_marker = if continuation { ' ' } else { marker };
        let source = &expanded.text[segment.byte_range.clone()];
        let text =
            format!("{old_number:>old_width$} {new_number:>new_width$} │{row_marker}{source}");
        let rendered_content_start = text.len() - source.len();
        let mut span_map = map_expanded_segment(
            &expanded,
            segment.byte_range.clone(),
            1,
            rendered_content_start,
        );
        if !continuation {
            span_map.insert(
                0,
                SpanMapping {
                    logical: 0..1,
                    rendered: rendered_content_start.saturating_sub(1)..rendered_content_start,
                    tab: false,
                },
            );
        }
        let content_range = mapping_bounds(&span_map).unwrap_or(1..1);
        let rendered_content_end = text.len();
        rows.push(VisualRow {
            line_id: line.id,
            commit_id: line.commit_id,
            file_id: line.file_id,
            hunk_id: line.hunk_id,
            text,
            role: role_for_line(line.kind),
            content_range,
            rendered_content_range: rendered_content_start..rendered_content_end,
            continuation,
            span_map,
        });
    }
    Ok(())
}

fn append_metadata_rows(
    rows: &mut Vec<VisualRow>,
    line: &DocumentLineRef<'_>,
    width: usize,
    tab_width: usize,
) -> Result<(), LayoutError> {
    if width == 0 {
        return Err(LayoutError::TooNarrow {
            actual: 0,
            required: 1,
        });
    }
    let expanded = expand_tabs_with_mapping(line.text, tab_width);
    for (index, segment) in wrap_hard(&expanded.text, width).into_iter().enumerate() {
        let text = expanded.text[segment.byte_range.clone()].to_owned();
        let span_map = map_expanded_segment(&expanded, segment.byte_range, 0, 0);
        let content_range = mapping_bounds(&span_map).unwrap_or(0..0);
        rows.push(VisualRow {
            line_id: line.id,
            commit_id: line.commit_id,
            file_id: line.file_id,
            hunk_id: line.hunk_id,
            rendered_content_range: 0..text.len(),
            text,
            role: role_for_line(line.kind),
            content_range,
            continuation: index > 0,
            span_map,
        });
    }
    Ok(())
}

#[derive(Debug)]
struct ExpandedText {
    text: String,
    mappings: Vec<SpanMapping>,
}

fn expand_tabs_with_mapping(input: &str, tab_width: usize) -> ExpandedText {
    let mut text = String::with_capacity(input.len());
    let mut mappings = Vec::new();
    let mut column = 0_usize;
    let mut plain_start = 0_usize;

    for (logical_start, grapheme) in input.grapheme_indices(true) {
        if grapheme != "\t" {
            text.push_str(grapheme);
            column = column.saturating_add(UnicodeWidthStr::width(grapheme));
            continue;
        }

        if plain_start < logical_start {
            let rendered_end = text.len();
            let byte_count = logical_start - plain_start;
            mappings.push(SpanMapping {
                logical: plain_start..logical_start,
                rendered: rendered_end - byte_count..rendered_end,
                tab: false,
            });
        }
        let rendered_start = text.len();
        let spaces = tab_width - (column % tab_width);
        text.extend(std::iter::repeat_n(' ', spaces));
        column += spaces;
        mappings.push(SpanMapping {
            logical: logical_start..logical_start + grapheme.len(),
            rendered: rendered_start..text.len(),
            tab: true,
        });
        plain_start = logical_start + grapheme.len();
    }

    if plain_start < input.len() {
        let byte_count = input.len() - plain_start;
        mappings.push(SpanMapping {
            logical: plain_start..input.len(),
            rendered: text.len() - byte_count..text.len(),
            tab: false,
        });
    }
    if input.is_empty() {
        mappings.push(SpanMapping {
            logical: 0..0,
            rendered: 0..0,
            tab: false,
        });
    }
    ExpandedText { text, mappings }
}

fn map_expanded_segment(
    expanded: &ExpandedText,
    segment: Range<usize>,
    logical_offset: usize,
    rendered_offset: usize,
) -> Vec<SpanMapping> {
    expanded
        .mappings
        .iter()
        .filter_map(|mapping| {
            let start = mapping.rendered.start.max(segment.start);
            let end = mapping.rendered.end.min(segment.end);
            if start >= end && !(mapping.rendered.is_empty() && segment.is_empty()) {
                return None;
            }

            let logical = if mapping.logical.end - mapping.logical.start
                == mapping.rendered.end - mapping.rendered.start
            {
                let relative_start = start.saturating_sub(mapping.rendered.start);
                let relative_end = end.saturating_sub(mapping.rendered.start);
                mapping.logical.start + logical_offset + relative_start
                    ..mapping.logical.start + logical_offset + relative_end
            } else {
                mapping.logical.start + logical_offset..mapping.logical.end + logical_offset
            };
            Some(SpanMapping {
                logical,
                rendered: rendered_offset + start.saturating_sub(segment.start)
                    ..rendered_offset + end.saturating_sub(segment.start),
                tab: mapping.tab,
            })
        })
        .collect()
}

fn mapping_bounds(mappings: &[SpanMapping]) -> Option<Range<usize>> {
    Some(mappings.first()?.logical.start..mappings.last()?.logical.end)
}

fn gutter_spec_for_line(line: &DocumentLineRef<'_>) -> GutterSpec {
    line.file_line_bounds
        .map(|bounds| {
            GutterSpec::new(
                bounds.max_old_line_number.map_or(0, decimal_width),
                bounds.max_new_line_number.map_or(0, decimal_width),
            )
        })
        .unwrap_or_default()
}

fn gutter_spec_for_metadata(line: DocumentLineMetadata) -> GutterSpec {
    line.file_line_bounds
        .map(|bounds| {
            GutterSpec::new(
                bounds.max_old_line_number.map_or(0, decimal_width),
                bounds.max_new_line_number.map_or(0, decimal_width),
            )
        })
        .unwrap_or_default()
}

fn decimal_width(number: u64) -> u8 {
    number.checked_ilog10().map_or(1, |log| log as u8 + 1)
}

fn is_source_line(kind: LineKind) -> bool {
    matches!(
        kind,
        LineKind::Context | LineKind::Deletion | LineKind::Addition
    )
}

fn role_for_line(kind: LineKind) -> SemanticRole {
    match kind {
        LineKind::CommitHeader => SemanticRole::CommitHeader,
        LineKind::DiffHeader | LineKind::FileMetadata => SemanticRole::FileHeader,
        LineKind::HunkHeader => SemanticRole::HunkHeader,
        LineKind::Addition => SemanticRole::Added,
        LineKind::Deletion => SemanticRole::Removed,
        LineKind::Context => SemanticRole::Context,
        LineKind::Fallback => SemanticRole::Plain,
        LineKind::Binary | LineKind::Submodule => SemanticRole::Dim,
        _ => SemanticRole::Plain,
    }
}

/// 選択済みvisual rowだけをTTYへ順に書く。Streaming direct modeはclosed fileまでのrowを渡し、
/// open fileのgutter/countが確定する前に出力しない。
pub fn write_direct_rows<'row, W, I>(writer: &mut W, rows: I, theme: &Theme) -> io::Result<()>
where
    W: Write,
    I: IntoIterator<Item = &'row VisualRow>,
{
    for row in rows {
        write_style(writer, theme.style(row.role))?;
        writer.write_all(row.text.as_bytes())?;
        writer.write_all(b"\x1b[0m\n")?;
    }
    writer.flush()
}

/// Syntax・intralineを合成し、指定logical-line範囲だけを逐次layoutしてTTYへ書く。
///
/// Precedenceはinteractive rendererと同じく、diff構造、syntax foreground、intraline backgroundの
/// 順とする。Searchはinteractive stateだけのためdirect outputへは含めない。1つのreaderと1 logical
/// line分の[`LineLayout`]だけを保持し、範囲より前のtextやvisual rowを再構築しない。Layoutとsnapshotを
/// 別引数にしないため、同じstable IDを持つ別documentのrowを混在させる経路も作らない。Decorationは
/// exact snapshotへ結び付け、同じbuilderの後続snapshotでfallback化したsource metadataにも流用しない。
pub fn write_direct_decorated_range<W: Write>(
    writer: &mut W,
    document: &DocumentSnapshot,
    decorations: &DecorationSet,
    theme: &Theme,
    line_range: Range<usize>,
    width: usize,
    tab_width: usize,
) -> Result<(), DirectWriteError> {
    validate_tab_width(tab_width)?;
    if !decorations.belongs_to(document) {
        return Err(DirectWriteError::DecorationSnapshotMismatch);
    }
    let range_end = line_range.end.min(document.line_count());
    if line_range.start >= range_end {
        return writer.flush().map_err(DirectWriteError::Io);
    }
    let mut reader = DocumentLineReader::new();
    let mut metadata_reader = DocumentLineMetadataReader::new();
    for index in line_range.start..range_end {
        let line_id = document
            .line_id_at(index)
            .ok_or(DirectWriteError::UnavailableLineIndex { index })?;
        let metadata = metadata_reader
            .read(document, line_id)?
            .ok_or(DirectWriteError::MissingLine { line_id })?;
        let gutter = gutter_spec_for_metadata(metadata);
        if line_requires_streaming_layout(metadata, width, tab_width, gutter)? {
            let mut stream = LargeLineRows::new(document, line_id, width, tab_width, gutter)?;
            let mut previous = None;
            while let Some(row) = stream.next_row()? {
                write_streaming_decorated_row(
                    writer,
                    previous.as_ref(),
                    &row,
                    metadata.kind,
                    decorations,
                    theme,
                )?;
                previous = Some(row);
            }
            continue;
        }
        let line = reader
            .read(document, line_id)?
            .ok_or(DirectWriteError::MissingLine { line_id })?;
        let gutter = gutter_spec_for_line(&line);
        let line_layout = layout_line(&line, width, tab_width, gutter)?;
        for (row_index, row) in line_layout.rows().iter().enumerate() {
            write_decorated_row(
                writer,
                line_layout.rows(),
                &line,
                decorations,
                theme,
                row_index,
                row,
            )?;
        }
    }
    writer.flush().map_err(DirectWriteError::Io)
}

fn write_large_undecorated_row<W: Write>(
    writer: &mut W,
    row: &VisualRow,
    kind: LineKind,
    theme: &Theme,
) -> io::Result<()> {
    let gutter_end = if is_source_line(kind) {
        row.rendered_content_range.start.saturating_sub(1)
    } else {
        0
    };
    if gutter_end > 0 {
        write_style(writer, theme.style(SemanticRole::LineNumber))?;
        writer.write_all(&row.text.as_bytes()[..gutter_end])?;
    }
    write_style(writer, theme.style(row.role))?;
    writer.write_all(&row.text.as_bytes()[gutter_end..])?;
    writer.write_all(b"\x1b[0m\n")
}

fn write_decorated_row<W: Write>(
    writer: &mut W,
    line_rows: &[VisualRow],
    line: &DocumentLineRef<'_>,
    decorations: &DecorationSet,
    theme: &Theme,
    row_index: usize,
    row: &VisualRow,
) -> io::Result<()> {
    let decoration = decorations.line(row.line_id);
    let syntax = direct_syntax_ranges(row, decoration);
    let intraline = direct_intraline_ranges(row, line.kind, decoration);
    let replacements =
        direct_whitespace_replacements(line_rows, row, row_index, line, decoration, decorations);
    write_composed_row(
        writer,
        row,
        line.kind,
        theme,
        &syntax,
        &intraline,
        &replacements,
    )
}

fn write_streaming_decorated_row<W: Write>(
    writer: &mut W,
    previous: Option<&VisualRow>,
    row: &VisualRow,
    kind: LineKind,
    decorations: &DecorationSet,
    theme: &Theme,
) -> io::Result<()> {
    let decoration = decorations.line(row.line_id);
    let syntax = direct_syntax_ranges(row, decoration);
    let intraline = direct_intraline_ranges(row, kind, decoration);
    let replacements = direct_streaming_whitespace_replacements(
        previous,
        row,
        decoration,
        decorations.whitespace(),
    );
    write_composed_row(writer, row, kind, theme, &syntax, &intraline, &replacements)
}

fn write_composed_row<W: Write>(
    writer: &mut W,
    row: &VisualRow,
    kind: LineKind,
    theme: &Theme,
    syntax: &[(Range<usize>, &crate::highlight::SyntaxSpan)],
    intraline: &[Range<usize>],
    replacements: &HashMap<usize, char>,
) -> io::Result<()> {
    let gutter_end = if is_source_line(kind) {
        row.rendered_content_range.start.saturating_sub(1)
    } else {
        0
    };
    let mut boundaries = vec![0, row.text.len(), gutter_end];
    for (range, _) in syntax {
        boundaries.extend([range.start, range.end]);
    }
    for range in intraline {
        boundaries.extend([range.start, range.end]);
    }
    boundaries.retain(|offset| *offset <= row.text.len() && row.text.is_char_boundary(*offset));
    boundaries.sort_unstable();
    boundaries.dedup();

    for pair in boundaries.windows(2) {
        let range = pair[0]..pair[1];
        if range.is_empty() {
            continue;
        }
        let base_role = if range.start < gutter_end {
            SemanticRole::LineNumber
        } else {
            row.role
        };
        let mut style = theme.style(base_role);
        if let Some((_, span)) = syntax
            .iter()
            .find(|(candidate, _)| candidate.start <= range.start && range.end <= candidate.end)
        {
            style = style.patch(theme.syntax_style(
                span.foreground,
                span.bold,
                span.italic,
                span.underline,
            ));
        }
        if intraline
            .iter()
            .any(|candidate| candidate.start <= range.start && range.end <= candidate.end)
        {
            let role = match kind {
                LineKind::Deletion => SemanticRole::IntralineRemoved,
                LineKind::Addition => SemanticRole::IntralineAdded,
                _ => row.role,
            };
            style = style.patch(theme.style(role));
        }
        write_style(writer, style)?;
        let rendered = replace_direct_whitespace(&row.text, range, replacements);
        writer.write_all(rendered.as_bytes())?;
    }
    writer.write_all(b"\x1b[0m\n")
}

fn direct_syntax_ranges<'decoration>(
    row: &VisualRow,
    decoration: Option<&'decoration LineDecoration>,
) -> Vec<(Range<usize>, &'decoration crate::highlight::SyntaxSpan)> {
    let spans = decoration.map_or(&[][..], LineDecoration::syntax_spans);
    let first = spans.partition_point(|span| span.byte_range.end <= row.content_range.start);
    let end = spans.partition_point(|span| span.byte_range.start < row.content_range.end);
    spans[first..end]
        .iter()
        .flat_map(|span| {
            row.rendered_ranges_for_logical(span.byte_range.clone())
                .into_iter()
                .map(move |range| (range, span))
        })
        .collect()
}

fn direct_intraline_ranges(
    row: &VisualRow,
    kind: LineKind,
    decoration: Option<&LineDecoration>,
) -> Vec<Range<usize>> {
    if !matches!(kind, LineKind::Deletion | LineKind::Addition) {
        return Vec::new();
    }
    let spans = decoration.map_or(&[][..], LineDecoration::intraline_ranges);
    let first = spans.partition_point(|range| range.end <= row.content_range.start);
    let end = spans.partition_point(|range| range.start < row.content_range.end);
    spans[first..end]
        .iter()
        .flat_map(|range| row.rendered_ranges_for_logical(range.clone()))
        .collect()
}

fn direct_whitespace_replacements(
    line_rows: &[VisualRow],
    row: &VisualRow,
    row_index: usize,
    line: &DocumentLineRef<'_>,
    decoration: Option<&LineDecoration>,
    decorations: &DecorationSet,
) -> HashMap<usize, char> {
    let mut replacements = HashMap::new();
    if decorations.whitespace() != crate::cli::WhitespaceChoice::Show {
        return replacements;
    }
    let Some(decoration) = decoration else {
        return replacements;
    };
    let spans = decoration.intraline_ranges();
    let first = spans.partition_point(|range| range.end <= row.content_range.start);
    let end = spans.partition_point(|range| range.start < row.content_range.end);
    for changed in &spans[first..end] {
        let visible =
            changed.start.max(row.content_range.start)..changed.end.min(row.content_range.end);
        let Some(text) = line.text.get(visible.clone()) else {
            continue;
        };
        for (relative, character) in text.char_indices() {
            if !matches!(character, ' ' | '\t') {
                continue;
            }
            let logical = visible.start + relative;
            let logical_range = logical..logical + character.len_utf8();
            let first_row =
                row_index == 0 || !line_rows[row_index - 1].renders_logical_range(&logical_range);
            for mapped in row.rendered_ranges_for_logical(logical_range) {
                for rendered in mapped.clone() {
                    if row.text.as_bytes().get(rendered) != Some(&b' ') {
                        continue;
                    }
                    let glyph = if character == '\t' && first_row && rendered == mapped.start {
                        '→'
                    } else {
                        '·'
                    };
                    replacements.insert(rendered, glyph);
                }
            }
        }
    }
    replacements
}

fn direct_streaming_whitespace_replacements(
    previous: Option<&VisualRow>,
    row: &VisualRow,
    decoration: Option<&LineDecoration>,
    whitespace: crate::cli::WhitespaceChoice,
) -> HashMap<usize, char> {
    let mut replacements = HashMap::new();
    if whitespace != crate::cli::WhitespaceChoice::Show {
        return replacements;
    }
    let Some(decoration) = decoration else {
        return replacements;
    };
    let changed = decoration.intraline_ranges();
    if changed.is_empty() {
        return replacements;
    }

    for mapping in row.span_mappings() {
        if !changed
            .iter()
            .any(|range| range.start < mapping.logical.end && range.end > mapping.logical.start)
        {
            continue;
        }
        if mapping.is_tab() {
            let first_fragment =
                previous.is_none_or(|row| !row.renders_logical_range(&mapping.logical));
            for rendered in mapping.rendered.clone() {
                if row.text.as_bytes().get(rendered) != Some(&b' ') {
                    continue;
                }
                let glyph = if first_fragment && rendered == mapping.rendered.start {
                    '→'
                } else {
                    '·'
                };
                replacements.insert(rendered, glyph);
            }
            continue;
        }

        let Some(text) = row.text.get(mapping.rendered.clone()) else {
            continue;
        };
        for (relative, character) in text.char_indices() {
            if character != ' ' {
                continue;
            }
            let logical = mapping.logical.start.saturating_add(relative);
            let logical_range = logical..logical.saturating_add(1);
            if changed
                .iter()
                .any(|range| range.start < logical_range.end && range.end > logical_range.start)
            {
                replacements.insert(mapping.rendered.start + relative, '·');
            }
        }
    }
    replacements
}

fn replace_direct_whitespace(
    text: &str,
    range: Range<usize>,
    replacements: &HashMap<usize, char>,
) -> String {
    let source = &text[range.clone()];
    if replacements.is_empty() {
        return source.to_owned();
    }
    let mut output = String::with_capacity(source.len());
    for (relative, character) in source.char_indices() {
        output.push(
            replacements
                .get(&(range.start + relative))
                .copied()
                .unwrap_or(character),
        );
    }
    output
}

/// File grammarへ入る前のplain/commit metadata lineを、既出document全体のreflowなしで直接書く。
/// 指定rangeをstable ID順に1行ずつdecodeし、reader bufferと1行分のrow bufferだけを再利用する。
/// Source lineはfile entry単位のgutter確定が必要なので、このAPIへ渡さない。
pub fn write_direct_metadata_lines<W: Write>(
    writer: &mut W,
    document: &DocumentSnapshot,
    line_range: Range<usize>,
    width: usize,
    tab_width: usize,
    theme: &Theme,
) -> Result<(), DirectWriteError> {
    validate_tab_width(tab_width)?;
    let mut reader = DocumentLineReader::new();
    let mut metadata_reader = DocumentLineMetadataReader::new();
    let mut rows = Vec::new();
    for index in line_range.start..line_range.end.min(document.line_count()) {
        let line_id = document
            .line_id_at(index)
            .ok_or(DirectWriteError::UnavailableLineIndex { index })?;
        let metadata = metadata_reader
            .read(document, line_id)?
            .ok_or(DirectWriteError::MissingLine { line_id })?;
        let gutter = gutter_spec_for_metadata(metadata);
        if line_requires_streaming_layout(metadata, width, tab_width, gutter)? {
            let mut stream = LargeLineRows::new(document, line_id, width, tab_width, gutter)?;
            while let Some(row) = stream.next_row()? {
                write_large_undecorated_row(writer, &row, metadata.kind, theme)?;
            }
            continue;
        }
        let line = reader
            .read(document, line_id)?
            .ok_or(DirectWriteError::MissingLine { line_id })?;
        rows.clear();
        append_metadata_rows(&mut rows, &line, width, tab_width)?;
        for row in &rows {
            write_style(writer, theme.style(row.role))?;
            writer.write_all(row.text.as_bytes())?;
            writer.write_all(b"\x1b[0m\n")?;
        }
    }
    writer.flush()?;
    Ok(())
}

/// Streaming direct layoutまたは出力のfailure。
#[derive(Debug, Error)]
pub enum DirectWriteError {
    #[error(transparent)]
    Layout(#[from] LayoutError),
    #[error(transparent)]
    Document(#[from] DocumentError),
    #[error(transparent)]
    Decoration(#[from] DecorationComputeError),
    #[error(transparent)]
    LargeLine(#[from] LargeLineError),
    #[error("decoration set belongs to a different document snapshot")]
    DecorationSnapshotMismatch,
    #[error("document snapshot does not contain logical line {line_id}")]
    MissingLine { line_id: LineId },
    #[error("document snapshot cannot represent logical line index {index} as a stable ID")]
    UnavailableLineIndex { index: usize },
    #[error("logical line {line_id} requires bounded streaming layout")]
    StreamingRequired { line_id: LineId },
    #[error("logical line {line_id} produced no visual rows")]
    MissingVisualRows { line_id: LineId },
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<DocumentLayoutError> for DirectWriteError {
    fn from(error: DocumentLayoutError) -> Self {
        match error {
            DocumentLayoutError::Layout(error) => Self::Layout(error),
            DocumentLayoutError::Document(error) => Self::Document(error),
            DocumentLayoutError::LargeLine(error) => Self::LargeLine(error),
            DocumentLayoutError::MissingLine { line_id } => Self::MissingLine { line_id },
            DocumentLayoutError::UnavailableLineIndex { index } => {
                Self::UnavailableLineIndex { index }
            }
            DocumentLayoutError::StreamingRequired { line_id } => {
                Self::StreamingRequired { line_id }
            }
            DocumentLayoutError::MissingVisualRows { line_id } => {
                Self::MissingVisualRows { line_id }
            }
        }
    }
}

fn write_style<W: Write>(writer: &mut W, style: Style) -> io::Result<()> {
    writer.write_all(b"\x1b[0m")?;
    if style.add_modifier.contains(Modifier::BOLD) {
        writer.write_all(b"\x1b[1m")?;
    }
    if style.add_modifier.contains(Modifier::DIM) {
        writer.write_all(b"\x1b[2m")?;
    }
    if style.add_modifier.contains(Modifier::ITALIC) {
        writer.write_all(b"\x1b[3m")?;
    }
    if style.add_modifier.contains(Modifier::UNDERLINED) {
        writer.write_all(b"\x1b[4m")?;
    }
    if style.add_modifier.contains(Modifier::REVERSED) {
        writer.write_all(b"\x1b[7m")?;
    }
    if let Some(color) = style.fg {
        write_color(writer, color, false)?;
    }
    if let Some(color) = style.bg {
        write_color(writer, color, true)?;
    }
    Ok(())
}

fn write_color<W: Write>(writer: &mut W, color: Color, background: bool) -> io::Result<()> {
    let base = if background { 48 } else { 38 };
    match color {
        Color::Reset => writer.write_all(if background { b"\x1b[49m" } else { b"\x1b[39m" }),
        Color::Indexed(index) => write!(writer, "\x1b[{base};5;{index}m"),
        Color::Rgb(red, green, blue) => {
            write!(writer, "\x1b[{base};2;{red};{green};{blue}m")
        }
        ansi => {
            let code = ansi16_code(ansi, background);
            write!(writer, "\x1b[{code}m")
        }
    }
}

fn ansi16_code(color: Color, background: bool) -> u8 {
    let (index, bright) = match color {
        Color::Black => (0, false),
        Color::Red => (1, false),
        Color::Green => (2, false),
        Color::Yellow => (3, false),
        Color::Blue => (4, false),
        Color::Magenta => (5, false),
        Color::Cyan => (6, false),
        Color::Gray => (7, false),
        Color::DarkGray => (0, true),
        Color::LightRed => (1, true),
        Color::LightGreen => (2, true),
        Color::LightYellow => (3, true),
        Color::LightBlue => (4, true),
        Color::LightMagenta => (5, true),
        Color::LightCyan => (6, true),
        Color::White => (7, true),
        _ => (7, false),
    };
    let normal_base = if background { 40 } else { 30 };
    let bright_base = if background { 100 } else { 90 };
    if bright {
        bright_base + index
    } else {
        normal_base + index
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::WhitespaceChoice;
    use crate::decorations::DecorationSet;
    use crate::document::OwnedDocumentLine;
    use crate::document_store::CHUNKED_LINE_RAW_BYTES;
    use crate::input::AppendStore;
    use crate::parser::{GitParser, parse_bytes as parse_snapshot};
    use crate::storage::StructuralStorage;
    use crate::theme::ColorLevel;

    fn parse_bytes(input: &[u8]) -> DocumentSnapshot {
        parse_snapshot(input).unwrap()
    }

    fn read_lines(document: &DocumentSnapshot) -> Vec<OwnedDocumentLine> {
        let mut reader = DocumentLineReader::new();
        (0..document.line_count())
            .map(|index| {
                let line_id = document.line_id_at(index).unwrap();
                reader.read_owned(document, line_id).unwrap().unwrap()
            })
            .collect()
    }

    fn line_with_kind(document: &DocumentSnapshot, kind: LineKind) -> OwnedDocumentLine {
        read_lines(document)
            .into_iter()
            .find(|line| line.kind == kind)
            .unwrap()
    }

    fn plain_metadata(display_len: u32, raw_len: u64) -> DocumentLineMetadata {
        DocumentLineMetadata {
            id: LineId::new(0),
            kind: LineKind::Plain,
            commit_id: None,
            file_id: None,
            hunk_id: None,
            old_line_number: None,
            new_line_number: None,
            file_line_bounds: None,
            terminated: true,
            raw_len,
            display_len: crate::document::LineDisplayLength::Exact(display_len),
        }
    }

    #[test]
    fn gutter_spec_is_compact_and_keeps_the_minimum_alignment() {
        assert_eq!(std::mem::size_of::<GutterSpec>(), 2);
        assert_eq!(GutterSpec::new(0, 2), GutterSpec::MINIMUM);
        assert_eq!(GutterSpec::new(4, 7).old_width(), 4);
        assert_eq!(GutterSpec::new(4, 7).new_width(), 7);
    }

    #[test]
    fn fallback_rows_use_plain_text_without_brightening_opaque_payloads() {
        assert_eq!(role_for_line(LineKind::Fallback), SemanticRole::Plain);
        assert_eq!(role_for_line(LineKind::Binary), SemanticRole::Dim);
        assert_eq!(role_for_line(LineKind::Submodule), SemanticRole::Dim);
    }

    #[test]
    fn zero_tab_width_is_rejected_for_line_and_bounded_count_paths() {
        let document = parse_bytes(b"metadata without tabs\n");
        let expected = LayoutError::InvalidTabWidth { actual: 0 };
        let line = line_with_kind(&document, LineKind::Plain);
        assert_eq!(
            layout_line(&line.as_borrowed(), 80, 0, GutterSpec::MINIMUM).unwrap_err(),
            expected,
        );
        let empty = parse_bytes(b"");
        assert!(matches!(
            count_rendered_rows_up_to(&empty, 80, 0, 1),
            Err(DocumentLayoutError::Layout(error)) if error == expected
        ));
    }

    #[test]
    fn line_layout_rejects_gutters_that_cannot_hold_current_line_numbers() {
        let document = parse_bytes(
            b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -12345 +54321 @@\n value\n",
        );
        let context = line_with_kind(&document, LineKind::Context);

        assert_eq!(
            layout_line(&context.as_borrowed(), 0, 4, GutterSpec::new(3, 4)).unwrap_err(),
            LayoutError::UndersizedGutter {
                old_width: 3,
                required_old_width: 5,
                new_width: 4,
                required_new_width: 5,
            }
        );
    }

    #[test]
    fn line_layout_handles_metadata_source_tabs_and_unicode_without_document_materialization() {
        let document = parse_bytes(
            "ab\t界cd\n\
             diff --git a/a.rs b/a.rs\n\
             --- a/a.rs\n\
             +++ b/a.rs\n\
             @@ -98,1 +12345,1 @@\n\
             -\t界abc\n\
             +\t界xyz\n"
                .as_bytes(),
        );
        let width = 17;
        let tab_width = 4;
        let lines = read_lines(&document);
        let mut per_line = Vec::new();

        for line in &lines {
            let gutter = gutter_spec_for_line(&line.as_borrowed());
            let line_layout = layout_line(&line.as_borrowed(), width, tab_width, gutter).unwrap();
            let cloned = line_layout.clone();
            assert_eq!(line_layout.rows().as_ptr(), cloned.rows().as_ptr());
            per_line.push(line_layout);
        }

        let metadata = &per_line[0];
        assert_eq!(metadata.rows()[0].text, "ab  界cd");

        let deletion = lines
            .iter()
            .find(|line| line.kind == LineKind::Deletion)
            .unwrap();
        let deletion_layout = &per_line[usize::try_from(deletion.id.get()).unwrap()];
        assert_eq!(
            deletion_layout
                .rows()
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            vec![" 98       │-    ", "          │ 界abc"]
        );
        assert_eq!(
            deletion_layout.rows()[0]
                .rendered_ranges_for_logical(1..2)
                .into_iter()
                .map(|range| deletion_layout.rows()[0].text[range].to_owned())
                .collect::<String>(),
            "    "
        );
        assert_eq!(
            deletion_layout.rows()[1]
                .rendered_ranges_for_logical(2..5)
                .into_iter()
                .map(|range| deletion_layout.rows()[1].text[range].to_owned())
                .collect::<String>(),
            "界"
        );

        let theme = Theme::new(crate::cli::ThemeChoice::Dark, ColorLevel::None);
        let mut per_line_output = Vec::new();
        write_direct_rows(
            &mut per_line_output,
            per_line
                .iter()
                .flat_map(|line_layout| line_layout.rows().iter()),
            &theme,
        )
        .unwrap();
        let rendered = String::from_utf8(crate::ansi::strip_ansi(&per_line_output)).unwrap();
        assert!(rendered.starts_with("ab  界cd\n"));
        assert!(rendered.contains(" 98       │-    \n          │ 界abc\n"));
    }

    #[test]
    fn line_layout_reports_typed_too_narrow_errors() {
        let metadata = parse_bytes(b"metadata\n");
        let metadata_line = line_with_kind(&metadata, LineKind::Plain);
        assert_eq!(
            layout_line(
                &metadata_line.as_borrowed(),
                0,
                4,
                GutterSpec::new(u8::MAX, u8::MAX),
            ),
            Err(LayoutError::TooNarrow {
                actual: 0,
                required: 1,
            })
        );
        let source = parse_bytes(
            b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,0 +12345 @@\n+value\n",
        );
        let addition = line_with_kind(&source, LineKind::Addition);
        let expected = LayoutError::TooNarrow {
            actual: 12,
            required: 13,
        };
        assert_eq!(
            layout_line(&addition.as_borrowed(), 12, 4, GutterSpec::new(3, 5),).unwrap_err(),
            expected,
        );
    }

    #[test]
    fn source_rows_have_dual_numbers_and_blank_continuations() {
        let document = parse_bytes(
            b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -10,1 +10,1 @@\n-abcdefgh\n+abcdefXY\n",
        );
        let deletion = line_with_kind(&document, LineKind::Deletion);
        let layout = layout_line(
            &deletion.as_borrowed(),
            14,
            4,
            gutter_spec_for_line(&deletion.as_borrowed()),
        )
        .unwrap();
        let deletion_rows = layout.rows();
        assert!(deletion_rows[0].text.starts_with(" 10     │-"));
        assert!(deletion_rows[1].text.starts_with("        │ "));
    }

    #[test]
    fn metadata_uses_full_width() {
        let document = parse_bytes(b"plain metadata line\n");
        let line = line_with_kind(&document, LineKind::Plain);
        let layout = layout_line(&line.as_borrowed(), 8, 4, GutterSpec::default()).unwrap();
        assert_eq!(layout.rows()[0].text, "plain me");
        assert_eq!(layout.rows()[1].text, "tadata l");
    }

    #[test]
    fn tab_span_maps_to_every_wrapped_expansion_cell() {
        let document = parse_bytes(
            b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -0,0 +1,1 @@\n+\tfoo\n",
        );
        let addition = line_with_kind(&document, LineKind::Addition);
        let layout = layout_line(
            &addition.as_borrowed(),
            12,
            4,
            gutter_spec_for_line(&addition.as_borrowed()),
        )
        .unwrap();
        let addition_rows: Vec<&VisualRow> = layout.rows().iter().collect();

        let expanded_tab: String = addition_rows
            .iter()
            .flat_map(|row| {
                row.rendered_ranges_for_logical(1..2)
                    .into_iter()
                    .map(|range| row.text[range].to_owned())
            })
            .collect::<Vec<_>>()
            .concat();
        assert_eq!(expanded_tab, "    ");

        let rendered_source: String = addition_rows
            .iter()
            .flat_map(|row| {
                row.rendered_ranges_for_logical(2..5)
                    .into_iter()
                    .map(|range| row.text[range].to_owned())
            })
            .collect::<Vec<_>>()
            .concat();
        assert_eq!(rendered_source, "foo");

        let row = layout
            .rows()
            .iter()
            .position(|row| row.renders_logical_offset(2))
            .unwrap();
        assert!(layout.rows()[row].text.ends_with("fo"));
    }

    #[test]
    fn colorless_direct_output_still_resets_attributes() {
        let document = parse_bytes(b"hello\n");
        let line = line_with_kind(&document, LineKind::Plain);
        let layout = layout_line(&line.as_borrowed(), 80, 4, GutterSpec::default()).unwrap();
        let mut output = Vec::new();
        write_direct_rows(
            &mut output,
            layout.rows(),
            &Theme::new(crate::cli::ThemeChoice::Dark, ColorLevel::None),
        )
        .unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "\x1b[0mhello\x1b[0m\n");
    }

    #[test]
    fn startup_row_count_stops_at_the_requested_threshold() {
        let document = parse_bytes(b"first\nsecond\nthird\n");
        assert_eq!(count_rendered_rows_up_to(&document, 80, 4, 2).unwrap(), 2);
        assert_eq!(count_rendered_rows_up_to(&document, 80, 4, 10).unwrap(), 3);
    }

    #[test]
    fn streaming_preflight_covers_raw_ansi_narrow_rows_tabs_and_mapping_weight() {
        let one_mib = MAX_SYNTAX_LINE_BYTES as u32;
        let raw_ansi = plain_metadata(0, (MAX_SYNTAX_LINE_BYTES + 1) as u64);
        assert!(line_requires_streaming_layout(raw_ansi, 80, 4, GutterSpec::default()).unwrap());

        let narrow = plain_metadata(one_mib, u64::from(one_mib));
        assert!(line_requires_streaming_layout(narrow, 1, 4, GutterSpec::default()).unwrap());

        let tab_heavy_rows = estimated_eager_rows(MAX_SYNTAX_LINE_BYTES, 1_000, 4);
        assert!(tab_heavy_rows <= MAX_EAGER_VISUAL_ROWS_PER_LINE);
        assert!(
            estimated_eager_layout_bytes(
                MAX_SYNTAX_LINE_BYTES,
                tab_heavy_rows,
                LineKind::Plain,
                GutterSpec::default(),
                4,
            ) > MAX_EAGER_LINE_LAYOUT_BYTES
        );
        assert!(line_requires_streaming_layout(narrow, 1_000, 4, GutterSpec::default()).unwrap());

        let one_row_width = MAX_SYNTAX_LINE_BYTES * 4;
        assert_eq!(
            estimated_eager_rows(MAX_SYNTAX_LINE_BYTES, one_row_width, 4),
            1
        );
        assert!(
            line_requires_streaming_layout(narrow, one_row_width, 4, GutterSpec::default())
                .unwrap()
        );
    }

    #[test]
    fn metadata_preflight_routes_raw_huge_display_short_lines_to_streaming() {
        let mut input = b"\x1b]title;".to_vec();
        input.extend(std::iter::repeat_n(
            b'x',
            MAX_SYNTAX_LINE_BYTES + CHUNKED_LINE_RAW_BYTES,
        ));
        input.extend_from_slice(b"\x07\n");
        let document = parse_bytes(&input);
        let metadata = DocumentLineMetadataReader::new()
            .read(&document, LineId::new(0))
            .unwrap()
            .unwrap();
        assert_eq!(metadata.display_len.exact(), Some(0));
        assert!(metadata.raw_len > MAX_SYNTAX_LINE_BYTES as u64);
        assert!(
            line_requires_streaming_layout(metadata, 80, 4, gutter_spec_for_metadata(metadata))
                .unwrap()
        );
    }

    #[test]
    fn public_large_line_step_yields_before_scanning_an_ansi_only_line() {
        let mut input = b"\x1b]title;".to_vec();
        input.extend(std::iter::repeat_n(
            b'x',
            MAX_SYNTAX_LINE_BYTES + CHUNKED_LINE_RAW_BYTES,
        ));
        input.extend_from_slice(b"\x07\n");
        let document = parse_bytes(&input);
        let mut reader = LargeLineTextReader::new();
        let _ = reader.start(&document, LineId::new(0), 0).unwrap();

        assert!(matches!(
            reader.next_step().unwrap(),
            crate::large_line::LargeLineReadStep::Progress
        ));
        assert_eq!(reader.stats().raw_bytes_read, CHUNKED_LINE_RAW_BYTES as u64);
    }

    #[test]
    fn direct_range_writers_emit_only_selected_stable_lines() {
        let document = parse_bytes(b"first\nsecond\nthird\n");
        let theme = Theme::new(crate::cli::ThemeChoice::Dark, ColorLevel::None);
        let decorations =
            DecorationSet::compute(&document, &theme, WhitespaceChoice::Ignore).unwrap();

        let mut decorated = Vec::new();
        write_direct_decorated_range(&mut decorated, &document, &decorations, &theme, 1..2, 80, 4)
            .unwrap();
        assert_eq!(
            String::from_utf8(crate::ansi::strip_ansi(&decorated)).unwrap(),
            "second\n"
        );

        let mut metadata = Vec::new();
        write_direct_metadata_lines(&mut metadata, &document, 1..3, 80, 4, &theme).unwrap();
        assert_eq!(
            String::from_utf8(crate::ansi::strip_ansi(&metadata)).unwrap(),
            "second\nthird\n"
        );
    }

    #[test]
    fn decorated_range_layout_is_derived_from_the_supplied_snapshot() {
        let document = parse_bytes(b"new first\nnew second\n");
        let theme = Theme::new(crate::cli::ThemeChoice::Dark, ColorLevel::None);
        let decorations =
            DecorationSet::compute(&document, &theme, WhitespaceChoice::Ignore).unwrap();
        let mut output = Vec::new();
        write_direct_decorated_range(
            &mut output,
            &document,
            &decorations,
            &theme,
            0..document.line_count(),
            80,
            4,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(crate::ansi::strip_ansi(&output)).unwrap(),
            "new first\nnew second\n"
        );
    }

    #[test]
    fn decorated_direct_writer_rejects_another_document_lineage() {
        let first = parse_bytes(b"first\n");
        let second = parse_bytes(b"second\n");
        let theme = Theme::new(crate::cli::ThemeChoice::Dark, ColorLevel::None);
        let decorations = DecorationSet::compute(&first, &theme, WhitespaceChoice::Ignore).unwrap();
        let error = write_direct_decorated_range(
            &mut Vec::new(),
            &second,
            &decorations,
            &theme,
            0..1,
            80,
            4,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            DirectWriteError::DecorationSnapshotMismatch
        ));
    }

    #[test]
    fn decorated_direct_writer_rejects_later_fallback_snapshot_from_same_lineage() {
        let raw = AppendStore::new();
        let storage = StructuralStorage::new().unwrap();
        let mut parser = GitParser::new(raw.clone(), &storage).unwrap();
        let input = b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n-let old = 1;\n+let new = 2;\n";
        let start = raw.append(input).unwrap();
        let _ = parser.push(start, input).unwrap();
        let rich = parser.snapshot().unwrap();
        let deletion = line_with_kind(&rich, LineKind::Deletion);
        let theme = Theme::new(crate::cli::ThemeChoice::Dark, ColorLevel::TrueColor);
        let decorations = DecorationSet::compute(&rich, &theme, WhitespaceChoice::Ignore).unwrap();
        assert!(decorations.belongs_to(&rich));
        assert!(decorations.belongs_to(&rich.clone()));
        assert!(decorations.line(deletion.id).is_some());

        let _ = raw.seal().unwrap();
        let fallback = parser.into_document().unwrap();
        let metadata = DocumentLineMetadataReader::new()
            .read(&fallback, deletion.id)
            .unwrap()
            .unwrap();
        assert_eq!(metadata.kind, LineKind::Fallback);
        assert_eq!(rich.identity(), fallback.identity());
        assert!(!decorations.belongs_to(&fallback));

        let error = write_direct_decorated_range(
            &mut Vec::new(),
            &fallback,
            &decorations,
            &theme,
            0..fallback.line_count(),
            80,
            4,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            DirectWriteError::DecorationSnapshotMismatch
        ));
    }

    #[test]
    fn decorated_direct_output_visualizes_changed_whitespace() {
        let document = parse_bytes(
            b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-\tfoo\n+    foo\n",
        );
        let theme = Theme::new(crate::cli::ThemeChoice::Dark, ColorLevel::None);
        let decorations =
            DecorationSet::compute(&document, &theme, WhitespaceChoice::Show).unwrap();
        let mut output = Vec::new();
        write_direct_decorated_range(
            &mut output,
            &document,
            &decorations,
            &theme,
            0..document.line_count(),
            60,
            4,
        )
        .unwrap();
        let plain = crate::ansi::strip_ansi(&output);
        let rendered = String::from_utf8(plain).unwrap();
        assert!(rendered.contains("-→···foo"), "{rendered}");
        assert!(rendered.contains("+····foo"), "{rendered}");
    }

    #[test]
    fn wrapped_changed_tab_emits_one_arrow_across_all_mapped_rows() {
        let document = parse_bytes(
            b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-\told\n+new\n",
        );
        let theme = Theme::new(crate::cli::ThemeChoice::Dark, ColorLevel::None);
        let decorations =
            DecorationSet::compute(&document, &theme, WhitespaceChoice::Show).unwrap();
        let mut output = Vec::new();
        write_direct_decorated_range(
            &mut output,
            &document,
            &decorations,
            &theme,
            0..document.line_count(),
            12,
            4,
        )
        .unwrap();
        let rendered = String::from_utf8(crate::ansi::strip_ansi(&output)).unwrap();
        assert_eq!(rendered.matches('→').count(), 1, "{rendered}");
        assert_eq!(rendered.matches('·').count(), 3, "{rendered}");
    }

    #[test]
    fn streaming_decorated_rows_preserve_tab_and_plain_whitespace_markers() {
        let old_tail = "let old_value = 1; ".repeat(320);
        let new_tail = "let new_value = 2; ".repeat(320);
        let input = format!(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-\t{old_tail}\n+    {new_tail}\n"
        );
        let document = parse_bytes(input.as_bytes());
        let theme = Theme::new(crate::cli::ThemeChoice::Dark, ColorLevel::TrueColor);
        let decorations =
            DecorationSet::compute(&document, &theme, WhitespaceChoice::Show).unwrap();
        let deletion = line_with_kind(&document, LineKind::Deletion);
        let metadata = DocumentLineMetadataReader::new()
            .read(&document, deletion.id)
            .unwrap()
            .unwrap();
        let gutter = gutter_spec_for_metadata(metadata);
        assert!(line_requires_streaming_layout(metadata, 11, 4, gutter).unwrap());
        assert!(
            !decorations
                .line(deletion.id)
                .unwrap()
                .intraline_ranges()
                .is_empty()
        );

        let mut eager = Vec::new();
        write_direct_decorated_range(
            &mut eager,
            &document,
            &decorations,
            &theme,
            0..document.line_count(),
            80,
            4,
        )
        .unwrap();
        let mut streaming = Vec::new();
        write_direct_decorated_range(
            &mut streaming,
            &document,
            &decorations,
            &theme,
            0..document.line_count(),
            11,
            4,
        )
        .unwrap();
        let eager = String::from_utf8(crate::ansi::strip_ansi(&eager)).unwrap();
        let streaming = String::from_utf8(crate::ansi::strip_ansi(&streaming)).unwrap();

        assert_eq!(streaming.matches('→').count(), eager.matches('→').count());
        assert_eq!(streaming.matches('·').count(), eager.matches('·').count());
        assert_eq!(streaming.matches('→').count(), 1);
    }

    #[test]
    fn streaming_rows_match_regular_layout_for_ansi_unicode_combining_emoji_and_tabs() {
        let document = parse_bytes(
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -0,0 +1,1 @@\n+\u{1b}[31mA\t界e\u{301}👩‍💻 xyz\u{1b}[0m\n"
                .as_bytes(),
        );
        let line = line_with_kind(&document, LineKind::Addition);
        let gutter = gutter_spec_for_line(&line.as_borrowed());
        let expected = layout_line(&line.as_borrowed(), 15, 4, gutter).unwrap();
        let mut stream = LargeLineRows::new(&document, line.id, 15, 4, gutter).unwrap();
        let mut actual = Vec::new();
        while let Some(row) = stream.next_row().unwrap() {
            actual.push(row);
        }

        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected.rows()) {
            assert_eq!(actual.text, expected.text);
            assert_eq!(actual.content_range, expected.content_range);
            assert_eq!(
                actual.rendered_content_range,
                expected.rendered_content_range
            );
            assert_eq!(actual.continuation, expected.continuation);
            for offset in actual.content_range.clone() {
                assert_eq!(
                    actual.rendered_ranges_for_logical(offset..offset + 1),
                    expected.rendered_ranges_for_logical(offset..offset + 1),
                );
            }
        }
    }

    #[test]
    fn streaming_grapheme_fallback_keeps_eager_boundaries_after_discarding_its_prefix() {
        let mut input = String::from("👩");
        input.extend(std::iter::repeat_n('\u{301}', 32_770));
        input.push('\u{200d}');
        input.push('👩');
        input.push('\n');
        let document = parse_bytes(input.as_bytes());
        let line = DocumentLineReader::new()
            .read_owned(&document, LineId::new(0))
            .unwrap()
            .unwrap();
        let expected = wrap_hard(&line.text, 3);
        let mut stream =
            LargeLineRows::new(&document, line.id, 3, 4, GutterSpec::default()).unwrap();
        let mut actual = Vec::new();
        while let Some(row) = stream.next_row().unwrap() {
            actual.push(row);
        }

        assert_eq!(
            actual
                .iter()
                .map(|row| row.content_range.clone())
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|segment| segment.byte_range.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            actual
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|segment| &line.text[segment.byte_range.clone()])
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn large_line_startup_count_stops_after_the_first_raw_chunk() {
        let mut input = vec![b'x'; MAX_SYNTAX_LINE_BYTES + CHUNKED_LINE_RAW_BYTES];
        input.push(b'\n');
        let document = parse_bytes(&input);
        let line_id = document.line_id_at(0).unwrap();

        let (rows, stats) =
            count_large_line_rows_up_to(&document, line_id, 10, 4, GutterSpec::default(), 2)
                .unwrap();

        assert_eq!(rows, 2);
        assert!(stats.raw_bytes_read <= CHUNKED_LINE_RAW_BYTES as u64);
        assert!(stats.peak_buffered_bytes < 1024 * 1024);
        assert!(stats.raw_bytes_read < input.len() as u64);
    }

    #[test]
    fn empty_large_row_window_does_not_read_toward_its_anchor() {
        let mut input = vec![b'x'; MAX_SYNTAX_LINE_BYTES + CHUNKED_LINE_RAW_BYTES];
        input.push(b'\n');
        let document = parse_bytes(&input);
        let line_id = document.line_id_at(0).unwrap();

        let window = collect_large_line_row_window(
            &document,
            line_id,
            10,
            4,
            GutterSpec::default(),
            LargeRowAnchor {
                byte_offset: MAX_SYNTAX_LINE_BYTES,
                fragment: 0,
            },
            0,
        )
        .unwrap();

        assert!(window.rows.is_empty());
        assert_eq!(window.stats, LargeLineStats::default());
    }

    #[test]
    fn navigator_reuses_forward_decoder_progress_across_repeated_single_row_steps() {
        let mut input = vec![b'x'; 2 * MAX_SYNTAX_LINE_BYTES];
        input.push(b'\n');
        let document = parse_bytes(&input);
        let line_id = document.line_id_at(0).unwrap();
        let mut navigator = LargeLineNavigator::new(
            &document,
            line_id,
            80,
            4,
            GutterSpec::default(),
            LargeRowAnchor::default(),
        )
        .unwrap();

        for _ in 0..1_000 {
            assert!(navigator.next_row().unwrap().is_some());
        }
        let stats = navigator.stats();
        assert!(stats.raw_bytes_read <= 3 * CHUNKED_LINE_RAW_BYTES as u64);
        assert!(stats.raw_bytes_read < input.len() as u64);
    }

    #[test]
    fn navigator_handles_unbounded_step_counts_without_count_sized_storage() {
        let document = parse_bytes(format!("{}\n", "x".repeat(20_000)).as_bytes());
        let line_id = document.line_id_at(0).unwrap();
        let mut navigator = LargeLineNavigator::new(
            &document,
            line_id,
            10,
            4,
            GutterSpec::default(),
            LargeRowAnchor::default(),
        )
        .unwrap();

        let end = navigator.step_forward(usize::MAX).unwrap();
        assert!(end.reached_line_boundary);
        assert!(end.anchor.byte_offset > 0);
        let start = navigator.step_backward(usize::MAX).unwrap();
        assert!(start.reached_line_boundary);
        assert_eq!(start.anchor, LargeRowAnchor::default());
    }

    #[test]
    fn navigator_window_stays_within_byte_budget_for_zero_width_heavy_rows() {
        let mut input = String::new();
        for _ in 0..8 {
            input.push('a');
            input.extend(std::iter::repeat_n('\u{301}', 120_000));
        }
        input.push('\n');
        let document = parse_bytes(input.as_bytes());
        let line_id = document.line_id_at(0).unwrap();
        let mut navigator = LargeLineNavigator::new(
            &document,
            line_id,
            1,
            4,
            GutterSpec::default(),
            LargeRowAnchor::default(),
        )
        .unwrap();

        let window = navigator
            .collect_window_bounded(5_000, 1024 * 1024)
            .unwrap();
        assert!(window.byte_limited);
        assert!(window.rows.len() < 5_000);
        assert!(window.retained_bytes <= 1024 * 1024);
        assert!(
            window
                .rows
                .iter()
                .all(|row| visual_row_weight(row) <= MAX_STREAMING_VISUAL_ROW_BYTES)
        );
        assert!(matches!(
            navigator.collect_window(usize::MAX),
            Err(DocumentLayoutError::Layout(
                LayoutError::StreamingWindowRowLimit { .. }
            ))
        ));
        assert!(matches!(
            navigator.collect_window_bounded(1, 0),
            Err(DocumentLayoutError::Layout(
                LayoutError::StreamingWindowByteLimit { .. }
            ))
        ));
    }

    #[test]
    fn direct_writer_emits_every_byte_of_a_large_plain_line_in_order() {
        let content = "ab界\t".repeat((MAX_SYNTAX_LINE_BYTES / 6) + 2);
        let document = parse_bytes(format!("{content}\n").as_bytes());
        let theme = Theme::new(crate::cli::ThemeChoice::Dark, ColorLevel::None);
        let decorations =
            DecorationSet::compute(&document, &theme, WhitespaceChoice::Ignore).unwrap();
        let mut output = Vec::new();
        write_direct_decorated_range(&mut output, &document, &decorations, &theme, 0..1, 80, 4)
            .unwrap();
        let rendered = String::from_utf8(crate::ansi::strip_ansi(&output)).unwrap();
        let without_wraps = rendered.replace('\n', "");
        let expected = crate::text::expand_tabs(&content, 4);
        assert_eq!(without_wraps, expected);
    }

    #[test]
    fn large_row_window_selects_a_wrapped_tab_fragment_without_retaining_prefix_rows() {
        let document = parse_bytes(
            b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -0,0 +1,1 @@\n+\tfoo\n",
        );
        let addition = line_with_kind(&document, LineKind::Addition);
        let gutter = gutter_spec_for_line(&addition.as_borrowed());
        let window = collect_large_line_row_window(
            &document,
            addition.id,
            12,
            4,
            gutter,
            LargeRowAnchor {
                byte_offset: 1,
                fragment: 1,
            },
            1,
        )
        .unwrap();
        let rows = window.rows;

        assert_eq!(rows.len(), 1);
        assert!(rows[0].continuation);
        assert_eq!(
            rows[0]
                .rendered_ranges_for_logical(1..2)
                .into_iter()
                .map(|range| rows[0].text[range].to_owned())
                .collect::<String>(),
            "  "
        );
    }

    #[test]
    fn large_row_anchor_at_normal_wrap_boundary_selects_the_following_row() {
        let document = parse_bytes(b"abcdefgh\n");
        let line_id = document.line_id_at(0).unwrap();
        let window = collect_large_line_row_window(
            &document,
            line_id,
            4,
            4,
            GutterSpec::default(),
            LargeRowAnchor {
                byte_offset: 4,
                fragment: 0,
            },
            1,
        )
        .unwrap();
        let rows = window.rows;

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "efgh");
    }

    #[test]
    fn tab_fragment_does_not_count_the_preceding_row_whose_range_only_ends_at_tab() {
        let document = parse_bytes(
            b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -0,0 +1,1 @@\n+a\t\n",
        );
        let addition = line_with_kind(&document, LineKind::Addition);
        let gutter = gutter_spec_for_line(&addition.as_borrowed());
        let window = collect_large_line_row_window(
            &document,
            addition.id,
            11,
            4,
            gutter,
            LargeRowAnchor {
                byte_offset: 2,
                fragment: 0,
            },
            1,
        )
        .unwrap();
        let rows = window.rows;

        assert_eq!(rows.len(), 1);
        assert!(rows[0].continuation);
        assert_eq!(rows[0].text.chars().last(), Some(' '));
        assert!(!rows[0].text.ends_with('a'));
    }

    #[test]
    fn navigator_counts_a_tab_started_inside_the_previous_row_as_fragment_zero() {
        let document = parse_bytes(
            b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -0,0 +1,1 @@\n+a\t\n",
        );
        let addition = line_with_kind(&document, LineKind::Addition);
        let gutter = gutter_spec_for_line(&addition.as_borrowed());
        let mut navigator = LargeLineNavigator::new(
            &document,
            addition.id,
            12,
            4,
            gutter,
            LargeRowAnchor::default(),
        )
        .unwrap();

        let (anchor, row) = navigator.next_row().unwrap().unwrap();
        assert_eq!(
            anchor,
            LargeRowAnchor {
                byte_offset: 2,
                fragment: 1,
            }
        );
        assert_eq!(
            row.rendered_ranges_for_logical(2..3)
                .into_iter()
                .map(|range| row.text[range].to_owned())
                .collect::<String>(),
            "  "
        );
    }

    #[test]
    fn pathological_zero_width_cluster_run_returns_a_bounded_error() {
        let mut input = String::from("a");
        input.extend(std::iter::repeat_n('\u{301}', 600_000));
        input.push('\n');
        let document = parse_bytes(input.as_bytes());
        let line_id = document.line_id_at(0).unwrap();

        let error =
            count_large_line_rows_up_to(&document, line_id, 80, 4, GutterSpec::default(), 1)
                .unwrap_err();
        assert!(matches!(
            error,
            DocumentLayoutError::Layout(LayoutError::PathologicalRowBytes { .. })
        ));
    }
}
