//! Rich unified hunk のsyntax・intraline装飾と、そのlatest-wins background worker。
//!
//! `DecorationSet` は`LineId`ごとにfull logical-line byte rangeを保持する。Source本文を扱う
//! syntect/intralineのoffsetにはdiff marker 1 byteを加えてから格納し、layout後のtab展開や折り返しは
//! rendererが`VisualRow::rendered_ranges_for_logical`で解決する。Raw input、parser identity、viewportは
//! 所有せず、256 MiBを超えるderived spanだけを後方から省略する。

use std::collections::HashMap;
use std::mem::size_of;
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded};
use thiserror::Error;

use crate::cli::WhitespaceChoice;
use crate::highlight::{HighlightError, HighlightedLine, SyntaxEngine, SyntaxSession, SyntaxSpan};
use crate::intraline::{IntralineResult, compute_refs};
use crate::model::{
    ChangeStatus, Document, DocumentLine, FileEntry, HunkId, HunkKind, LineId, LineKind,
};
use crate::text::MAX_SYNTAX_LINE_BYTES;
use crate::theme::{ColorLevel, Theme};

/// Syntax span、intraline range、line indexを合わせたderived resultの概算上限。
pub const DERIVED_RESULT_BUDGET_BYTES: usize = 256 * 1024 * 1024;

/// 1 logical lineへ適用するdecorations。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineDecoration {
    syntax: Vec<SyntaxSpan>,
    intraline: Vec<Range<usize>>,
}

impl LineDecoration {
    /// Full logical-line byteへ写したsyntax foreground spanを返す。
    #[must_use]
    pub fn syntax_spans(&self) -> &[SyntaxSpan] {
        &self.syntax
    }

    /// Full logical-line byteへ写したintraline changed spanを返す。
    #[must_use]
    pub fn intraline_ranges(&self) -> &[Range<usize>] {
        &self.intraline
    }
}

/// 1 immutable document snapshotに対応するderived decoration集合。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecorationSet {
    lines: HashMap<LineId, LineDecoration>,
    whitespace: WhitespaceChoice,
    approximate_bytes: usize,
    truncated: bool,
    warning: Option<String>,
}

impl DecorationSet {
    /// Rich unified hunkをdocument順に計算する。
    ///
    /// Old/new syntax stateはhunkごとに独立して作り、context lineは両方へ進めたうえで通常はnew側、
    /// deleted fileだけold側を採用する。Fallback file/hunkにはdecorationsを付けない。
    #[must_use]
    pub fn compute(document: &Document, theme: &Theme, whitespace: WhitespaceChoice) -> Self {
        Self::compute_with_budget(document, theme, whitespace, DERIVED_RESULT_BUDGET_BYTES)
    }

    fn compute_with_budget(
        document: &Document,
        theme: &Theme,
        whitespace: WhitespaceChoice,
        budget: usize,
    ) -> Self {
        let engine = (theme.color_level() != ColorLevel::None).then(SyntaxEngine::new);
        let syntax = match engine.as_ref() {
            None => SyntaxAvailability::Disabled,
            Some(Ok(engine)) => SyntaxAvailability::Ready(engine),
            Some(Err(error)) => SyntaxAvailability::Failed(error),
        };
        let cancelled = || false;
        let context = DecorationContext {
            document,
            theme,
            whitespace,
            syntax,
            cancelled: &cancelled,
        };
        let scope = DecorationScope {
            budget,
            priority_hunk: None,
            only_priority: false,
        };
        match Self::compute_internal(context, scope) {
            Some(result) => result,
            // Synchronous pathのcancel predicateはfalse固定なので到達しない。型上のNoneは空setへ
            // 写し、public APIからpanic経路を作らない。
            None => DecorationBuilder::new(whitespace, budget).finish(),
        }
    }

    fn compute_internal(context: DecorationContext<'_>, scope: DecorationScope) -> Option<Self> {
        let mut builder = DecorationBuilder::new(context.whitespace, scope.budget);
        if !context
            .document
            .files()
            .iter()
            .any(|file| file.mode.is_rich() && !file.hunk_ids.is_empty())
        {
            return Some(builder.finish());
        }
        if let SyntaxAvailability::Failed(error) = context.syntax {
            builder.record_syntax_error(error);
        }
        if let Some(hunk_id) = scope.priority_hunk {
            if let Some(hunk) = context.document.hunk(hunk_id)
                && let Some(file) = context.document.file(hunk.file_id)
                && file.mode.is_rich()
                && hunk.kind == HunkKind::Unified
                && !decorate_one_hunk(&mut builder, &context, file, hunk)?
            {
                return Some(builder.finish());
            }
            if scope.only_priority {
                return Some(builder.finish());
            }
        } else if scope.only_priority {
            return Some(builder.finish());
        }
        'files: for file in context
            .document
            .files()
            .iter()
            .filter(|file| file.mode.is_rich())
        {
            if (context.cancelled)() {
                return None;
            }
            for hunk_id in &file.hunk_ids {
                if Some(*hunk_id) == scope.priority_hunk {
                    continue;
                }
                if (context.cancelled)() {
                    return None;
                }
                let Some(hunk) = context.document.hunk(*hunk_id) else {
                    continue;
                };
                if hunk.kind != HunkKind::Unified {
                    continue;
                }
                if !decorate_one_hunk(&mut builder, &context, file, hunk)? {
                    break 'files;
                }
            }
        }
        Some(builder.finish())
    }

