//! File type detection と syntax highlighting actor。
//!
//! `syntect::ParseState` / `HighlightState` は worker 間を移動させず、専用 actor thread が所有する。
//! AppState へ返すのは thread-independent な byte span と RGB/style bit だけである。diff background、
//! search、intraline の precedence は renderer が合成し、この module は source foreground だけを
//! 提供する。Built-in `SyntaxSet`/`ThemeSet`はworker lifetimeのimmutable program assetであり、256 MiB
//! derived-data partitionには含めない。Requestごとのparse state、synthetic line、region/span Vecだけを
//! `DecorationWorkspace`へ計上する。

use std::collections::HashMap;
use std::mem::{replace, size_of};
use std::ops::Range;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::thread;

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, HighlightState, Highlighter, Style, ThemeSet};
use syntect::parsing::syntax_definition::{
    Context, ContextReference, MatchOperation, MatchPattern, Pattern,
};
use syntect::parsing::{
    ParseState, SyntaxDefinition, SyntaxReference, SyntaxSet, SyntaxSetBuilder,
};
use thiserror::Error;

use crate::cli::ThemeChoice;
use crate::derived_memory::{DerivedMemoryLease, DerivedMemoryLedger, DerivedMemoryPool};
use crate::text::MAX_SYNTAX_LINE_BYTES;

const SYNTAX_STATE_BASE_BYTES: usize = 64 * 1024;
const MAX_CONTINUATION_CANDIDATES: usize = 64;

/// Syntax style を renderer へ渡す immutable span。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntaxSpan {
    /// Request line のoriginal UTF-8 byte range。synthetic newlineは含まない。
    pub byte_range: Range<usize>,
    pub foreground: (u8, u8, u8),
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// 1 logical source line の highlight 結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HighlightedLine {
    Styled(Vec<SyntaxSpan>),
    Plain,
}

/// Decoration workerへ返すsyntax spanとDecorationWorkspace lease。
///
/// Styled payloadをleaseより先にdropし、builderは[`Self::into_parts`]でspan ownershipと同時にresult
/// poolへbyteをtransferする。`None` leaseはPlain resultを表す。
#[derive(Debug)]
pub(crate) struct BudgetedHighlightedLine {
    line: HighlightedLine,
    lease: Option<DerivedMemoryLease>,
}

impl BudgetedHighlightedLine {
    pub(crate) fn plain() -> Self {
        Self {
            line: HighlightedLine::Plain,
            lease: None,
        }
    }

    pub(crate) fn into_parts(self) -> (HighlightedLine, Option<DerivedMemoryLease>) {
        (self.line, self.lease)
    }

    fn line(&self) -> &HighlightedLine {
        &self.line
    }
}

/// Syntax actor へ渡す hunk 単位の request。
#[derive(Clone, Debug)]
pub struct SyntaxRequest {
    /// Reflow/document replacement後のstale responseを捨てるための世代。
    pub generation: u64,
    /// 同一世代内でrequestとresponseを対応付けるID。
    pub request_id: u64,
    /// File type判定だけに使うGit由来のpath。repository fileは開かない。
    pub path: Option<String>,
    /// Pathで判定できない場合の追加/context側先頭行。shebang以外は判定に使わない。
    pub first_line: Option<String>,
    /// 同じside・hunkに属し、先頭からparse stateを連続させるlogical line列。
    pub lines: Arc<[String]>,
    pub theme: ThemeChoice,
}

/// Syntax actor から main state へ返す result。
#[derive(Clone, Debug)]
pub struct SyntaxResponse {
    /// Requestから複写し、main state側でstale判定に使う世代。
    pub generation: u64,
    /// Requestから複写した対応ID。
    pub request_id: u64,
    /// Line順のspan列。worker panicは`Err`となり、actor自体は次のrequestを受け付ける。
    pub result: Result<Vec<HighlightedLine>, HighlightError>,
}

/// Highlight setup または grammar evaluation の failure。
#[derive(Clone, Debug, Error)]
pub enum HighlightError {
    #[error("built-in syntax set does not contain Plain Text")]
    MissingPlainText,
    #[error("built-in theme `{0}` is unavailable")]
    MissingTheme(&'static str),
    #[error("syntax highlighting failed: {0}")]
    Syntect(String),
    #[error("syntax worker could not start: {0}")]
    WorkerSpawn(String),
    #[error("syntax worker panicked")]
    WorkerPanicked,
    #[error("syntax decoration workspace is exhausted")]
    WorkspaceBudgetExceeded,
}

/// 専用 syntax thread の request handle。
pub struct SyntaxWorker {
    requests: Sender<SyntaxRequest>,
    pub responses: Receiver<SyntaxResponse>,
}

impl SyntaxWorker {
    /// Bounded queue を持つ actor を開始する。
    ///
    /// OS がthreadを作れない場合はhandleを返さず、callerがinteractive起動をerrorへ写せる。queueが
    /// 満杯の場合は[`Self::try_submit`]がrequestを返し、callerがviewport優先順位を更新する。
    pub fn spawn(queue_capacity: usize) -> Result<Self, HighlightError> {
        let (request_tx, request_rx) = bounded::<SyntaxRequest>(queue_capacity.max(1));
        let (response_tx, response_rx) = bounded::<SyntaxResponse>(queue_capacity.max(1));
        thread::Builder::new()
            .name("git-ultrapager-syntax".to_owned())
            .spawn(move || syntax_actor(request_rx, response_tx))
            .map_err(|error| HighlightError::WorkerSpawn(error.to_string()))?;
        Ok(Self {
            requests: request_tx,
            responses: response_rx,
        })
    }

