//! File type detection と syntax highlighting actor。
//!
//! `syntect::ParseState` / `HighlightState` は worker 間を移動させず、専用 actor thread が所有する。
//! AppState へ返すのは thread-independent な byte span と RGB/style bit だけである。diff background、
//! search、intraline の precedence は renderer が合成し、この module は source foreground だけを
//! 提供する。Built-in `SyntaxSet`/`ThemeSet`はworker lifetimeのimmutable program assetであり、256 MiB
//! derived-data partitionには含めない。Requestごとのparse state、synthetic line、region/span Vecだけを
//! `DecorationWorkspace`へ計上する。

use std::cmp::Ordering;
use std::collections::HashMap;
use std::mem::size_of;
use std::ops::Range;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::thread;

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style, ThemeSet};
use syntect::parsing::{
    ParseState, Regex as SyntaxRegex, Region as SyntaxRegion, Scope, ScopeStack, SyntaxReference,
    SyntaxSet,
};
use thiserror::Error;

use crate::cli::ThemeChoice;
use crate::derived_memory::{DerivedMemoryLease, DerivedMemoryLedger, DerivedMemoryPool};
use crate::text::MAX_SYNTAX_LINE_BYTES;

const SYNTAX_STATE_BASE_BYTES: usize = 64 * 1024;

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

/// Build時に内部seed entryを追加したprocess-wide asset。
///
/// Seedはsanitized sourceに現れないNUL prefixでだけ到達でき、通常sessionのgrammar動作を変えない。
/// Runtimeは生成済みdumpを1回deserializeし、actorと同期decoratorはimmutable setを共有する。
struct SyntaxAssets {
    syntaxes: SyntaxSet,
    themes: ThemeSet,
    continuation_candidates: HashMap<Scope, Vec<ContinuationCandidate>>,
}

type SerializedContinuationCandidate = (String, Vec<String>, Vec<String>);
type SerializedContinuationCandidates = HashMap<Scope, Vec<SerializedContinuationCandidate>>;
type GeneratedSyntaxAsset = (SyntaxSet, SerializedContinuationCandidates);