    /// Lineにdecorationsがあれば返す。
    #[must_use]
    pub fn line(&self, id: LineId) -> Option<&LineDecoration> {
        self.lines.get(&id)
    }

    /// Decoration spanを1つも保持していないかを返す。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Intraline rangeを計算したwhitespace modeを返す。
    #[must_use]
    pub const fn whitespace(&self) -> WhitespaceChoice {
        self.whitespace
    }

    /// Hash tableとspan payloadの概算byte数を返す。
    #[must_use]
    pub const fn approximate_bytes(&self) -> usize {
        self.approximate_bytes
    }

    /// Budget到達後のlineを省略したかを返す。
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Syntax failure、intraline cap、budget省略のsummaryを返す。
    #[must_use]
    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }
}

fn decorate_one_hunk(
    builder: &mut DecorationBuilder,
    context: &DecorationContext<'_>,
    file: &FileEntry,
    hunk: &crate::model::Hunk,
) -> Option<bool> {
    let lines = source_lines_for_hunk(context.document, hunk, context.cancelled)?;
    if let SyntaxAvailability::Ready(engine) = context.syntax
        && !decorate_syntax_hunk(
            builder,
            engine,
            file,
            &lines,
            context.theme,
            context.cancelled,
        )
    {
        return (!(context.cancelled)()).then_some(false);
    }
    if !decorate_intraline_hunk(builder, &lines, context.whitespace, context.cancelled) {
        return (!(context.cancelled)()).then_some(false);
    }
    Some(true)
}

/// 1回のdecorations計算で共有するimmutable input。Hunk helperへ個別に渡さず、cancel判定と
/// syntax engineの組を同じsnapshotへ固定する。
struct DecorationContext<'input> {
    document: &'input Document,
    theme: &'input Theme,
    whitespace: WhitespaceChoice,
    syntax: SyntaxAvailability<'input>,
    cancelled: &'input dyn Fn() -> bool,
}

/// Derived budgetと走査範囲。`only_priority`はpartial response専用で、指定hunkを処理した後に
/// document全体へ進まない。
#[derive(Clone, Copy)]
struct DecorationScope {
    budget: usize,
    priority_hunk: Option<HunkId>,
    only_priority: bool,
}

#[derive(Clone, Copy)]
enum SyntaxAvailability<'engine> {
    Disabled,
    Ready(&'engine SyntaxEngine),
    Failed(&'engine HighlightError),
}

fn source_lines_for_hunk<'document>(
    document: &'document Document,
    hunk: &crate::model::Hunk,
    cancelled: &dyn Fn() -> bool,
) -> Option<Vec<&'document DocumentLine>> {
    let start = usize::try_from(hunk.span.start.get()).unwrap_or(usize::MAX);
    let end = hunk
        .span
        .end_exclusive
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(document.lines().len())
        .min(document.lines().len());
    let mut lines = Vec::new();
    for line in document.lines().get(start..end).unwrap_or_default() {
        if cancelled() {
            return None;
        }
        if is_source(line.kind) && line.hunk_id == Some(hunk.id) {
            lines.push(line);
        }
    }
    Some(lines)
}

impl Default for DecorationSet {
    fn default() -> Self {
        Self {
            lines: HashMap::new(),
            whitespace: WhitespaceChoice::Ignore,
            approximate_bytes: 0,
            truncated: false,
            warning: None,
        }
    }
}

fn decorate_syntax_hunk(
    builder: &mut DecorationBuilder,
    engine: &SyntaxEngine,
    file: &FileEntry,
    lines: &[&DocumentLine],
    theme: &Theme,
    cancelled: &dyn Fn() -> bool,
) -> bool {
    let old_path = file.old_path.as_deref().or(file.new_path.as_deref());
    let new_path = file.new_path.as_deref().or(file.old_path.as_deref());
    let old_first = lines
        .iter()
        .find(|line| matches!(line.kind, LineKind::Deletion | LineKind::Context))
        .map(|line| line.content());
    let new_first = lines
        .iter()
        .find(|line| matches!(line.kind, LineKind::Addition | LineKind::Context))
        .map(|line| line.content());
    let mut old = open_session(builder, engine, old_path, old_first, theme);
    let mut new = open_session(builder, engine, new_path, new_first, theme);
    let use_old_context = file.status == ChangeStatus::Deleted;

    for line in lines {
        if cancelled() {
            return false;
        }
        if line.content().len() > MAX_SYNTAX_LINE_BYTES {
            builder.syntax_skipped_lines = builder.syntax_skipped_lines.saturating_add(1);
        }
        let selected = match line.kind {
            LineKind::Deletion => advance_session(builder, &mut old, line.content()),
            LineKind::Addition => advance_session(builder, &mut new, line.content()),
            LineKind::Context => {
                let old_line = advance_session(builder, &mut old, line.content());
                let new_line = advance_session(builder, &mut new, line.content());
                if use_old_context { old_line } else { new_line }
            }
            _ => continue,
        };
        if !builder.add_syntax(line, selected) {
            return false;
        }
    }
    true
}