    /// Request を待たずにenqueueする。
    ///
    /// Queue満杯またはworker切断時は所有権をcallerへ返す。callerは待機せず、次のviewport requestを
    /// 優先するか、切断をtask failureとして扱う。
    pub fn try_submit(&self, request: SyntaxRequest) -> Result<(), SyntaxRequest> {
        match self.requests.try_send(request) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(request) | TrySendError::Disconnected(request)) => Err(request),
        }
    }
}

fn syntax_actor(requests: Receiver<SyntaxRequest>, responses: Sender<SyntaxResponse>) {
    let engine =
        std::panic::catch_unwind(SyntaxEngine::new).unwrap_or(Err(HighlightError::WorkerPanicked));
    while let Ok(request) = requests.recv() {
        let generation = request.generation;
        let request_id = request.request_id;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            engine
                .as_ref()
                .map_err(|error| error.clone())
                .and_then(|engine| engine.highlight(&request))
        }))
        .unwrap_or(Err(HighlightError::WorkerPanicked));
        if responses
            .send(SyntaxResponse {
                generation,
                request_id,
                result,
            })
            .is_err()
        {
            break;
        }
    }
}

pub(crate) struct SyntaxEngine {
    assets: Arc<SyntaxAssets>,
}

/// Bundled grammarへ内部seed entryを足したprocess-wide asset。
///
/// Seedはsanitized sourceに現れないNUL prefixでだけ到達でき、通常sessionのgrammar動作を変えない。
/// Rebuildとtheme selector解析は最初のengine生成時に1回行い、actorと同期decoratorはimmutable setを共有する。
struct SyntaxAssets {
    syntaxes: SyntaxSet,
    themes: ThemeSet,
    continuation_candidates: HashMap<syntect::parsing::Scope, Vec<ContinuationCandidate>>,
}

#[derive(Clone, Debug)]
struct ContinuationCandidate {
    marker: String,
}

/// Grammar内の1回のpush/setが作るcontext stackと、そのtopへ付くprototype。
/// Openerのregexやcaptureは復元せず、backreferenceを使うstackは候補生成時に除外する。
#[derive(Clone, Debug, PartialEq, Eq)]
struct ContinuationStack {
    contexts: Vec<ContextReference>,
    with_prototype: Option<ContextReference>,
}

impl SyntaxEngine {
    pub(crate) fn new() -> Result<Self, HighlightError> {
        static ASSETS: OnceLock<Result<Arc<SyntaxAssets>, HighlightError>> = OnceLock::new();
        let assets = ASSETS
            .get_or_init(|| {
                let themes = ThemeSet::load_defaults();
                let (syntaxes, continuation_candidates) =
                    syntax_set_with_continuation_candidates(&themes);
                if syntaxes.find_syntax_by_name("Plain Text").is_none() {
                    return Err(HighlightError::MissingPlainText);
                }
                Ok(Arc::new(SyntaxAssets {
                    syntaxes,
                    themes,
                    continuation_candidates,
                }))
            })
            .clone()?;
        Ok(Self { assets })
    }