struct ContinuationCandidate {
    marker: String,
    opener_patterns: Vec<SyntaxRegex>,
    exit_patterns: Vec<SyntaxRegex>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExitEvidence {
    start: usize,
    consumed: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ContinuationScore {
    convergence_line: usize,
    exit: ExitEvidence,
    opener_consumed: usize,
}

impl ContinuationScore {
    /// 長いdelimiterを先に選び、同じ消費長なら早く通常stateへ戻る候補を選ぶ。
    /// `* `prefixではsyntheticな`/**`を長く消費するopenerを優先し、JSDoc固有の
    /// markupを通常block commentへ丸めない。残りも同じ場合は行内の早いexitを選ぶ。
    fn strength_cmp(self, other: Self) -> Ordering {
        self.exit
            .consumed
            .cmp(&other.exit.consumed)
            .then_with(|| other.convergence_line.cmp(&self.convergence_line))
            .then_with(|| self.opener_consumed.cmp(&other.opener_consumed))
            .then_with(|| other.exit.start.cmp(&self.exit.start))
    }
}

impl ExitEvidence {
    /// 同じ行で複数contextが閉じる場合は、delimiterを長く消費するregexを強い証拠とする。
    /// 消費長も同じなら早い位置のmatchを選び、別delimiter内のsuffixだけに一致した候補を落とす。
    fn strength_cmp(self, other: Self) -> Ordering {
        self.consumed
            .cmp(&other.consumed)
            .then_with(|| other.start.cmp(&self.start))
    }
}

impl ContinuationCandidate {
    /// 同じstackをpush/setするgrammar規則がvisible source内で成立するかを返す。
    fn has_visible_opener(&self, lines: &[String], line_with_newline: &mut String) -> bool {
        lines
            .iter()
            .any(|line| self.opener_matches(line, line_with_newline))
    }

    fn opener_matches(&self, line: &str, line_with_newline: &mut String) -> bool {
        line_with_newline.clear();
        line_with_newline.push_str(line);
        line_with_newline.push('\n');
        self.opener_patterns
            .iter()
            .any(|pattern| pattern.search(line_with_newline, 0, line_with_newline.len(), None))
    }

    fn opener_evidence(
        &self,
        line: &str,
        line_with_newline: &mut String,
        region: &mut SyntaxRegion,
    ) -> Option<ExitEvidence> {
        line_with_newline.clear();
        line_with_newline.push_str(line);
        line_with_newline.push('\n');
        strongest_pattern_evidence(&self.opener_patterns, line_with_newline, region)
    }

    /// `* `だけが続くprefix用に、grammar上のblock-comment contextかをsynthetic delimiterで判定する。
    fn is_block_comment(&self, line_with_newline: &mut String, region: &mut SyntaxRegion) -> bool {
        ["/**", "/*"]
            .into_iter()
            .any(|opener| self.opener_matches(opener, line_with_newline))
            && self
                .exit_evidence(" */", line_with_newline, region)
                .is_some_and(|evidence| evidence.consumed >= 2)
    }

    fn exit_evidence(
        &self,
        line: &str,
        line_with_newline: &mut String,
        region: &mut SyntaxRegion,
    ) -> Option<ExitEvidence> {
        line_with_newline.clear();
        line_with_newline.push_str(line);
        line_with_newline.push('\n');
        strongest_pattern_evidence(&self.exit_patterns, line_with_newline, region)
    }

    fn last_exit_line(
        &self,
        lines: &[String],
        line_with_newline: &mut String,
        region: &mut SyntaxRegion,
    ) -> Option<usize> {
        if self.exit_patterns.is_empty() {
            return None;
        }
        let (first, remaining) = lines.split_first()?;
        // 先頭行でexit regexが成立する候補は、missing openerの継続より通常の一時contextである
        // 可能性が高い。後続行だけにcloserがある候補へ絞ると、1行目だけで戻る候補をparse前に除ける。
        if self
            .exit_evidence(first, line_with_newline, region)
            .is_some()
        {
            return None;
        }
        remaining
            .iter()
            .enumerate()
            .filter_map(|(index, line)| {
                self.exit_evidence(line, line_with_newline, region)
                    .map(|_| index + 1)
            })
            .last()
    }
}

fn is_unclosed_comment_leader_prefix(lines: &[String]) -> bool {
    is_star_comment_prefix(lines) && lines.iter().all(|line| line.trim_start() != "*/")
}

fn is_star_comment_prefix(lines: &[String]) -> bool {
    lines.len() >= 2
        && lines.iter().all(|line| {
            let trimmed = line.trim_start();
            trimmed == "*" || trimmed == "*/" || trimmed.starts_with("* ")
        })
}

fn strongest_pattern_evidence(
    patterns: &[SyntaxRegex],
    line_with_newline: &str,
    region: &mut SyntaxRegion,
) -> Option<ExitEvidence> {
    let mut strongest = None;
    for pattern in patterns {
        if pattern.search(line_with_newline, 0, line_with_newline.len(), Some(region))
            && let Some((start, end)) = region.pos(0)
        {
            let evidence = ExitEvidence {
                start,
                consumed: end.saturating_sub(start),
            };
            if strongest.is_none_or(|current| evidence.strength_cmp(current) == Ordering::Greater) {
                strongest = Some(evidence);
            }
        }
    }
    strongest
}

fn exit_is_partial_comment_delimiter(line: &str, evidence: ExitEvidence) -> bool {
    let Some(end) = evidence.start.checked_add(evidence.consumed) else {
        return true;
    };
    line.get(evidence.start..end) == Some("/")
        && (line
            .get(end..)
            .is_some_and(|suffix| suffix.starts_with('/'))
            || line
                .get(..evidence.start)
                .is_some_and(|prefix| prefix.ends_with('*')))
}

impl SyntaxEngine {
    pub(crate) fn new() -> Result<Self, HighlightError> {
        static ASSETS: OnceLock<Result<Arc<SyntaxAssets>, HighlightError>> = OnceLock::new();
        let assets = ASSETS
            .get_or_init(|| {
                let themes = ThemeSet::load_defaults();
                let (syntaxes, serialized_candidates): GeneratedSyntaxAsset =
                    syntect::dumps::from_uncompressed_data(include_bytes!(concat!(
                        env!("OUT_DIR"),
                        "/syntax-with-continuations.packdump"
                    )))
                    .map_err(|error| HighlightError::Syntect(error.to_string()))?;
                let continuation_candidates = serialized_candidates
                    .into_iter()
                    .map(|(scope, candidates)| {
                        let candidates = candidates
                            .into_iter()
                            .map(
                                |(marker, opener_patterns, exit_patterns)| ContinuationCandidate {
                                    marker,
                                    opener_patterns: opener_patterns
                                        .into_iter()
                                        .map(SyntaxRegex::new)
                                        .collect(),
                                    exit_patterns: exit_patterns
                                        .into_iter()
                                        .map(SyntaxRegex::new)
                                        .collect(),
                                },
                            )
                            .collect();
                        (scope, candidates)
                    })
                    .collect();
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
    /// 候補は改行後も残り、prefix内で通常stateへ合流するcontextに限る。終了regexが消費する
    /// delimiter長、合流位置の順で候補を絞る。`* `prefixでは`/**`に対するopener regexの
    /// 消費長も使い、JSDocと通常block commentを区別する。同じ強さの候補が異なるspanを返す場合は
    /// 入力だけでは一意に決められないため、通常stateのsessionを返す。closerがない`* `prefixも、
    /// grammarのblock-comment候補のうち同じ基準で最も具体的な候補を採用する。
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
        let filter_line_bytes = lines
            .iter()
            .map(String::len)
            .max()
            .unwrap_or(0)
            .max(3)
            .checked_add(1)?;
        let _filter_line_lease = ledger
            .reserve(DerivedMemoryPool::DecorationWorkspace, filter_line_bytes)
            .ok()?;
        let candidate_entry_bytes =
            size_of::<(&ContinuationCandidate, usize)>().checked_add(size_of::<&str>())?;
        // `SyntaxRegion::new`はOnigurumaのcapture 8個分を確保する。候補Vec 2本と合わせて
        // request固有allocationを生成前にDecorationWorkspaceへ計上する。
        let exit_region_bytes = size_of::<SyntaxRegion>()
            .checked_add(8_usize.checked_mul(2)?.checked_mul(size_of::<i32>())?)?;
        let candidate_workspace_bytes = candidates
            .len()
            .checked_mul(candidate_entry_bytes)?
            .checked_add(exit_region_bytes)?;
        let _candidate_workspace_lease = ledger
            .reserve(
                DerivedMemoryPool::DecorationWorkspace,
                candidate_workspace_bytes,
            )
            .ok()?;
        let mut filter_line = String::with_capacity(filter_line_bytes);
        let mut exit_region = SyntaxRegion::new();
        let mut eligible_candidates = Vec::with_capacity(candidates.len());
        eligible_candidates.extend(candidates.iter().filter_map(|candidate| {
            candidate
                .last_exit_line(lines, &mut filter_line, &mut exit_region)
                .map(|last_exit| (candidate, last_exit))
        }));
        let mut best_score = None;
        let mut best_visible_score = None;
        let mut selected_markers = Vec::with_capacity(candidates.len());
        let star_comment_prefix = is_star_comment_prefix(lines);
        let unclosed_comment_fallback = is_unclosed_comment_leader_prefix(lines);
        if eligible_candidates.is_empty() && !unclosed_comment_fallback {
            return None;
        }

        let max_probe_line = if unclosed_comment_fallback {
            lines.len().checked_sub(1)?
        } else {
            eligible_candidates
                .iter()
                .map(|(_, last_exit)| *last_exit)
                .max()?
        };
        let probe_lines = &lines[..=max_probe_line];
        let max_line_bytes = probe_lines.iter().map(String::len).max().unwrap_or(0);
        let state_bytes = SYNTAX_STATE_BASE_BYTES
            .checked_add(
                max_line_bytes.checked_mul(SyntaxProbeSession::STATE_BYTES_PER_INPUT_BYTE)?,
            )?
            .checked_mul(probe_lines.len().checked_add(1)?)?
            .checked_add(
                probe_lines
                    .len()
                    .checked_mul(size_of::<SyntaxProbeState>())?,
            )?;
        let _baseline_states_lease = ledger
            .reserve(DerivedMemoryPool::DecorationWorkspace, state_bytes)
            .ok()?;
        let (baseline_seed, baseline_states) = {
            let mut baseline = self.probe_session(syntax, ledger)?;
            baseline.advance_with_budget("", ledger).ok()??;
            let seed = baseline.snapshot();
            let mut states = Vec::with_capacity(probe_lines.len());
            for line in probe_lines {
                baseline.advance_with_budget(line, ledger).ok()??;
                states.push(baseline.snapshot());
            }
            (seed, states)
        };

        if unclosed_comment_fallback {
            let mut best_opener_consumed = None;
            for candidate in candidates {
                if !candidate.is_block_comment(&mut filter_line, &mut exit_region)
                    || candidate.has_visible_opener(lines, &mut filter_line)
                {
                    continue;
                }
                let mut inferred = self.probe_session(syntax, ledger)?;
                inferred
                    .advance_with_budget(&candidate.marker, ledger)
                    .ok()??;
                if inferred.same_state(&baseline_seed) {
                    continue;
                }
                let mut converged = false;
                for (line_index, line) in lines.iter().enumerate() {
                    inferred.advance_with_budget(line, ledger).ok()??;
                    if inferred.same_state(&baseline_states[line_index]) {
                        converged = true;
                        break;
                    }
                }
                if !converged {
                    let opener_consumed = candidate
                        .opener_evidence("/**", &mut filter_line, &mut exit_region)
                        .map_or(0, |evidence| evidence.consumed);
                    match best_opener_consumed.map(|current| opener_consumed.cmp(&current)) {
                        Some(Ordering::Less) => {}
                        Some(Ordering::Equal) => {
                            selected_markers.push(candidate.marker.as_str());
                        }
                        Some(Ordering::Greater) | None => {
                            best_opener_consumed = Some(opener_consumed);
                            selected_markers.clear();
                            selected_markers.push(candidate.marker.as_str());
                        }
                    }
                }
            }
            if !selected_markers.is_empty() {
                return self.equivalent_inference_marker(
                    syntax,
                    theme,
                    &selected_markers,
                    lines,
                    ledger,
                );
            }
        }

        for (candidate, last_exit) in &eligible_candidates {
            let mut inferred = self.probe_session(syntax, ledger)?;
            inferred
                .advance_with_budget(&candidate.marker, ledger)
                .ok()??;
            if inferred.same_state(&baseline_seed) {
                continue;
            }

            let mut convergence_line = None;
            for (line_index, line) in lines[..=*last_exit].iter().enumerate() {
                inferred.advance_with_budget(line, ledger).ok()??;
                if inferred.same_state(&baseline_states[line_index]) {
                    convergence_line = Some(line_index);
                    break;
                }
            }
            let Some(convergence_line) = convergence_line else {
                continue;
            };
            // 先頭行だけで戻るcontextは引数・型・groupなどにも多く、欠けたprefixの証拠にならない。
            // 2行以上を同じstateで説明できる候補だけに絞り、通常コードの誤着色を避ける。
            if convergence_line == 0 {
                continue;
            }
            let Some(exit_evidence) = candidate.exit_evidence(
                &lines[convergence_line],
                &mut filter_line,
                &mut exit_region,
            ) else {
                continue;
            };
            // `/`単独のexitが`//`や`*/`の一部に一致した場合、そのdelimiterは別contextが所有する。
            // regex literal候補などを採用すると、delimiterより前の通常コードを誤って継続扱いする。
            if exit_is_partial_comment_delimiter(&lines[convergence_line], exit_evidence) {
                continue;
            }
            let score = ContinuationScore {
                convergence_line,
                exit: exit_evidence,
                opener_consumed: if star_comment_prefix {
                    candidate
                        .opener_evidence("/**", &mut filter_line, &mut exit_region)
                        .map_or(0, |evidence| evidence.consumed)
                } else {
                    0
                },
            };
            // 候補と同じcontextを開くvisible openerがあれば、そのopenerだけでcloserを説明できる。
            // 同じか弱い終了証拠へfallbackすると別contextで前半を誤着色するため、scoreも保持する。
            if candidate.has_visible_opener(&lines[..=convergence_line], &mut filter_line) {
                if best_visible_score
                    .is_none_or(|current| score.strength_cmp(current) == Ordering::Greater)
                {
                    best_visible_score = Some(score);
                }
                continue;
            }
            match best_score.map(|current| score.strength_cmp(current)) {
                Some(Ordering::Less) => {}
                Some(Ordering::Equal) => {
                    selected_markers.push(candidate.marker.as_str());
                }
                Some(Ordering::Greater) | None => {
                    best_score = Some(score);
                    selected_markers.clear();
                    selected_markers.push(candidate.marker.as_str());
                }
            }
        }

        let best_score = best_score?;
        if best_visible_score
            .is_some_and(|visible| visible.strength_cmp(best_score) != Ordering::Less)
        {
            return None;
        }
        let convergence_line = best_score.convergence_line;
        self.equivalent_inference_marker(
            syntax,
            theme,
            &selected_markers,
            &lines[..=convergence_line],
            ledger,
        )
    }

    fn equivalent_inference_marker(
        &self,
        syntax: &SyntaxReference,
        theme: &syntect::highlighting::Theme,
        markers: &[&str],
        lines: &[String],
        ledger: &DerivedMemoryLedger,
    ) -> Option<String> {
        let first_marker = *markers.first()?;
        if markers.len() == 1 {
            return Some(first_marker.to_owned());
        }

        let output_vec_bytes = lines
            .len()
            .checked_mul(size_of::<BudgetedHighlightedLine>())?
            .checked_mul(2)?;
        let _output_vec_lease = ledger
            .reserve(DerivedMemoryPool::DecorationWorkspace, output_vec_bytes)
            .ok()?;
        let expected =
            self.inference_highlight_output(syntax, theme, first_marker, lines, ledger)?;
        for marker in markers.iter().copied().skip(1) {
            let actual = self.inference_highlight_output(syntax, theme, marker, lines, ledger)?;
            if actual.len() != expected.len()
                || actual.iter().zip(&expected).any(|(actual, expected)| {
                    !highlighted_lines_equivalent(actual.line(), expected.line())
                })
            {
                return None;
            }
        }
        Some(first_marker.to_owned())
    }

    fn probe_session<'engine>(
        &'engine self,
        syntax: &'engine SyntaxReference,
        ledger: &DerivedMemoryLedger,
    ) -> Option<SyntaxProbeSession<'engine>> {
        let lease = ledger
            .reserve(
                DerivedMemoryPool::DecorationWorkspace,
                SYNTAX_STATE_BASE_BYTES,
            )
            .ok()?;
        Some(SyntaxProbeSession::new(
            &self.assets.syntaxes,
            syntax,
            lease,
        ))
    }

    fn inference_highlight_output<'engine>(
        &'engine self,
        syntax: &'engine SyntaxReference,
        theme: &'engine syntect::highlighting::Theme,
        marker: &str,
        lines: &[String],
        ledger: &DerivedMemoryLedger,
    ) -> Option<Vec<BudgetedHighlightedLine>> {
        let lease = ledger
            .reserve(
                DerivedMemoryPool::DecorationWorkspace,
                SYNTAX_STATE_BASE_BYTES,
            )
            .ok()?;
        let mut session = SyntaxSession::new(&self.assets.syntaxes, syntax, theme, Some(lease));
        drop(session.highlight_line_with_budget(marker, ledger).ok()??);
        lines
            .iter()
            .map(|line| session.highlight_line_with_budget(line, ledger).ok()?)
            .collect()
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

/// 候補stateの合流判定だけを行い、theme lookupと`SyntaxSpan`生成を持たないparse session。
struct SyntaxProbeSession<'engine> {
    syntaxes: &'engine SyntaxSet,
    parse_state: ParseState,
    scope_stack: ScopeStack,
    state_lease: DerivedMemoryLease,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SyntaxProbeState {
    parse_state: ParseState,
    scope_stack: ScopeStack,
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
}

impl<'engine> SyntaxProbeSession<'engine> {
    const STATE_BYTES_PER_INPUT_BYTE: usize = 8;

    fn new(
        syntaxes: &'engine SyntaxSet,
        syntax: &'engine SyntaxReference,
        state_lease: DerivedMemoryLease,
    ) -> Self {
        Self {
            syntaxes,
            parse_state: ParseState::new(syntax),
            scope_stack: ScopeStack::new(),
            state_lease,
        }
    }

    fn advance_with_budget(
        &mut self,
        line: &str,
        ledger: &DerivedMemoryLedger,
    ) -> Result<Option<()>, HighlightError> {
        if line.len() > MAX_SYNTAX_LINE_BYTES || !self.ensure_state_budget(line.len()) {
            return Ok(None);
        }
        let Some(workspace_bytes) = syntax_workspace_upper_bound(line.len()) else {
            return Ok(None);
        };
        let _workspace =
            match ledger.reserve(DerivedMemoryPool::DecorationWorkspace, workspace_bytes) {
                Ok(lease) => lease,
                Err(_) => return Ok(None),
            };
        let mut line_with_newline = String::with_capacity(line.len() + 1);
        line_with_newline.push_str(line);
        line_with_newline.push('\n');
        let operations = self
            .parse_state
            .parse_line(&line_with_newline, self.syntaxes)
            .map_err(|error| HighlightError::Syntect(error.to_string()))?;
        for (_, operation) in operations {
            self.scope_stack
                .apply(&operation)
                .map_err(|error| HighlightError::Syntect(error.to_string()))?;
        }
        Ok(Some(()))
    }

    fn snapshot(&self) -> SyntaxProbeState {
        SyntaxProbeState {
            parse_state: self.parse_state.clone(),
            scope_stack: self.scope_stack.clone(),
        }
    }

    fn same_state(&self, other: &SyntaxProbeState) -> bool {
        self.parse_state == other.parse_state && self.scope_stack == other.scope_stack
    }

    fn ensure_state_budget(&mut self, line_len: usize) -> bool {
        let Some(required) = line_len
            .checked_mul(Self::STATE_BYTES_PER_INPUT_BYTE)
            .and_then(|bytes| bytes.checked_add(SYNTAX_STATE_BASE_BYTES))
        else {
            return false;
        };
        if required <= self.state_lease.bytes() {
            return true;
        }
        self.state_lease
            .reserve(required - self.state_lease.bytes())
            .is_ok()
    }
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
        // JSX は JSX を内包する TypeScriptReact grammar へ割り当てる。TSX と同じ parse state
        // を使うことで、タグと
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
        let engine = SyntaxEngine::new().unwrap();
        let syntax =
            detect_syntax(&engine.assets.syntaxes, Some("not/on/disk/main.rs"), None).unwrap();
        assert_eq!(syntax.name, "Rust");
    }

    #[test]
    fn detects_swift_typescript_and_jsx_family_extensions() {
        let engine = SyntaxEngine::new().unwrap();
        let syntaxes = &engine.assets.syntaxes;
        let cases = [
            ("main.swift", "Swift"),
            ("main.ts", "TypeScript"),
            ("component.tsx", "TypeScriptReact"),
            ("component.jsx", "TypeScriptReact"),
        ];

        for (path, expected) in cases {
            let syntax = detect_syntax(syntaxes, Some(path), None)
                .unwrap_or_else(|| panic!("{path} should have a syntax"));
            assert_eq!(syntax.name, expected, "unexpected syntax for {path}");
        }
    }

    #[test]
    fn shebang_is_used_only_as_fallback() {
        let engine = SyntaxEngine::new().unwrap();
        let syntax = detect_syntax(
            &engine.assets.syntaxes,
            Some("script"),
            Some("#!/usr/bin/env python3"),
        )
        .unwrap();
        assert!(syntax.name.contains("Python"));
    }

    #[test]
    fn non_shebang_first_line_does_not_guess_a_syntax() {
        let engine = SyntaxEngine::new().unwrap();
        assert!(
            detect_syntax(
                &engine.assets.syntaxes,
                Some("unknown"),
                Some("<?xml version=\"1.0\"?>"),
            )
            .is_none()
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