fn open_session<'engine>(
    builder: &mut DecorationBuilder,
    engine: &'engine SyntaxEngine,
    path: Option<&str>,
    first_line: Option<&str>,
    theme: &Theme,
) -> Option<SyntaxSession<'engine>> {
    match engine.session(path, first_line, theme.choice()) {
        Ok(session) => session,
        Err(error) => {
            builder.record_syntax_error(&error);
            None
        }
    }
}

fn advance_session(
    builder: &mut DecorationBuilder,
    session: &mut Option<SyntaxSession<'_>>,
    line: &str,
) -> HighlightedLine {
    let Some(active) = session.as_mut() else {
        return HighlightedLine::Plain;
    };
    match active.highlight_line(line) {
        Ok(highlighted) => highlighted,
        Err(error) => {
            builder.record_syntax_error(&error);
            *session = None;
            HighlightedLine::Plain
        }
    }
}

fn decorate_intraline_hunk(
    builder: &mut DecorationBuilder,
    lines: &[&DocumentLine],
    whitespace: WhitespaceChoice,
    cancelled: &dyn Fn() -> bool,
) -> bool {
    let mut index = 0_usize;
    while index < lines.len() {
        if cancelled() {
            return false;
        }
        if lines[index].kind != LineKind::Deletion {
            index += 1;
            continue;
        }
        let removed_start = index;
        while index < lines.len() && lines[index].kind == LineKind::Deletion {
            index += 1;
        }
        let removed_end = index;
        let added_start = index;
        while index < lines.len() && lines[index].kind == LineKind::Addition {
            index += 1;
        }
        let added_end = index;
        if added_start == added_end {
            continue;
        }

        let removed = lines[removed_start..removed_end]
            .iter()
            .map(|line| line.content())
            .collect::<Vec<_>>();
        let added = lines[added_start..added_end]
            .iter()
            .map(|line| line.content())
            .collect::<Vec<_>>();
        match compute_refs(&removed, &added, whitespace) {
            IntralineResult::Computed(pairs) => {
                let mut updates = Vec::with_capacity(pairs.len().saturating_mul(2));
                for pair in pairs {
                    if cancelled() {
                        return false;
                    }
                    let old_line = lines[removed_start + pair.old_index];
                    updates.push((old_line, pair.old_changed));
                    let new_line = lines[added_start + pair.new_index];
                    updates.push((new_line, pair.new_changed));
                }
                if cancelled() {
                    return false;
                }
                if !builder.add_intraline_block(updates) {
                    return false;
                }
            }
            IntralineResult::Skipped(_) => {
                builder.intraline_skipped_blocks =
                    builder.intraline_skipped_blocks.saturating_add(1);
            }
        }
    }
    true
}

fn is_source(kind: LineKind) -> bool {
    matches!(
        kind,
        LineKind::Context | LineKind::Deletion | LineKind::Addition
    )
}

struct DecorationBuilder {
    set: DecorationSet,
    budget: usize,
    syntax_error: Option<String>,
    syntax_skipped_lines: usize,
    intraline_skipped_blocks: usize,
}

impl DecorationBuilder {
    fn new(whitespace: WhitespaceChoice, budget: usize) -> Self {
        Self {
            set: DecorationSet {
                whitespace,
                ..DecorationSet::default()
            },
            budget,
            syntax_error: None,
            syntax_skipped_lines: 0,
            intraline_skipped_blocks: 0,
        }
    }

    fn add_syntax(&mut self, line: &DocumentLine, highlighted: HighlightedLine) -> bool {
        let HighlightedLine::Styled(spans) = highlighted else {
            return true;
        };
        let spans = spans
            .into_iter()
            .filter_map(|span| shift_syntax_span(span, line.text.len()))
            .collect::<Vec<_>>();
        if spans.is_empty() {
            return true;
        }
        let cost = self
            .entry_cost(line.id)
            .saturating_add(spans.capacity().saturating_mul(size_of::<SyntaxSpan>()));
        if !self.reserve(cost) {
            return false;
        }
        self.set.lines.entry(line.id).or_default().syntax = spans;
        true
    }