    pub(crate) fn session<'engine>(
        &'engine self,
        path: Option<&str>,
        first_line: Option<&str>,
        theme_choice: ThemeChoice,
    ) -> Result<Option<SyntaxSession<'engine>>, HighlightError> {
        self.session_with_state_lease(path, first_line, theme_choice, None)
    }

    pub(crate) fn session_with_budget<'engine>(
        &'engine self,
        path: Option<&str>,
        first_line: Option<&str>,
        theme_choice: ThemeChoice,
        ledger: &DerivedMemoryLedger,
    ) -> Result<Option<SyntaxSession<'engine>>, HighlightError> {
        let lease = ledger
            .reserve(
                DerivedMemoryPool::DecorationWorkspace,
                SYNTAX_STATE_BASE_BYTES,
            )
            .map_err(|_| HighlightError::WorkspaceBudgetExceeded)?;
        self.session_with_state_lease(path, first_line, theme_choice, Some(lease))
    }

    fn session_with_state_lease<'engine>(
        &'engine self,
        path: Option<&str>,
        first_line: Option<&str>,
        theme_choice: ThemeChoice,
        state_lease: Option<DerivedMemoryLease>,
    ) -> Result<Option<SyntaxSession<'engine>>, HighlightError> {
        let Some(syntax) = detect_syntax(&self.assets.syntaxes, path, first_line) else {
            return Ok(None);
        };
        let theme = self.theme(theme_choice)?;
        Ok(Some(SyntaxSession::new(
            &self.assets.syntaxes,
            syntax,
            theme,
            state_lease,
        )))
    }

    /// Hunk先頭から読めるsource lineだけを使い、欠けたprefixで継続中だったgrammar contextを選ぶ。
    ///
    /// 候補は改行後も残り、prefix内で通常stateへ合流するcontextに限る。複数候補が異なるspanを
    /// 返す場合は入力だけでは一意に決められないため、通常stateのsessionを返す。
    pub(crate) fn session_with_inferred_continuation<'engine>(
        &'engine self,
        path: Option<&str>,
        first_line: Option<&str>,
        theme_choice: ThemeChoice,
        lines: &[String],
        ledger: &DerivedMemoryLedger,
    ) -> Result<Option<SyntaxSession<'engine>>, HighlightError> {
        let Some(syntax) = detect_syntax(&self.assets.syntaxes, path, first_line) else {
            return Ok(None);
        };
        let theme = self.theme(theme_choice)?;
        let marker = self.infer_continuation_marker(syntax, theme, lines, ledger);
        let lease = ledger
            .reserve(
                DerivedMemoryPool::DecorationWorkspace,
                SYNTAX_STATE_BASE_BYTES,
            )
            .map_err(|_| HighlightError::WorkspaceBudgetExceeded)?;
        let mut session = SyntaxSession::new(&self.assets.syntaxes, syntax, theme, Some(lease));
        if let Some(marker) = marker {
            let seeded = session.highlight_line_with_budget(&marker, ledger)?;
            if seeded.is_none() {
                session.reset_after_skipped_line();
            }
        }
        Ok(Some(session))
    }

    fn theme(
        &self,
        theme_choice: ThemeChoice,
    ) -> Result<&syntect::highlighting::Theme, HighlightError> {
        let theme_name = match theme_choice {
            ThemeChoice::Dark => "base16-ocean.dark",
            ThemeChoice::Light => "InspiredGitHub",
        };
        self.assets
            .themes
            .themes
            .get(theme_name)
            .ok_or(HighlightError::MissingTheme(theme_name))
    }

    fn infer_continuation_marker(
        &self,
        syntax: &SyntaxReference,
        theme: &syntect::highlighting::Theme,
        lines: &[String],
        ledger: &DerivedMemoryLedger,
    ) -> Option<String> {
        let candidates = self.assets.continuation_candidates.get(&syntax.scope)?;
        let output_vec_bytes = lines
            .len()
            .checked_mul(size_of::<BudgetedHighlightedLine>())?
            .checked_mul(2)?;
        let _output_vec_lease = ledger
            .reserve(DerivedMemoryPool::DecorationWorkspace, output_vec_bytes)
            .ok()?;
        let mut accepted: Option<(usize, String, Vec<BudgetedHighlightedLine>)> = None;

        for candidate in candidates {
            let mut baseline = self.inference_session(syntax, theme, ledger)?;
            let mut inferred = self.inference_session(syntax, theme, ledger)?;
            drop(baseline.highlight_line_with_budget("", ledger).ok()??);
            drop(
                inferred
                    .highlight_line_with_budget(&candidate.marker, ledger)
                    .ok()??,
            );
            if inferred.same_state(&mut baseline) {
                continue;
            }

            let mut convergence_line = None;
            let mut output = Vec::with_capacity(lines.len());
            for (line_index, line) in lines.iter().enumerate() {
                let baseline_line = baseline.highlight_line_with_budget(line, ledger).ok()??;
                let inferred_line = inferred.highlight_line_with_budget(line, ledger).ok()??;
                let states_match = inferred.same_state(&mut baseline);
                if states_match && convergence_line.is_none() {
                    convergence_line = Some(line_index);
                }
                drop(baseline_line);
                output.push(inferred_line);
            }
            let Some(convergence_line) = convergence_line else {
                continue;
            };
            // 先頭行だけで戻るcontextは引数・型・groupなどにも多く、欠けたprefixの証拠にならない。
            // 2行以上を同じstateで説明できる候補だけに絞り、通常コードの誤着色を避ける。
            if convergence_line == 0 {
                continue;
            }

            if accepted
                .as_ref()
                .is_some_and(|(accepted_line, _, _)| convergence_line > *accepted_line)
            {
                continue;
            }
            if accepted
                .as_ref()
                .is_some_and(|(accepted_line, _, _)| convergence_line < *accepted_line)
            {
                accepted = Some((convergence_line, candidate.marker.clone(), output));
                continue;
            }
            if let Some((_, _, expected)) = accepted.as_ref() {
                if output.len() != expected.len()
                    || output.iter().zip(expected).any(|(actual, expected)| {
                        !highlighted_lines_equivalent(actual.line(), expected.line())
                    })
                {
                    return None;
                }
            } else {
                accepted = Some((convergence_line, candidate.marker.clone(), output));
            }
        }

        accepted.map(|(_, marker, _)| marker)
    }

    fn inference_session<'engine>(
        &'engine self,
        syntax: &'engine SyntaxReference,
        theme: &'engine syntect::highlighting::Theme,
        ledger: &DerivedMemoryLedger,
    ) -> Option<SyntaxSession<'engine>> {
        let lease = ledger
            .reserve(
                DerivedMemoryPool::DecorationWorkspace,
                SYNTAX_STATE_BASE_BYTES,
            )
            .ok()?;
        Some(SyntaxSession::new(
            &self.assets.syntaxes,
            syntax,
            theme,
            Some(lease),
        ))
    }

    fn highlight(&self, request: &SyntaxRequest) -> Result<Vec<HighlightedLine>, HighlightError> {
        let mut session = self.session(
            request.path.as_deref(),
            request.first_line.as_deref(),
            request.theme,
        )?;
        let mut output = Vec::with_capacity(request.lines.len());

        for line in request.lines.iter() {
            output.push(match session.as_mut() {
                Some(session) => session.highlight_line(line)?,
                None => HighlightedLine::Plain,
            });
        }
        Ok(output)
    }
}