    fn add_intraline_block(&mut self, updates: Vec<(&DocumentLine, Vec<Range<usize>>)>) -> bool {
        let mut pending = HashMap::<LineId, Vec<Range<usize>>>::new();
        for (line, ranges) in updates {
            pending.entry(line.id).or_default().extend(
                ranges
                    .into_iter()
                    .filter_map(|range| shift_range(range, line.text.len())),
            );
        }
        pending.retain(|_, ranges| !ranges.is_empty());
        let cost = pending.iter().fold(0_usize, |total, (line_id, ranges)| {
            total
                .saturating_add(self.entry_cost(*line_id))
                .saturating_add(ranges.capacity().saturating_mul(size_of::<Range<usize>>()))
        });
        // Pair/blockの片側だけをpublishすると、同じ変更でold/newの強調有無が分かれる。Budgetは
        // block全体へ先に予約し、成功した場合だけLineId mapを更新する。
        if !self.reserve(cost) {
            return false;
        }
        for (line_id, ranges) in pending {
            self.set
                .lines
                .entry(line_id)
                .or_default()
                .intraline
                .extend(ranges);
        }
        true
    }

    fn entry_cost(&self, id: LineId) -> usize {
        if self.set.lines.contains_key(&id) {
            0
        } else {
            // Hash bucketのcontrol byteとload-factor余白を含め、payload型の2倍を概算する。
            2 * (size_of::<LineId>() + size_of::<LineDecoration>())
        }
    }

    fn reserve(&mut self, additional: usize) -> bool {
        let Some(next) = self.set.approximate_bytes.checked_add(additional) else {
            self.set.truncated = true;
            return false;
        };
        if next > self.budget {
            self.set.truncated = true;
            return false;
        }
        self.set.approximate_bytes = next;
        true
    }

    fn record_syntax_error(&mut self, error: &HighlightError) {
        if self.syntax_error.is_none() {
            self.syntax_error = Some(error.to_string());
        }
    }

    fn finish(mut self) -> DecorationSet {
        let mut warnings = Vec::new();
        if self.set.truncated {
            warnings.push("decoration budget exceeded; later spans were omitted".to_owned());
        }
        if let Some(error) = self.syntax_error {
            warnings.push(format!("syntax: {error}"));
        }
        if self.syntax_skipped_lines > 0 {
            warnings.push(format!(
                "syntax skipped for {} oversized lines",
                self.syntax_skipped_lines
            ));
        }
        if self.intraline_skipped_blocks > 0 {
            warnings.push(format!(
                "intraline skipped for {} oversized blocks",
                self.intraline_skipped_blocks
            ));
        }
        self.set.warning = (!warnings.is_empty()).then(|| warnings.join("; "));
        self.set
    }
}

fn shift_syntax_span(mut span: SyntaxSpan, line_len: usize) -> Option<SyntaxSpan> {
    span.byte_range = shift_range(span.byte_range, line_len)?;
    Some(span)
}

fn shift_range(range: Range<usize>, line_len: usize) -> Option<Range<usize>> {
    let start = range.start.checked_add(1)?;
    let end = range.end.checked_add(1)?;
    (start <= end && end <= line_len).then_some(start..end)
}

/// Background request。`generation`はcallerのdocument/whitespace世代をそのままresponseへ返す。
#[derive(Clone, Debug)]
pub struct DecorationRequest {
    /// Callerがdocument/whitespace世代として割り当て、response照合に使うID。
    pub generation: u64,
    /// Worker taskの間だけ保持するimmutable document snapshot。
    pub document: Arc<Document>,
    /// Built-in palette選択と出力時のcolor capability。
    pub theme: Theme,
    /// このsnapshotのintraline比較mode。
    pub whitespace: WhitespaceChoice,
    /// 最初のpartial responseとfull計算の先頭で処理するvisible hunk。
    pub priority_hunk: Option<HunkId>,
}

/// Latest requestに対応するbackground result。
#[derive(Clone, Debug)]
pub struct DecorationResponse {
    /// 対応requestから複写したcaller generation。
    pub generation: u64,
    /// `false`はpriority hunkだけのpartial set、`true`はfull-document計算の完了を表す。
    pub complete: bool,
    /// Task panicだけが`Err`。個別syntax/cap failureはsetのwarningへ格納する。
    pub result: Result<DecorationSet, DecorationWorkerError>,
}

/// Decoration workerの起動またはtask panic。
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum DecorationWorkerError {
    #[error("decoration worker could not start: {0}")]
    WorkerSpawn(String),
    #[error("decoration worker panicked")]
    WorkerPanicked,
}

#[derive(Clone, Debug)]
struct QueuedRequest {
    sequence: u64,
    request: DecorationRequest,
}

/// Pending requestを1件、priority/full responseを合計2件に制限するlatest-wins worker。
pub struct DecorationWorker {
    requests: Sender<QueuedRequest>,
    pending: Receiver<QueuedRequest>,
    latest_sequence: Arc<AtomicU64>,
    submission_gate: Arc<Mutex<()>>,
    next_sequence: AtomicU64,
    /// Priority partialとfull resultをmain loopへ渡すbounded receiver。
    pub responses: Receiver<DecorationResponse>,
}