/// 1 file side・1 hunk の連続したparse state。
///
/// Decoration計算はold/new用に2つ作り、deletionはoldだけ、additionはnewだけ、contextは両方へ
/// 順番どおり渡す。Hunkごとに作り直すことで、別hunkの不完全なsyntax stateを持ち越さない。
pub(crate) struct SyntaxSession<'engine> {
    syntaxes: &'engine SyntaxSet,
    syntax: &'engine syntect::parsing::SyntaxReference,
    theme: &'engine syntect::highlighting::Theme,
    highlighter: HighlightLines<'engine>,
    state_lease: Option<DerivedMemoryLease>,
}

#[derive(Debug, PartialEq, Eq)]
struct SyntaxSessionState {
    highlight: HighlightState,
    parse: ParseState,
}

impl SyntaxSession<'_> {
    const STATE_BYTES_PER_INPUT_BYTE: usize = 8;

    fn new<'engine>(
        syntaxes: &'engine SyntaxSet,
        syntax: &'engine SyntaxReference,
        theme: &'engine syntect::highlighting::Theme,
        state_lease: Option<DerivedMemoryLease>,
    ) -> SyntaxSession<'engine> {
        SyntaxSession {
            syntaxes,
            syntax,
            theme,
            highlighter: HighlightLines::new(syntax, theme),
            state_lease,
        }
    }

    /// Logical lineのtransient working setを事前leaseし、output capacity leaseを返す。
    ///
    /// `Ok(None)`はworkspace不足。Lineをparseせずstateをresetするため、callerはそのlineだけPlainとして
    /// 扱い、後続lineを未知stateから継続しない。
    pub(crate) fn highlight_line_with_budget(
        &mut self,
        line: &str,
        ledger: &DerivedMemoryLedger,
    ) -> Result<Option<BudgetedHighlightedLine>, HighlightError> {
        if line.len() > MAX_SYNTAX_LINE_BYTES || !self.ensure_state_budget(line.len()) {
            self.reset_after_skipped_line();
            return Ok(None);
        }
        let Some(workspace_bytes) = syntax_workspace_upper_bound(line.len()) else {
            self.reset_after_skipped_line();
            return Ok(None);
        };
        let mut workspace =
            match ledger.reserve(DerivedMemoryPool::DecorationWorkspace, workspace_bytes) {
                Ok(lease) => lease,
                Err(_) => {
                    self.reset_after_skipped_line();
                    return Ok(None);
                }
            };
        let highlighted = self.highlight_line_inner(line)?;
        let output_bytes = match &highlighted {
            HighlightedLine::Styled(spans) => spans
                .capacity()
                .checked_mul(size_of::<SyntaxSpan>())
                .unwrap_or(workspace_bytes),
            HighlightedLine::Plain => 0,
        };
        if output_bytes > workspace.bytes() {
            // Upper-bound違反を事後chargeすると、allocation済みの未計上期間が生じる。Payloadをlease
            // より先にdropし、このlineをpublishしない。
            drop(highlighted);
            return Ok(None);
        } else {
            let released = workspace.bytes() - output_bytes;
            if workspace.release(released).is_err() {
                return Ok(None);
            }
        }
        Ok(Some(BudgetedHighlightedLine {
            line: highlighted,
            lease: (output_bytes > 0).then_some(workspace),
        }))
    }

    /// Logical source lineを1行進め、original content byteへのspanを返す。
    pub(crate) fn highlight_line(&mut self, line: &str) -> Result<HighlightedLine, HighlightError> {
        if line.len() > MAX_SYNTAX_LINE_BYTES {
            self.reset_after_skipped_line();
            return Ok(HighlightedLine::Plain);
        }

        self.highlight_line_inner(line)
    }

    fn highlight_line_inner(&mut self, line: &str) -> Result<HighlightedLine, HighlightError> {
        // `load_defaults_newlines` のgrammarはline末尾の`\n`でsingle-line contextをpopする。
        // Modelは改行をlogical lineから分離するため、worker内だけsynthetic newlineを補い、返す
        // spanはoriginal byte lengthでclipする。
        let mut line_with_newline = String::with_capacity(line.len() + 1);
        line_with_newline.push_str(line);
        line_with_newline.push('\n');
        let regions = self
            .highlighter
            .highlight_line(&line_with_newline, self.syntaxes)
            .map_err(|error| HighlightError::Syntect(error.to_string()))?;
        let mut offset = 0_usize;
        let spans = regions
            .into_iter()
            .filter_map(|(style, text)| {
                let start = offset;
                offset += text.len();
                let end = offset.min(line.len());
                (start < end).then_some(SyntaxSpan {
                    byte_range: start..end,
                    foreground: (style.foreground.r, style.foreground.g, style.foreground.b),
                    bold: style.font_style.contains(FontStyle::BOLD),
                    italic: style.font_style.contains(FontStyle::ITALIC),
                    underline: style.font_style.contains(FontStyle::UNDERLINE),
                })
            })
            .collect();
        Ok(HighlightedLine::Styled(spans))
    }

    fn ensure_state_budget(&mut self, line_len: usize) -> bool {
        let Some(lease) = self.state_lease.as_mut() else {
            return false;
        };
        let Some(required) = line_len
            .checked_mul(Self::STATE_BYTES_PER_INPUT_BYTE)
            .and_then(|bytes| bytes.checked_add(SYNTAX_STATE_BASE_BYTES))
        else {
            return false;
        };
        if required <= lease.bytes() {
            return true;
        }
        let additional = required - lease.bytes();
        lease.reserve(additional).is_ok()
    }

    /// Decodeまたはparseを省略したsource lineの後で、grammar stateをhunk先頭へ戻す。
    ///
    /// 省略したlineがmultiline string/commentを開閉したかは判定できない。直前stateを継続すると
    /// 未確認の遷移を既知として扱うため、callerは省略したlineが属するold/new sideだけをresetする。
    pub(crate) fn reset_after_skipped_line(&mut self) {
        self.highlighter = HighlightLines::new(self.syntax, self.theme);
    }

    fn same_state(&mut self, other: &mut SyntaxSession<'_>) -> bool {
        let own_state = self.take_state();
        let other_state = other.take_state();
        let same = own_state == other_state;
        self.restore_state(own_state);
        other.restore_state(other_state);
        same
    }

    fn take_state(&mut self) -> SyntaxSessionState {
        let current = replace(
            &mut self.highlighter,
            HighlightLines::new(self.syntax, self.theme),
        );
        let (highlight, parse) = current.state();
        SyntaxSessionState { highlight, parse }
    }

    fn restore_state(&mut self, state: SyntaxSessionState) {
        self.highlighter = HighlightLines::from_state(self.theme, state.highlight, state.parse);
    }
}

fn syntax_set_with_continuation_candidates(
    themes: &ThemeSet,
) -> (
    SyntaxSet,
    HashMap<syntect::parsing::Scope, Vec<ContinuationCandidate>>,
) {
    let base = two_face::syntax::extra_newlines();
    let builder = base.clone().into_builder();
    let mut definitions = builder.syntaxes().to_vec();
    let context_references = definitions
        .iter()
        .flat_map(pushed_context_groups)
        .flat_map(|stack| stack.contexts)
        .collect::<Vec<_>>();
    let mut candidates = HashMap::new();

    for definition in &mut definitions {
        let syntax_candidates =
            add_continuation_markers(definition, &base, &context_references, themes);
        if !syntax_candidates.is_empty() {
            candidates.insert(definition.scope, syntax_candidates);
        }
    }

    let mut rebuilt = SyntaxSetBuilder::new();
    for definition in definitions {
        rebuilt.add(definition);
    }
    (rebuilt.build(), candidates)
}

fn add_continuation_markers(
    definition: &mut SyntaxDefinition,
    syntax_set: &SyntaxSet,
    context_references: &[ContextReference],
    themes: &ThemeSet,
) -> Vec<ContinuationCandidate> {
    let Some(main) = definition.contexts.get("__main") else {
        return Vec::new();
    };
    let Some(main_reference) = reference_to_context(syntax_set, context_references, main) else {
        return Vec::new();
    };

    let mut context_groups = pushed_context_groups(definition)
        .into_iter()
        .filter(|stack| {
            let contexts = stack
                .contexts
                .iter()
                .map(|reference| reference.resolve(syntax_set))
                .collect::<Result<Vec<_>, _>>();
            let Ok(contexts) = contexts else {
                return false;
            };
            !contexts.iter().any(|context| context.uses_backrefs)
                && contexts
                    .last()
                    .is_some_and(|context| context_can_pop(context, syntax_set, 0))
                && context_group_changes_visible_style(definition.scope, &contexts, themes)
        })
        .collect::<Vec<_>>();
    context_groups.sort_by_cached_key(|stack| format!("{stack:?}"));
    context_groups.dedup();
    context_groups.truncate(MAX_CONTINUATION_CANDIDATES);

    let mut marker_patterns = Vec::with_capacity(context_groups.len());
    let mut candidates = Vec::with_capacity(context_groups.len());
    for (index, context_group) in context_groups.into_iter().enumerate() {
        // Sanitized source textはNULを可視表記へ変えるため、このprefixは内部seedと入力行で衝突しない。
        let marker = format!("\0GUP-CONTINUATION-{index:02x}\0");
        let mut seeded_stack = Vec::with_capacity(1 + context_group.contexts.len());
        seeded_stack.push(main_reference.clone());
        seeded_stack.extend(context_group.contexts);
        marker_patterns.push(Pattern::Match(MatchPattern::new(
            false,
            format!(r"\A{}", regex::escape(&marker)),
            Vec::new(),
            None,
            MatchOperation::Push(seeded_stack),
            context_group.with_prototype,
        )));
        candidates.push(ContinuationCandidate { marker });
    }

    if let Some(start) = definition.contexts.get_mut("__start") {
        start.patterns.splice(0..0, marker_patterns);
    } else {
        candidates.clear();
    }
    candidates
}