impl DecorationWorker {
    /// Capacity 1のrequestとcapacity 2のresponse channelを持つworker threadを開始する。
    pub fn spawn() -> Result<Self, DecorationWorkerError> {
        let (requests, worker_requests) = bounded::<QueuedRequest>(1);
        let pending = worker_requests.clone();
        let (response_sender, responses) = bounded::<DecorationResponse>(2);
        let response_eviction = responses.clone();
        let latest_sequence = Arc::new(AtomicU64::new(0));
        let worker_latest = Arc::clone(&latest_sequence);
        let submission_gate = Arc::new(Mutex::new(()));
        let worker_gate = Arc::clone(&submission_gate);
        thread::Builder::new()
            .name("git-ultrapager-decorations".to_owned())
            .spawn(move || {
                decoration_worker_loop(
                    worker_requests,
                    response_sender,
                    response_eviction,
                    &worker_latest,
                    &worker_gate,
                );
            })
            .map_err(|error| DecorationWorkerError::WorkerSpawn(error.to_string()))?;
        Ok(Self {
            requests,
            pending,
            latest_sequence,
            submission_gate,
            next_sequence: AtomicU64::new(0),
            responses,
        })
    }

    /// Main threadをblockせず、未開始requestと未取得responseを新しいrequestで置き換える。
    pub fn submit_latest(&self, request: DecorationRequest) -> Result<(), DecorationRequest> {
        let sequence = self
            .next_sequence
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        // Result送信とlatest更新を同じgateに置く。古いtaskが比較を終えた直後に新requestが入っても、
        // submit完了後のresponse queueへ古いgenerationを残さない。
        let _gate = lock_unpoisoned(&self.submission_gate);
        self.latest_sequence.store(sequence, Ordering::Release);
        while self.responses.try_recv().is_ok() {}
        let mut queued = QueuedRequest { sequence, request };

        loop {
            match self.requests.try_send(queued) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(returned)) => return Err(returned.request),
                Err(TrySendError::Full(returned)) => {
                    queued = returned;
                    match self.pending.try_recv() {
                        Ok(_) => {}
                        Err(TryRecvError::Empty) => std::hint::spin_loop(),
                        Err(TryRecvError::Disconnected) => return Err(queued.request),
                    }
                }
            }
        }
    }

    /// Latest completed responseを待たずに取り出す。
    pub fn try_recv(&self) -> Result<DecorationResponse, TryRecvError> {
        self.responses.try_recv()
    }

    /// Main loopの`select!`へ登録できるresponse receiverを返す。
    #[must_use]
    pub const fn responses(&self) -> &Receiver<DecorationResponse> {
        &self.responses
    }
}

fn decoration_worker_loop(
    requests: Receiver<QueuedRequest>,
    responses: Sender<DecorationResponse>,
    response_eviction: Receiver<DecorationResponse>,
    latest_sequence: &AtomicU64,
    submission_gate: &Mutex<()>,
) {
    // SyntaxSet/ThemeSetのloadはworker lifetimeで1回にする。Streaming snapshotやW toggleごとに
    // grammar bundleを再構築すると、latest requestが古いsetup待ちになる。
    let mut engine: Option<Result<SyntaxEngine, HighlightError>> = None;
    while let Ok(queued) = requests.recv() {
        let generation = queued.request.generation;
        if queued.request.theme.color_level() != ColorLevel::None && engine.is_none() {
            match catch_unwind(SyntaxEngine::new) {
                Ok(result) => engine = Some(result),
                Err(_) => {
                    let _gate = lock_unpoisoned(submission_gate);
                    if latest_sequence.load(Ordering::Acquire) == queued.sequence {
                        send_latest_response(
                            &responses,
                            &response_eviction,
                            DecorationResponse {
                                generation,
                                complete: true,
                                result: Err(DecorationWorkerError::WorkerPanicked),
                            },
                        );
                    }
                    continue;
                }
            }
        }
        let syntax = match (queued.request.theme.color_level(), engine.as_ref()) {
            (ColorLevel::None, _) | (_, None) => SyntaxAvailability::Disabled,
            (_, Some(Ok(engine))) => SyntaxAvailability::Ready(engine),
            (_, Some(Err(error))) => SyntaxAvailability::Failed(error),
        };
        let cancelled = || latest_sequence.load(Ordering::Acquire) != queued.sequence;
        if let Some(priority_hunk) = queued.request.priority_hunk {
            let partial = catch_unwind(AssertUnwindSafe(|| {
                let context = DecorationContext {
                    document: &queued.request.document,
                    theme: &queued.request.theme,
                    whitespace: queued.request.whitespace,
                    syntax,
                    cancelled: &cancelled,
                };
                let scope = DecorationScope {
                    budget: DERIVED_RESULT_BUDGET_BYTES,
                    priority_hunk: Some(priority_hunk),
                    only_priority: true,
                };
                DecorationSet::compute_internal(context, scope)
            }));
            match partial {
                Ok(Some(result)) if !result.is_empty() => {
                    let _gate = lock_unpoisoned(submission_gate);
                    if latest_sequence.load(Ordering::Acquire) != queued.sequence {
                        continue;
                    }
                    send_latest_response(
                        &responses,
                        &response_eviction,
                        DecorationResponse {
                            generation,
                            complete: false,
                            result: Ok(result),
                        },
                    );
                }
                Ok(Some(_)) => {}
                Ok(None) => continue,
                Err(_) => {
                    let _gate = lock_unpoisoned(submission_gate);
                    if latest_sequence.load(Ordering::Acquire) == queued.sequence {
                        send_latest_response(
                            &responses,
                            &response_eviction,
                            DecorationResponse {
                                generation,
                                complete: true,
                                result: Err(DecorationWorkerError::WorkerPanicked),
                            },
                        );
                    }
                    continue;
                }
            }
        }
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let context = DecorationContext {
                document: &queued.request.document,
                theme: &queued.request.theme,
                whitespace: queued.request.whitespace,
                syntax,
                cancelled: &cancelled,
            };
            let scope = DecorationScope {
                budget: DERIVED_RESULT_BUDGET_BYTES,
                priority_hunk: queued.request.priority_hunk,
                only_priority: false,
            };
            DecorationSet::compute_internal(context, scope)
        }))
        .map_err(|_| DecorationWorkerError::WorkerPanicked);
        let result = match outcome {
            Ok(Some(result)) => Ok(result),
            Ok(None) => continue,
            Err(error) => Err(error),
        };
        let _gate = lock_unpoisoned(submission_gate);
        if latest_sequence.load(Ordering::Acquire) != queued.sequence {
            continue;
        }
        send_latest_response(
            &responses,
            &response_eviction,
            DecorationResponse {
                generation,
                complete: true,
                result,
            },
        );
    }
}