fn context_group_changes_visible_style(
    syntax_scope: syntect::parsing::Scope,
    contexts: &[&Context],
    themes: &ThemeSet,
) -> bool {
    let scope_count = contexts
        .iter()
        .map(|context| context.meta_scope.len() + context.meta_content_scope.len())
        .sum::<usize>();
    let mut continuation_stack = Vec::with_capacity(1 + scope_count);
    continuation_stack.push(syntax_scope);
    for context in contexts {
        continuation_stack.extend_from_slice(&context.meta_scope);
        continuation_stack.extend_from_slice(&context.meta_content_scope);
    }

    ["base16-ocean.dark", "InspiredGitHub"]
        .into_iter()
        .filter_map(|name| themes.themes.get(name))
        .any(|theme| {
            let highlighter = Highlighter::new(theme);
            let baseline = highlighter.style_for_stack(&[syntax_scope]);
            let continuation = highlighter.style_for_stack(&continuation_stack);
            baseline.foreground != continuation.foreground
                || baseline.font_style != continuation.font_style
        })
}

fn pushed_context_groups(definition: &SyntaxDefinition) -> Vec<ContinuationStack> {
    let mut context_names = definition.contexts.keys().collect::<Vec<_>>();
    context_names.sort_unstable();
    let mut groups = Vec::new();
    for context_name in context_names {
        let context = &definition.contexts[context_name];
        for pattern in &context.patterns {
            if let Pattern::Match(pattern) = pattern
                && let MatchOperation::Push(pushed) | MatchOperation::Set(pushed) =
                    &pattern.operation
                && !pushed.is_empty()
            {
                groups.push(ContinuationStack {
                    contexts: pushed.clone(),
                    with_prototype: pattern.with_prototype.clone(),
                });
            }
        }
    }
    groups
}

fn context_can_pop(context: &Context, syntax_set: &SyntaxSet, depth: usize) -> bool {
    const MAX_INCLUDE_DEPTH: usize = 16;
    if depth == MAX_INCLUDE_DEPTH {
        return false;
    }
    context.patterns.iter().any(|pattern| match pattern {
        Pattern::Match(pattern) => matches!(pattern.operation, MatchOperation::Pop),
        Pattern::Include(reference) => reference
            .resolve(syntax_set)
            .is_ok_and(|included| context_can_pop(included, syntax_set, depth + 1)),
    })
}

fn reference_to_context(
    syntax_set: &SyntaxSet,
    references: &[ContextReference],
    target: &Context,
) -> Option<ContextReference> {
    references
        .iter()
        .find(|reference| {
            reference
                .resolve(syntax_set)
                .is_ok_and(|context| context == target)
        })
        .cloned()
}

fn highlighted_lines_equivalent(left: &HighlightedLine, right: &HighlightedLine) -> bool {
    match (left, right) {
        (HighlightedLine::Plain, HighlightedLine::Plain) => true,
        (HighlightedLine::Styled(left), HighlightedLine::Styled(right)) => {
            syntax_spans_equivalent(left, right)
        }
        _ => false,
    }
}

pub(crate) fn syntax_spans_equivalent(left: &[SyntaxSpan], right: &[SyntaxSpan]) -> bool {
    if left.is_empty() || right.is_empty() {
        return left.is_empty() && right.is_empty();
    }
    if left[0].byte_range.start != right[0].byte_range.start {
        return false;
    }
    let mut left_index = 0_usize;
    let mut right_index = 0_usize;
    let mut position = left[0].byte_range.start;

    while let (Some(left_span), Some(right_span)) = (left.get(left_index), right.get(right_index)) {
        if left_span.byte_range.start > position
            || right_span.byte_range.start > position
            || left_span.foreground != right_span.foreground
            || left_span.bold != right_span.bold
            || left_span.italic != right_span.italic
            || left_span.underline != right_span.underline
        {
            return false;
        }
        position = left_span.byte_range.end.min(right_span.byte_range.end);
        if left_span.byte_range.end == position {
            left_index += 1;
        }
        if right_span.byte_range.end == position {
            right_index += 1;
        }
    }

    left_index == left.len() && right_index == right.len()
}

fn syntax_workspace_upper_bound(line_len: usize) -> Option<usize> {
    const PARSER_BYTES_PER_INPUT_BYTE: usize = 32;
    let per_input = size_of::<SyntaxSpan>()
        .checked_add(size_of::<(Style, &str)>())?
        .checked_add(PARSER_BYTES_PER_INPUT_BYTE)?;
    line_len
        .max(1)
        .checked_mul(per_input)?
        .checked_add(line_len.checked_add(1)?.checked_mul(2)?)
        .and_then(|bytes| bytes.checked_add(64 * 1024))
}