fn lock_unpoisoned(mutex: &Mutex<()>) -> MutexGuard<'_, ()> {
    match mutex.lock() {
        Ok(guard) => guard,
        // Worker panicはresponseへ変換する。Gate自体に保護対象dataはないため、poison後も排他だけを
        // 再利用すればlatest-wins契約を維持できる。
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn send_latest_response(
    responses: &Sender<DecorationResponse>,
    response_eviction: &Receiver<DecorationResponse>,
    mut response: DecorationResponse,
) {
    loop {
        match responses.try_send(response) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => return,
            Err(TrySendError::Full(returned)) => {
                response = returned;
                let _ = response_eviction.try_recv();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::cli::ThemeChoice;
    use crate::parser::parse_bytes;
    use crate::theme::ColorLevel;

    fn theme() -> Theme {
        Theme::new(ThemeChoice::Dark, ColorLevel::TrueColor)
    }

    #[test]
    fn rename_uses_independent_old_and_new_syntax_paths() {
        let document = parse_bytes(
            b"diff --git a/src/lib.rs b/src/lib.py\nsimilarity index 80%\nrename from src/lib.rs\nrename to src/lib.py\n--- a/src/lib.rs\n+++ b/src/lib.py\n@@ -1 +1 @@\n-fn old() {}\n+def new():\n",
        );
        let decorations = DecorationSet::compute(&document, &theme(), WhitespaceChoice::Ignore);
        let deletion = document
            .lines()
            .iter()
            .find(|line| line.kind == LineKind::Deletion)
            .unwrap();
        let addition = document
            .lines()
            .iter()
            .find(|line| line.kind == LineKind::Addition)
            .unwrap();
        assert!(!decorations.line(deletion.id).unwrap().syntax.is_empty());
        assert!(!decorations.line(addition.id).unwrap().syntax.is_empty());

        let engine = SyntaxEngine::new().unwrap();
        let expected_old = engine
            .session(Some("src/lib.rs"), Some("fn old() {}"), ThemeChoice::Dark)
            .unwrap()
            .unwrap()
            .highlight_line("fn old() {}")
            .unwrap();
        let expected_new = engine
            .session(Some("src/lib.py"), Some("def new():"), ThemeChoice::Dark)
            .unwrap()
            .unwrap()
            .highlight_line("def new():")
            .unwrap();
        let HighlightedLine::Styled(expected_old) = expected_old else {
            panic!("expected old syntax");
        };
        let HighlightedLine::Styled(expected_new) = expected_new else {
            panic!("expected new syntax");
        };
        assert_eq!(
            decorations.line(deletion.id).unwrap().syntax,
            expected_old
                .into_iter()
                .filter_map(|span| shift_syntax_span(span, deletion.text.len()))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            decorations.line(addition.id).unwrap().syntax,
            expected_new
                .into_iter()
                .filter_map(|span| shift_syntax_span(span, addition.text.len()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn rename_context_prefers_new_side_syntax() {
        let document = parse_bytes(
            b"diff --git a/src/notes.rs b/src/notes.md\nsimilarity index 100%\nrename from src/notes.rs\nrename to src/notes.md\n--- a/src/notes.rs\n+++ b/src/notes.md\n@@ -1 +1 @@\n # heading\n",
        );
        let context = document
            .lines()
            .iter()
            .find(|line| line.kind == LineKind::Context)
            .unwrap();
        let decorations = DecorationSet::compute(&document, &theme(), WhitespaceChoice::Ignore);

        let engine = SyntaxEngine::new().unwrap();
        let expected = engine
            .session(Some("src/notes.md"), Some("# heading"), ThemeChoice::Dark)
            .unwrap()
            .unwrap()
            .highlight_line("# heading")
            .unwrap();
        let HighlightedLine::Styled(expected) = expected else {
            panic!("expected markdown syntax");
        };
        assert_eq!(
            decorations.line(context.id).unwrap().syntax,
            expected
                .into_iter()
                .filter_map(|span| shift_syntax_span(span, context.text.len()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn deleted_file_context_keeps_old_side_state() {
        let document = parse_bytes(
            b"diff --git a/src/old.rs b/src/old.rs\ndeleted file mode 100644\n--- a/src/old.rs\n+++ /dev/null\n@@ -1,2 +1 @@\n-/*\n let value = 1;\n",
        );
        let context = document
            .lines()
            .iter()
            .find(|line| line.kind == LineKind::Context)
            .unwrap();
        assert_eq!(document.files()[0].status, ChangeStatus::Deleted);
        let decorations = DecorationSet::compute(&document, &theme(), WhitespaceChoice::Ignore);

        let engine = SyntaxEngine::new().unwrap();
        let mut old = engine
            .session(Some("src/old.rs"), Some("/*"), ThemeChoice::Dark)
            .unwrap()
            .unwrap();
        let _ = old.highlight_line("/*").unwrap();
        let expected = old.highlight_line("let value = 1;").unwrap();
        let HighlightedLine::Styled(expected) = expected else {
            panic!("expected old-side syntax");
        };
        assert_eq!(
            decorations.line(context.id).unwrap().syntax,
            expected
                .into_iter()
                .filter_map(|span| shift_syntax_span(span, context.text.len()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn syntax_state_resets_at_each_hunk() {
        let document = parse_bytes(
            b"diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-old\n+/*\n@@ -10 +10 @@\n let value = 1;\n",
        );
        let context = document
            .lines()
            .iter()
            .find(|line| line.kind == LineKind::Context)
            .unwrap();
        let decorations = DecorationSet::compute(&document, &theme(), WhitespaceChoice::Ignore);

        let engine = SyntaxEngine::new().unwrap();
        let expected = engine
            .session(
                Some("src/main.rs"),
                Some("let value = 1;"),
                ThemeChoice::Dark,
            )
            .unwrap()
            .unwrap()
            .highlight_line("let value = 1;")
            .unwrap();
        let HighlightedLine::Styled(expected) = expected else {
            panic!("expected reset syntax");
        };
        assert_eq!(
            decorations.line(context.id).unwrap().syntax,
            expected
                .into_iter()
                .filter_map(|span| shift_syntax_span(span, context.text.len()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn intraline_ignore_and_show_keep_full_line_offsets() {
        let document = parse_bytes(
            b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-foo\tbar\n+foo   bar\n",
        );
        let deletion = document
            .lines()
            .iter()
            .find(|line| line.kind == LineKind::Deletion)
            .unwrap();
        let ignored = DecorationSet::compute(&document, &theme(), WhitespaceChoice::Ignore);
        assert!(
            ignored
                .line(deletion.id)
                .is_none_or(|line| line.intraline.is_empty())
        );

        let shown = DecorationSet::compute(&document, &theme(), WhitespaceChoice::Show);
        let ranges = &shown.line(deletion.id).unwrap().intraline;
        assert!(
            ranges
                .iter()
                .any(|range| &deletion.text[range.clone()] == "\t")
        );
        assert!(ranges.iter().all(|range| range.start >= 1));
    }

    #[test]
    fn small_budget_marks_result_truncated() {
        let document = parse_bytes(
            b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-let old = 1;\n+let new = 2;\n",
        );
        let result =
            DecorationSet::compute_with_budget(&document, &theme(), WhitespaceChoice::Show, 1);
        assert!(result.is_truncated());
        assert!(result.warning().unwrap().contains("budget"));
    }

    #[test]
    fn budget_never_publishes_only_one_side_of_intraline_block() {
        let document = parse_bytes(
            b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old value\n+new value\n",
        );
        let colorless = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        let complete = DecorationSet::compute_with_budget(
            &document,
            &colorless,
            WhitespaceChoice::Show,
            usize::MAX,
        );
        let constrained = DecorationSet::compute_with_budget(
            &document,
            &colorless,
            WhitespaceChoice::Show,
            complete.approximate_bytes().saturating_sub(1),
        );
        assert!(constrained.is_truncated());
        for line in document
            .lines()
            .iter()
            .filter(|line| matches!(line.kind, LineKind::Deletion | LineKind::Addition))
        {
            assert!(
                constrained
                    .line(line.id)
                    .is_none_or(|decoration| decoration.intraline.is_empty())
            );
        }
    }

    #[test]
    fn no_color_skips_syntax_but_keeps_intraline_data() {
        let document = parse_bytes(
            b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-let old = 1;\n+let new = 2;\n",
        );
        let colorless = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        let result = DecorationSet::compute(&document, &colorless, WhitespaceChoice::Show);
        for line in document.lines().iter().filter(|line| is_source(line.kind)) {
            let decoration = result.line(line.id).unwrap();
            assert!(decoration.syntax.is_empty());
            assert!(!decoration.intraline.is_empty());
        }
    }

    #[test]
    fn worker_replaces_queued_stale_response() {
        let worker = DecorationWorker::spawn().unwrap();
        let document = Arc::new(parse_bytes(b"plain\n"));
        worker
            .submit_latest(DecorationRequest {
                generation: 1,
                document: Arc::clone(&document),
                theme: theme(),
                whitespace: WhitespaceChoice::Ignore,
                priority_hunk: None,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while worker.responses.is_empty() && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(!worker.responses.is_empty());

        worker
            .submit_latest(DecorationRequest {
                generation: 2,
                document,
                theme: theme(),
                whitespace: WhitespaceChoice::Show,
                priority_hunk: None,
            })
            .unwrap();
        let response = worker
            .responses
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(response.generation, 2);
        assert_eq!(
            response.result.unwrap().whitespace(),
            WhitespaceChoice::Show
        );
        assert!(worker.responses.try_recv().is_err());
    }

    #[test]
    fn worker_publishes_only_the_latest_back_to_back_request() {
        let worker = DecorationWorker::spawn().unwrap();
        let document = Arc::new(parse_bytes(b"plain\n"));
        worker
            .submit_latest(DecorationRequest {
                generation: 10,
                document: Arc::clone(&document),
                theme: theme(),
                whitespace: WhitespaceChoice::Ignore,
                priority_hunk: None,
            })
            .unwrap();
        worker
            .submit_latest(DecorationRequest {
                generation: 11,
                document,
                theme: theme(),
                whitespace: WhitespaceChoice::Show,
                priority_hunk: None,
            })
            .unwrap();

        let response = worker
            .responses
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(response.generation, 11);
        assert!(worker.responses.try_recv().is_err());
    }

    #[test]
    fn compute_abandons_a_stale_snapshot_between_lines() {
        let mut input = String::from(
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,50 +1,50 @@\n",
        );
        for index in 0..50 {
            input.push_str(&format!(" line {index}\n"));
        }
        let document = parse_bytes(input.as_bytes());
        let checks = Cell::new(0_usize);
        let cancelled = || {
            let current = checks.get();
            checks.set(current + 1);
            current >= 8
        };
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        let context = DecorationContext {
            document: &document,
            theme: &theme,
            whitespace: WhitespaceChoice::Ignore,
            syntax: SyntaxAvailability::Disabled,
            cancelled: &cancelled,
        };
        let scope = DecorationScope {
            budget: DERIVED_RESULT_BUDGET_BYTES,
            priority_hunk: None,
            only_priority: false,
        };
        let result = DecorationSet::compute_internal(context, scope);
        assert!(result.is_none());
        assert!(checks.get() < 50);
    }

    #[test]
    fn worker_publishes_priority_hunk_before_full_set() {
        let document = Arc::new(parse_bytes(
            b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-let first = 1;\n+let first = 2;\n@@ -10 +10 @@\n-let second = 1;\n+let second = 2;\n",
        ));
        let priority = document.hunks()[1].id;
        let first_line = document
            .lines()
            .iter()
            .find(|line| line.hunk_id == Some(document.hunks()[0].id) && is_source(line.kind))
            .unwrap()
            .id;
        let priority_line = document
            .lines()
            .iter()
            .find(|line| line.hunk_id == Some(priority) && is_source(line.kind))
            .unwrap()
            .id;
        let worker = DecorationWorker::spawn().unwrap();
        worker
            .submit_latest(DecorationRequest {
                generation: 20,
                document,
                theme: theme(),
                whitespace: WhitespaceChoice::Show,
                priority_hunk: Some(priority),
            })
            .unwrap();

        let partial = worker
            .responses
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(!partial.complete);
        let partial = partial.result.unwrap();
        assert!(partial.line(priority_line).is_some());
        assert!(partial.line(first_line).is_none());

        let complete = worker
            .responses
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(complete.complete);
        let complete = complete.result.unwrap();
        assert!(complete.line(priority_line).is_some());
        assert!(complete.line(first_line).is_some());
    }
}