fn detect_syntax<'a>(
    syntaxes: &'a SyntaxSet,
    path: Option<&str>,
    first_line: Option<&str>,
) -> Option<&'a syntect::parsing::SyntaxReference> {
    let path_match = path.and_then(|path| {
        let path = Path::new(path);
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        // fancy-regex では JavaScript (Babel) grammar が除外されるため、JSX は JSX を内包する
        // TypeScriptReact grammar へ割り当てる。TSX と同じ parse state を使うことで、タグと
        // JavaScript expression の境界を plain JavaScript grammar より細かく保持できる。
        let jsx = extension
            .eq_ignore_ascii_case("jsx")
            .then(|| syntaxes.find_syntax_by_name("TypeScriptReact"))
            .flatten();
        jsx.or_else(|| syntaxes.find_syntax_by_extension(file_name))
            .or_else(|| syntaxes.find_syntax_by_extension(extension))
    });
    path_match.or_else(|| {
        first_line
            .filter(|line| line.len() <= MAX_SYNTAX_LINE_BYTES && line.starts_with("#!"))
            .and_then(|line| syntaxes.find_syntax_by_first_line(line))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_detection_does_not_read_repository_files() {
        let syntaxes = two_face::syntax::extra_newlines();
        let syntax = detect_syntax(&syntaxes, Some("not/on/disk/main.rs"), None).unwrap();
        assert_eq!(syntax.name, "Rust");
    }

    #[test]
    fn detects_swift_typescript_and_jsx_family_extensions() {
        let syntaxes = two_face::syntax::extra_newlines();
        let cases = [
            ("main.swift", "Swift"),
            ("main.ts", "TypeScript"),
            ("component.tsx", "TypeScriptReact"),
            ("component.jsx", "TypeScriptReact"),
        ];

        for (path, expected) in cases {
            let syntax = detect_syntax(&syntaxes, Some(path), None)
                .unwrap_or_else(|| panic!("{path} should have a syntax"));
            assert_eq!(syntax.name, expected, "unexpected syntax for {path}");
        }
    }

    #[test]
    fn shebang_is_used_only_as_fallback() {
        let syntaxes = two_face::syntax::extra_newlines();
        let syntax =
            detect_syntax(&syntaxes, Some("script"), Some("#!/usr/bin/env python3")).unwrap();
        assert!(syntax.name.contains("Python"));
    }

    #[test]
    fn non_shebang_first_line_does_not_guess_a_syntax() {
        let syntaxes = two_face::syntax::extra_newlines();
        assert!(
            detect_syntax(&syntaxes, Some("unknown"), Some("<?xml version=\"1.0\"?>")).is_none()
        );
    }

    #[test]
    fn oversized_lines_fall_back_to_plain() {
        let engine = SyntaxEngine::new().unwrap();
        let request = SyntaxRequest {
            generation: 1,
            request_id: 1,
            path: Some("large.rs".to_owned()),
            first_line: None,
            lines: Arc::from(["x".repeat(MAX_SYNTAX_LINE_BYTES + 1)]),
            theme: ThemeChoice::Dark,
        };
        assert_eq!(
            engine.highlight(&request).unwrap(),
            vec![HighlightedLine::Plain]
        );
    }

    #[test]
    fn oversized_line_resets_unknown_parse_state() {
        let engine = SyntaxEngine::new().unwrap();
        let request = SyntaxRequest {
            generation: 1,
            request_id: 1,
            path: Some("large.rs".to_owned()),
            first_line: None,
            lines: Arc::from([
                "/* open comment".to_owned(),
                "x".repeat(MAX_SYNTAX_LINE_BYTES + 1),
                "let value = 1;".to_owned(),
            ]),
            theme: ThemeChoice::Dark,
        };
        let output = engine.highlight(&request).unwrap();

        let standalone = SyntaxRequest {
            generation: 2,
            request_id: 2,
            path: Some("large.rs".to_owned()),
            first_line: None,
            lines: Arc::from(["let value = 1;".to_owned()]),
            theme: ThemeChoice::Dark,
        };
        assert_eq!(output[2], engine.highlight(&standalone).unwrap()[0]);
    }

    #[test]
    fn synthetic_newline_closes_single_line_context() {
        let engine = SyntaxEngine::new().unwrap();
        let request = SyntaxRequest {
            generation: 1,
            request_id: 1,
            path: Some("main.rs".to_owned()),
            first_line: None,
            lines: Arc::from(["// comment".to_owned(), "let value = 1;".to_owned()]),
            theme: ThemeChoice::Dark,
        };
        let output = engine.highlight(&request).unwrap();
        let standalone = SyntaxRequest {
            generation: 2,
            request_id: 2,
            path: Some("main.rs".to_owned()),
            first_line: None,
            lines: Arc::from(["let value = 1;".to_owned()]),
            theme: ThemeChoice::Dark,
        };
        assert_eq!(output[1], engine.highlight(&standalone).unwrap()[0]);
        let HighlightedLine::Styled(first_spans) = &output[0] else {
            panic!("expected styled comment");
        };
        let HighlightedLine::Styled(second_spans) = &output[1] else {
            panic!("expected styled source");
        };
        assert!(
            first_spans
                .iter()
                .all(|span| span.byte_range.end <= "// comment".len())
        );
        assert!(
            second_spans
                .iter()
                .all(|span| span.byte_range.end <= "let value = 1;".len())
        );
    }

    #[test]
    fn budgeted_session_keeps_state_and_output_charged_until_each_owner_drops() {
        let engine = SyntaxEngine::new().unwrap();
        let ledger = DerivedMemoryLedger::new();
        let line = "let value = 1;";
        let mut session = engine
            .session_with_budget(Some("src/main.rs"), Some(line), ThemeChoice::Dark, &ledger)
            .unwrap()
            .unwrap();
        assert_eq!(
            ledger
                .snapshot()
                .reserved_bytes(DerivedMemoryPool::DecorationWorkspace),
            SYNTAX_STATE_BASE_BYTES
        );

        let output = session
            .highlight_line_with_budget(line, &ledger)
            .unwrap()
            .unwrap();
        let state_bytes =
            SYNTAX_STATE_BASE_BYTES + line.len() * SyntaxSession::STATE_BYTES_PER_INPUT_BYTE;
        assert!(
            ledger
                .snapshot()
                .reserved_bytes(DerivedMemoryPool::DecorationWorkspace)
                > state_bytes
        );

        drop(output);
        assert_eq!(
            ledger
                .snapshot()
                .reserved_bytes(DerivedMemoryPool::DecorationWorkspace),
            state_bytes
        );
        drop(session);
        assert_eq!(ledger.snapshot().total_reserved_bytes(), Some(0));
    }

    #[test]
    fn syntax_session_fails_before_construction_when_workspace_is_full() {
        let engine = SyntaxEngine::new().unwrap();
        let ledger = DerivedMemoryLedger::new();
        let held_bytes =
            DerivedMemoryPool::DecorationWorkspace.limit_bytes() - SYNTAX_STATE_BASE_BYTES + 1;
        let held = ledger
            .reserve(DerivedMemoryPool::DecorationWorkspace, held_bytes)
            .unwrap();

        assert!(matches!(
            engine.session_with_budget(
                Some("src/main.rs"),
                Some("fn main() {}"),
                ThemeChoice::Dark,
                &ledger,
            ),
            Err(HighlightError::WorkspaceBudgetExceeded)
        ));
        assert_eq!(
            ledger
                .snapshot()
                .reserved_bytes(DerivedMemoryPool::DecorationWorkspace),
            held_bytes
        );
        drop(held);
        assert_eq!(ledger.snapshot().total_reserved_bytes(), Some(0));
    }

    #[test]
    fn unknown_syntax_drops_the_precharged_state_lease() {
        let engine = SyntaxEngine::new().unwrap();
        let ledger = DerivedMemoryLedger::new();
        assert!(
            engine
                .session_with_budget(
                    Some("unknown.no-such-syntax"),
                    None,
                    ThemeChoice::Dark,
                    &ledger,
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(ledger.snapshot().total_reserved_bytes(), Some(0));
    }
}
