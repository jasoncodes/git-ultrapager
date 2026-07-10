//! Adjacent deletion/addition block の grapheme-level diff。
//!
//! Git の `-`/`+` 行を隠さず、対応付けと強調 span だけを生成する。block の line alignment は
//! whitespace policy 適用後の文字列へ Patience diff を使い、anchor 間の unmatched 行を入力順に
//! pair する。worker が budget 超過を検出した場合は部分結果を返さず block 全体を fallback する。

use std::mem::size_of;
use std::ops::Range;

use similar::{Algorithm, DiffOp, capture_diff_slices};
use unicode_segmentation::UnicodeSegmentation;

use crate::cli::WhitespaceChoice;
use crate::derived_memory::{
    DECORATION_WORKSPACE_MEMORY_BYTES, DerivedMemoryLease, DerivedMemoryLedger, DerivedMemoryPool,
};
use crate::text::MAX_GRAPHEME_BYTES;

/// 1 change block に含める削除・追加行の合計上限。
pub const MAX_CHANGED_LINES: usize = 5_000;
/// 1 logical line の grapheme 上限。
pub const MAX_LINE_GRAPHEMES: usize = 20_000;
/// 1 change block の UTF-8 byte 上限。
pub const MAX_BLOCK_BYTES: usize = 64 * 1024 * 1024;
/// 1 change block の grapheme 合計上限。
pub const MAX_BLOCK_GRAPHEMES: usize = 10_000_000;
/// 1 natural grapheme の normalization mapping が占有できる byte 上限。
///
/// Ignore mode は非whitespace scalarごとに [`MappingPiece`] を1つ作る。Combining markだけを多数連結
/// した1 graphemeがline/grapheme件数capをすり抜けても、1 clusterだけで一時Vecを増やさないよう、
/// mapping recordの合計を表示側のgrapheme byte上限と同じ64 KiBで止める。
pub const MAX_MAPPING_BYTES_PER_GRAPHEME: usize = MAX_GRAPHEME_BYTES;

// `similar` の Myers/Patience は入力長に比例するvectorとhash bucketを内部で作る。公開APIからその
// capacityを取得できないため、入力1要素あたりこのbyte数を事前予約へ加える。64-bit targetで
// DiffOp、hash key、bucket、index vector、Myersのforward/backward vectorを同時に持つ場合より大きい。
const ALGORITHM_BYTES_PER_ELEMENT: usize = 256;
const VEC_GROWTH_FACTOR: usize = 2;

/// 1 old/new line pair の強調範囲。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinePairDiff {
    /// Change block 内の削除側index。
    pub old_index: usize,
    /// Change block 内の追加側index。
    pub new_index: usize,
    /// 削除側original UTF-8 textへのchanged byte range。
    pub old_changed: Vec<Range<usize>>,
    /// 追加側original UTF-8 textへのchanged byte range。
    pub new_changed: Vec<Range<usize>>,
}

/// Intraline を省略した deterministic な理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    TooManyLines,
    LineTooLong,
    BlockTooLarge,
    TooManyGraphemes,
    /// 1 natural grapheme が64 KiBを超え、mapping前のbounded unitにできない。
    NaturalGraphemeTooLarge,
    /// Ignore modeの1 graphemeから作るscalar mapping recordが64 KiBを超える。
    MappingClusterTooLarge,
    /// Capacityの乗算または加算が`usize`へ収まらない。
    WorkspaceEstimateOverflow,
    /// 見積もりが96 MiBを超えるか、同じsessionの別計算がworkspaceを占有している。
    WorkspaceBudgetExceeded,
}

/// Allocation前の走査で得たintraline workspace上限。
///
/// `alignment_peak_bytes` はblock全lineのnormalized textとPatience diff、`pairing_peak_bytes` は
/// output rangeの最大保持量と1 old/new pairのMyers diffを表す。両phaseの間でnormalized line Vecを
/// dropするため、leaseする値は合計ではなく大きい側になる。各値は`Vec`のgrowth余白と`similar`の
/// 非公開working setを含む上限であり、入力文字列そのものはcallerが既に所有するため含めない。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntralineWorkspaceEstimate {
    alignment_peak_bytes: usize,
    pairing_peak_bytes: usize,
    workspace_bytes: usize,
    decoded_input_bytes: usize,
    scalar_count: usize,
    grapheme_count: usize,
    mapping_piece_count: usize,
}

impl IntralineWorkspaceEstimate {
    /// Normalized line alignment phaseへ予約する上限。
    #[must_use]
    pub fn alignment_peak_bytes(self) -> usize {
        self.alignment_peak_bytes
    }

    /// Pairごとのmappingとgrapheme diff phaseへ予約する上限。
    #[must_use]
    pub fn pairing_peak_bytes(self) -> usize {
        self.pairing_peak_bytes
    }

    /// [`DerivedMemoryPool::DecorationWorkspace`] からleaseするbyte数。
    #[must_use]
    pub fn workspace_bytes(self) -> usize {
        self.workspace_bytes
    }

    /// Callerが既にdecodeして所有しているremoved/added textのUTF-8 payload合計。
    ///
    /// この値は[`Self::workspace_bytes`]へ含めない。Decorations workerはdecode前に同じbyte数を
    /// [`DerivedMemoryPool::DecorationWorkspace`]から確保し、decode bufferを破棄するまでleaseを保持
    /// する。分離した値により、compute側のnormalization領域とdecoded blockを二重計上しない。
    #[must_use]
    pub fn decoded_input_bytes(self) -> usize {
        self.decoded_input_bytes
    }

    /// Preflightが走査したUnicode scalar合計。
    #[must_use]
    pub fn scalar_count(self) -> usize {
        self.scalar_count
    }

    /// 4つのsemantic capへ照合したnatural grapheme合計。
    #[must_use]
    pub fn grapheme_count(self) -> usize {
        self.grapheme_count
    }

    /// Ignore modeで確保し得るnormalization mapping record合計。
    #[must_use]
    pub fn mapping_piece_count(self) -> usize {
        self.mapping_piece_count
    }
}

/// Block 単位の計算結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntralineResult {
    Computed(Vec<LinePairDiff>),
    Skipped(SkipReason),
}

/// 計算結果と、そのheap capacityをDecorationWorkspaceへ計上するlease。
///
/// Computed resultではtransient計算後にleaseをresultの実capacityまで縮め、この値をdropするまで保持
/// する。Fieldはresult、leaseの順なので、wrapperをそのままdropした場合もrange Vecが先に解放される。
/// Decorations workerは[`Self::into_parts`]で両方を取り出し、resultへspanを移すのと同じtransitionで
/// lease byteをNext/Priority result leaseへ[`DerivedMemoryLedger::transfer`]する。
#[derive(Debug)]
pub struct BudgetedIntralineResult {
    result: IntralineResult,
    workspace_lease: Option<DerivedMemoryLease>,
    resident_bytes: usize,
}

impl BudgetedIntralineResult {
    fn skipped(reason: SkipReason) -> Self {
        Self {
            result: IntralineResult::Skipped(reason),
            workspace_lease: None,
            resident_bytes: 0,
        }
    }

    /// ComputedまたはSkipped resultを借用する。
    #[must_use]
    pub fn result(&self) -> &IntralineResult {
        &self.result
    }

    /// Computed resultが保持するVec capacityのbyte合計。
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }

    /// Resultとworkspace leaseをownershipごと返す。
    ///
    /// Computedではleaseが`Some`、Skippedでは`None`となる。Callerはresult allocationをNext/Priority
    /// ownerへ移した後、leaseの全byteを同ownerのleaseへatomic transferする。Tupleをそのままdrop
    /// した場合はresultが先、leaseが後にdropされる。
    #[must_use]
    pub fn into_parts(self) -> (IntralineResult, Option<DerivedMemoryLease>) {
        (self.result, self.workspace_lease)
    }

    /// Session共有台帳へ結果を残さないstandalone caller向けにplain resultを返す。
    ///
    /// Workspace leaseを先に返すため、long-lived decorations workerは[`Self::into_parts`]を使う。
    #[must_use]
    pub fn into_result(self) -> IntralineResult {
        let Self {
            result,
            workspace_lease,
            resident_bytes: _,
        } = self;
        drop(workspace_lease);
        result
    }
}

/// 削除・追加 line block を対応付け、changed byte span を返す。
#[must_use]
pub fn compute(
    removed: &[String],
    added: &[String],
    whitespace: WhitespaceChoice,
) -> IntralineResult {
    let ledger = DerivedMemoryLedger::new();
    compute_owned_with_budget(removed, added, whitespace, &ledger).into_result()
}

/// Owned line blockをsessionの共有workspace台帳へ照合して計算する。
///
/// Decorations workerのように複数計算が同時に走るcallerは、session開始時に作った同じledgerを渡す。
/// 単独利用向けの [`compute`] と違い、別workerが保持するworkspace leaseも上限判定へ入る。返り値は
/// output Vecのcapacity leaseを保持するため、result ownerへのhandoffまでdropしない。
#[must_use]
pub fn compute_with_budget(
    removed: &[String],
    added: &[String],
    whitespace: WhitespaceChoice,
    ledger: &DerivedMemoryLedger,
) -> BudgetedIntralineResult {
    compute_owned_with_budget(removed, added, whitespace, ledger)
}

fn compute_owned_with_budget(
    removed: &[String],
    added: &[String],
    whitespace: WhitespaceChoice,
    ledger: &DerivedMemoryLedger,
) -> BudgetedIntralineResult {
    if removed.len().saturating_add(added.len()) > MAX_CHANGED_LINES {
        return BudgetedIntralineResult::skipped(SkipReason::TooManyLines);
    }
    let decoded_bytes = removed
        .iter()
        .chain(added)
        .try_fold(0_usize, |sum, line| sum.checked_add(line.len()));
    if decoded_bytes.is_none_or(|bytes| bytes > MAX_BLOCK_BYTES) {
        return BudgetedIntralineResult::skipped(SkipReason::BlockTooLarge);
    }

    // `&[String]` convenience APIだけが作るborrowed-view VecもDecorationWorkspaceへ計上する。Line capを
    // 先に判定するため、巨大なcaller sliceを参照Vecへ変換してから拒否する順序にはならない。
    let line_count = removed.len().saturating_add(added.len());
    let view_bytes = match line_count.checked_mul(size_of::<&str>()) {
        Some(bytes) => bytes,
        None => {
            return BudgetedIntralineResult::skipped(SkipReason::WorkspaceEstimateOverflow);
        }
    };
    let _views = match ledger.reserve(DerivedMemoryPool::DecorationWorkspace, view_bytes) {
        Ok(lease) => lease,
        Err(_) => return BudgetedIntralineResult::skipped(SkipReason::WorkspaceBudgetExceeded),
    };
    let removed = removed.iter().map(String::as_str).collect::<Vec<_>>();
    let added = added.iter().map(String::as_str).collect::<Vec<_>>();
    compute_refs_with_budget(&removed, &added, whitespace, ledger)
}

/// Borrowした削除・追加lineをcopyせず対応付け、changed byte spanを返す。
///
/// Document snapshotを処理するworkerはbounded blockだけをowned化し、その
/// [`crate::document::OwnedDocumentLine::content`]を渡す。返すrangeは各入力`str`のUTF-8 byte
/// offsetで、所有権には依存しない。
#[must_use]
pub fn compute_refs(
    removed: &[&str],
    added: &[&str],
    whitespace: WhitespaceChoice,
) -> IntralineResult {
    // Standalone APIも96 MiB preflightを迂回しない。Long-lived workerは
    // `compute_refs_with_budget`へsession ledgerを渡し、同時計算を同じpoolへ計上する。
    let ledger = DerivedMemoryLedger::new();
    compute_refs_with_budget(removed, added, whitespace, &ledger).into_result()
}

/// Borrowしたline blockをallocation前に走査し、session workspaceをleaseして計算する。
///
/// Semantic cap、新しいgrapheme/mapping cap、capacityのchecked算術、96 MiB poolの順に判定する。
/// いずれかを超えたblockはnormalized `String`、[`MappingPiece`]、grapheme/range `Vec`を1つも作らず
/// [`IntralineResult::Skipped`] を返す。成功時はtransient leaseをoutput Vecの実capacityまで縮め、
/// [`BudgetedIntralineResult`]へ移す。計算中のpanicとresult dropはいずれもRAIIでpoolへ返却する。
#[must_use]
pub fn compute_refs_with_budget(
    removed: &[&str],
    added: &[&str],
    whitespace: WhitespaceChoice,
    ledger: &DerivedMemoryLedger,
) -> BudgetedIntralineResult {
    let estimate = match preflight_workspace(removed, added, whitespace) {
        Ok(estimate) => estimate,
        Err(reason) => return BudgetedIntralineResult::skipped(reason),
    };
    let mut workspace = match ledger.reserve(
        DerivedMemoryPool::DecorationWorkspace,
        estimate.workspace_bytes(),
    ) {
        Ok(lease) => lease,
        Err(_) => return BudgetedIntralineResult::skipped(SkipReason::WorkspaceBudgetExceeded),
    };
    let result = compute_preflighted(removed, added, whitespace);
    let resident_bytes = match result_resident_bytes(&result) {
        Ok(bytes) => bytes,
        Err(reason) => {
            drop(result);
            return BudgetedIntralineResult::skipped(reason);
        }
    };
    if resident_bytes > workspace.bytes() {
        // Preflight underestimateを事後reserveで補うと、allocation済みだが未計上の時間が生じる。
        // Resultをworkspace leaseより先に破棄し、block全体をpublishしない。
        drop(result);
        return BudgetedIntralineResult::skipped(SkipReason::WorkspaceEstimateOverflow);
    } else {
        let released = workspace.bytes() - resident_bytes;
        if workspace.release(released).is_err() {
            drop(result);
            return BudgetedIntralineResult::skipped(SkipReason::WorkspaceEstimateOverflow);
        }
    }
    BudgetedIntralineResult {
        result,
        workspace_lease: Some(workspace),
        resident_bytes,
    }
}

fn result_resident_bytes(result: &IntralineResult) -> Result<usize, SkipReason> {
    let IntralineResult::Computed(pairs) = result else {
        return Ok(0);
    };
    let mut bytes = checked_mul(pairs.capacity(), size_of::<LinePairDiff>())?;
    for pair in pairs {
        bytes = checked_add(
            bytes,
            checked_mul(pair.old_changed.capacity(), size_of::<Range<usize>>())?,
        )?;
        bytes = checked_add(
            bytes,
            checked_mul(pair.new_changed.capacity(), size_of::<Range<usize>>())?,
        )?;
    }
    Ok(bytes)
}

/// Intraline計算が要求するworkspaceをallocationなしで見積もる。
///
/// Input sliceと`str`はcaller所有として計上しない。走査中はUnicode segmentationのcursorと整数だけを
/// 保持し、semantic capまたはimplementation capを超えた時点で対応する[`SkipReason`]を返す。
pub fn preflight_workspace(
    removed: &[&str],
    added: &[&str],
    whitespace: WhitespaceChoice,
) -> Result<IntralineWorkspaceEstimate, SkipReason> {
    if removed.len().saturating_add(added.len()) > MAX_CHANGED_LINES {
        return Err(SkipReason::TooManyLines);
    }

    let byte_count = removed
        .iter()
        .chain(added)
        .try_fold(0_usize, |sum, line| sum.checked_add(line.len()));
    if byte_count.is_none_or(|count| count > MAX_BLOCK_BYTES) {
        return Err(SkipReason::BlockTooLarge);
    }

    let removed_metrics = analyze_block(removed, whitespace)?;
    let added_metrics = analyze_block(added, whitespace)?;
    let grapheme_count = checked_add(removed_metrics.grapheme_count, added_metrics.grapheme_count)?;
    if grapheme_count > MAX_BLOCK_GRAPHEMES {
        return Err(SkipReason::TooManyGraphemes);
    }

    estimate_workspace(removed_metrics, added_metrics)
}

#[derive(Clone, Copy, Debug, Default)]
struct BlockMetrics {
    line_count: usize,
    input_bytes: usize,
    scalar_count: usize,
    grapheme_count: usize,
    mapping_piece_count: usize,
    max_line_bytes: usize,
    max_line_graphemes: usize,
    max_line_mapping_pieces: usize,
}

#[derive(Clone, Copy, Debug, Default)]
struct LineMetrics {
    input_bytes: usize,
    scalar_count: usize,
    grapheme_count: usize,
    mapping_piece_count: usize,
}

fn analyze_block(lines: &[&str], whitespace: WhitespaceChoice) -> Result<BlockMetrics, SkipReason> {
    let mut block = BlockMetrics::default();
    for line in lines {
        let metrics = analyze_line(line, whitespace)?;
        block.line_count = checked_add(block.line_count, 1)?;
        block.input_bytes = checked_add(block.input_bytes, metrics.input_bytes)?;
        block.scalar_count = checked_add(block.scalar_count, metrics.scalar_count)?;
        block.grapheme_count = checked_add(block.grapheme_count, metrics.grapheme_count)?;
        if block.grapheme_count > MAX_BLOCK_GRAPHEMES {
            return Err(SkipReason::TooManyGraphemes);
        }
        block.mapping_piece_count =
            checked_add(block.mapping_piece_count, metrics.mapping_piece_count)?;
        block.max_line_bytes = block.max_line_bytes.max(metrics.input_bytes);
        block.max_line_graphemes = block.max_line_graphemes.max(metrics.grapheme_count);
        block.max_line_mapping_pieces = block
            .max_line_mapping_pieces
            .max(metrics.mapping_piece_count);
    }
    Ok(block)
}

fn analyze_line(input: &str, whitespace: WhitespaceChoice) -> Result<LineMetrics, SkipReason> {
    let mut metrics = LineMetrics {
        input_bytes: input.len(),
        ..LineMetrics::default()
    };

    for grapheme in input.graphemes(true) {
        metrics.grapheme_count = checked_add(metrics.grapheme_count, 1)?;
        if metrics.grapheme_count > MAX_LINE_GRAPHEMES {
            // ASCIIのような短いgraphemeを上限+1個読んだ時点で返し、line末尾までは走査しない。
            return Err(SkipReason::LineTooLong);
        }
        if grapheme.len() > MAX_GRAPHEME_BYTES {
            return Err(SkipReason::NaturalGraphemeTooLarge);
        }

        let scalar_count = grapheme.chars().count();
        metrics.scalar_count = checked_add(metrics.scalar_count, scalar_count)?;
        if whitespace == WhitespaceChoice::Ignore {
            let pieces = mapping_piece_upper_bound(grapheme)?;
            let mapping_bytes = checked_mul(pieces, size_of::<MappingPiece>())?;
            if mapping_bytes > MAX_MAPPING_BYTES_PER_GRAPHEME {
                return Err(SkipReason::MappingClusterTooLarge);
            }
            metrics.mapping_piece_count = checked_add(metrics.mapping_piece_count, pieces)?;
        }
    }
    Ok(metrics)
}

fn mapping_piece_upper_bound(text: &str) -> Result<usize, SkipReason> {
    let mut pieces = 0_usize;
    let mut in_horizontal_whitespace = false;
    for character in text.chars() {
        if matches!(character, ' ' | '\t') {
            if !in_horizontal_whitespace {
                pieces = checked_add(pieces, 1)?;
                in_horizontal_whitespace = true;
            }
        } else {
            pieces = checked_add(pieces, 1)?;
            in_horizontal_whitespace = false;
        }
    }
    Ok(pieces)
}

fn estimate_workspace(
    removed: BlockMetrics,
    added: BlockMetrics,
) -> Result<IntralineWorkspaceEstimate, SkipReason> {
    let line_count = checked_add(removed.line_count, added.line_count)?;
    let input_bytes = checked_add(removed.input_bytes, added.input_bytes)?;
    let scalar_count = checked_add(removed.scalar_count, added.scalar_count)?;
    let grapheme_count = checked_add(removed.grapheme_count, added.grapheme_count)?;
    let mapping_piece_count = checked_add(removed.mapping_piece_count, added.mapping_piece_count)?;

    // Alignment phaseでは全normalized Stringが同時に生存する。Ignore modeのnormalized textは入力より
    // 長くならず、Vec<String>の要素数はslice長からexact capacityを指定する。
    let normalized_lines = checked_mul(line_count, size_of::<String>())?;
    let alignment_algorithm = checked_mul(
        checked_add(line_count, VEC_GROWTH_FACTOR)?,
        ALGORITHM_BYTES_PER_ELEMENT,
    )?;
    let alignment_peak_bytes = checked_sum(&[input_bytes, normalized_lines, alignment_algorithm])?;

    // Patience alignmentが選ぶpairをpreflight時点では知らない。片側それぞれの最大lineを組み合わせれば、
    // どのpair順でもcurrent pairのtext、scalar mapping、grapheme vectorの上限になる。
    let pair_bytes = checked_add(removed.max_line_bytes, added.max_line_bytes)?;
    let pair_graphemes = checked_add(removed.max_line_graphemes, added.max_line_graphemes)?;
    let pair_mapping_pieces = checked_add(
        removed.max_line_mapping_pieces,
        added.max_line_mapping_pieces,
    )?;

    let line_operations = checked_mul(
        checked_mul(checked_add(line_count, 1)?, size_of::<DiffOp>())?,
        VEC_GROWTH_FACTOR,
    )?;
    let pair_records = checked_mul(
        removed.line_count.min(added.line_count),
        size_of::<LinePairDiff>(),
    )?;
    let scalar_mapping = checked_mul(
        checked_mul(pair_mapping_pieces, size_of::<MappingPiece>())?,
        VEC_GROWTH_FACTOR,
    )?;
    let grapheme_references = checked_mul(
        checked_mul(pair_graphemes, size_of::<&str>())?,
        VEC_GROWTH_FACTOR,
    )?;
    let retained_ranges = checked_mul(
        checked_mul(grapheme_count, size_of::<Range<usize>>())?,
        VEC_GROWTH_FACTOR,
    )?;
    let current_pair_ranges =
        checked_mul(checked_mul(pair_graphemes, size_of::<Range<usize>>())?, 3)?;
    let pair_algorithm = checked_mul(
        checked_add(pair_graphemes, VEC_GROWTH_FACTOR)?,
        ALGORITHM_BYTES_PER_ELEMENT,
    )?;
    let pairing_peak_bytes = checked_sum(&[
        line_operations,
        pair_records,
        pair_bytes,
        scalar_mapping,
        grapheme_references,
        retained_ranges,
        current_pair_ranges,
        pair_algorithm,
    ])?;
    let workspace_bytes = alignment_peak_bytes.max(pairing_peak_bytes);
    if workspace_bytes > DECORATION_WORKSPACE_MEMORY_BYTES {
        return Err(SkipReason::WorkspaceBudgetExceeded);
    }

    Ok(IntralineWorkspaceEstimate {
        alignment_peak_bytes,
        pairing_peak_bytes,
        workspace_bytes,
        decoded_input_bytes: input_bytes,
        scalar_count,
        grapheme_count,
        mapping_piece_count,
    })
}

fn checked_sum(values: &[usize]) -> Result<usize, SkipReason> {
    values.iter().copied().try_fold(0_usize, checked_add)
}

fn checked_add(left: usize, right: usize) -> Result<usize, SkipReason> {
    left.checked_add(right)
        .ok_or(SkipReason::WorkspaceEstimateOverflow)
}

fn checked_mul(left: usize, right: usize) -> Result<usize, SkipReason> {
    left.checked_mul(right)
        .ok_or(SkipReason::WorkspaceEstimateOverflow)
}

fn compute_preflighted(
    removed: &[&str],
    added: &[&str],
    whitespace: WhitespaceChoice,
) -> IntralineResult {
    let mut normalized_old = Vec::with_capacity(removed.len());
    for line in removed {
        normalized_old.push(normalize_text(line, whitespace));
    }
    let mut normalized_new = Vec::with_capacity(added.len());
    for line in added {
        normalized_new.push(normalize_text(line, whitespace));
    }

    let operations = capture_diff_slices(Algorithm::Patience, &normalized_old, &normalized_new);
    // Line alignment後に必要なのはindexだけである。Block全体のnormalized textをpair phaseまで残すと、
    // preflightの2つのphaseが重なり、96 MiB leaseの前提が崩れる。
    drop(normalized_old);
    drop(normalized_new);

    let mut pairs = Vec::with_capacity(removed.len().min(added.len()));
    for operation in operations {
        match operation {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                for delta in 0..len {
                    pairs.push(pair_lines(
                        old_index + delta,
                        new_index + delta,
                        removed[old_index + delta],
                        added[new_index + delta],
                        whitespace,
                    ));
                }
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                for delta in 0..old_len.min(new_len) {
                    pairs.push(pair_lines(
                        old_index + delta,
                        new_index + delta,
                        removed[old_index + delta],
                        added[new_index + delta],
                        whitespace,
                    ));
                }
            }
            DiffOp::Delete { .. } | DiffOp::Insert { .. } => {}
        }
    }

    pairs.sort_by_key(|pair| (pair.old_index, pair.new_index));
    IntralineResult::Computed(pairs)
}

fn pair_lines(
    old_index: usize,
    new_index: usize,
    old: &str,
    new: &str,
    whitespace: WhitespaceChoice,
) -> LinePairDiff {
    let old_comparable = ComparableLine::new(old, whitespace);
    let new_comparable = ComparableLine::new(new, whitespace);
    if old_comparable.text == new_comparable.text {
        return LinePairDiff {
            old_index,
            new_index,
            old_changed: Vec::new(),
            new_changed: Vec::new(),
        };
    }

    // Ignore modeではoriginal textをdiffすると、content変更と同じlineにある無視対象spaceまで
    // changed spanへ入る。正規化後のgraphemeを比較し、対応表でoriginal byte rangeへ戻す。
    let mut old_graphemes = Vec::with_capacity(old_comparable.original_ranges.len());
    old_graphemes.extend(old_comparable.text.graphemes(true));
    let mut new_graphemes = Vec::with_capacity(new_comparable.original_ranges.len());
    new_graphemes.extend(new_comparable.text.graphemes(true));
    let operations = capture_diff_slices(Algorithm::Myers, &old_graphemes, &new_graphemes);
    let mut old_changed = Vec::with_capacity(operations.len());
    let mut new_changed = Vec::with_capacity(operations.len());

    for operation in operations {
        match operation {
            DiffOp::Equal { .. } => {}
            DiffOp::Delete {
                old_index, old_len, ..
            } => push_span(
                &mut old_changed,
                &old_comparable.original_ranges,
                old_index,
                old_len,
            ),
            DiffOp::Insert {
                new_index, new_len, ..
            } => push_span(
                &mut new_changed,
                &new_comparable.original_ranges,
                new_index,
                new_len,
            ),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                push_span(
                    &mut old_changed,
                    &old_comparable.original_ranges,
                    old_index,
                    old_len,
                );
                push_span(
                    &mut new_changed,
                    &new_comparable.original_ranges,
                    new_index,
                    new_len,
                );
            }
        }
    }

    LinePairDiff {
        old_index,
        new_index,
        old_changed: merge_adjacent(old_changed),
        new_changed: merge_adjacent(new_changed),
    }
}

struct ComparableLine {
    text: String,
    original_ranges: Vec<Range<usize>>,
}

impl ComparableLine {
    fn new(input: &str, whitespace: WhitespaceChoice) -> Self {
        if whitespace == WhitespaceChoice::Show {
            let grapheme_count = input.graphemes(true).count();
            let mut original_ranges = Vec::with_capacity(grapheme_count);
            original_ranges.extend(
                input
                    .grapheme_indices(true)
                    .map(|(start, grapheme)| start..start + grapheme.len()),
            );
            return Self {
                text: input.to_owned(),
                original_ranges,
            };
        }

        let (text, pieces) = normalize_with_mapping(input);
        let mut original_ranges = Vec::with_capacity(text.graphemes(true).count());
        let mut piece_index = 0_usize;
        for (start, grapheme) in text.grapheme_indices(true) {
            let end = start + grapheme.len();
            while pieces
                .get(piece_index)
                .is_some_and(|piece| piece.normalized.end <= start)
            {
                piece_index += 1;
            }
            let Some(first) = pieces.get(piece_index) else {
                break;
            };
            let mut original_end = first.original.end;
            let mut cursor = piece_index + 1;
            while let Some(piece) = pieces.get(cursor)
                && piece.normalized.start < end
            {
                original_end = piece.original.end;
                cursor += 1;
            }
            original_ranges.push(first.original.start..original_end);
        }
        Self {
            text,
            original_ranges,
        }
    }
}

struct MappingPiece {
    normalized: Range<usize>,
    original: Range<usize>,
}

fn normalize_with_mapping(input: &str) -> (String, Vec<MappingPiece>) {
    let (content_start, content_end) = normalized_content_bounds(input);
    let content = &input[content_start..content_end];
    let mut normalized = String::with_capacity(content.len());
    // 1 scalarにつき最大1 recordであり、whitespace runは複数scalarを1 recordへ縮める。Preflightと
    // 同じ入力をもう一度数えてexact upper capacityを指定し、push growthをworkspace見積もりから除く。
    let mut pieces = Vec::with_capacity(content.chars().count());
    let mut pending_space: Option<Range<usize>> = None;

    for (relative, character) in content.char_indices() {
        let original_start = content_start + relative;
        let original_end = original_start + character.len_utf8();
        if matches!(character, ' ' | '\t') {
            match &mut pending_space {
                Some(range) => range.end = original_end,
                None => pending_space = Some(original_start..original_end),
            }
            continue;
        }

        if let Some(original) = pending_space.take() {
            let start = normalized.len();
            normalized.push(' ');
            pieces.push(MappingPiece {
                normalized: start..normalized.len(),
                original,
            });
        }
        let start = normalized.len();
        normalized.push(character);
        pieces.push(MappingPiece {
            normalized: start..normalized.len(),
            original: original_start..original_end,
        });
    }
    (normalized, pieces)
}

fn normalize_text(input: &str, whitespace: WhitespaceChoice) -> String {
    if whitespace == WhitespaceChoice::Show {
        return input.to_owned();
    }

    let (content_start, content_end) = normalized_content_bounds(input);
    let content = &input[content_start..content_end];
    let mut normalized = String::with_capacity(content.len());
    let mut pending_space = false;
    for character in content.chars() {
        if matches!(character, ' ' | '\t') {
            pending_space = true;
            continue;
        }
        if pending_space {
            normalized.push(' ');
            pending_space = false;
        }
        normalized.push(character);
    }
    normalized
}

fn normalized_content_bounds(input: &str) -> (usize, usize) {
    let content_start = input
        .char_indices()
        .find(|(_, character)| !matches!(character, ' ' | '\t'))
        .map_or(input.len(), |(index, _)| index);
    let content_end = input
        .char_indices()
        .rev()
        .find(|(_, character)| !matches!(character, ' ' | '\t'))
        .map_or(content_start, |(index, character)| {
            index + character.len_utf8()
        });
    (content_start, content_end)
}

fn push_span(
    spans: &mut Vec<Range<usize>>,
    original_ranges: &[Range<usize>],
    index: usize,
    len: usize,
) {
    if len == 0 {
        return;
    }
    let Some(last_index) = index.checked_add(len - 1) else {
        return;
    };
    let (Some(first), Some(last)) = (original_ranges.get(index), original_ranges.get(last_index))
    else {
        return;
    };
    spans.push(first.start..last.end);
}

fn merge_adjacent(mut spans: Vec<Range<usize>>) -> Vec<Range<usize>> {
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(spans.len());
    spans.sort_by_key(|range| range.start);
    for span in spans {
        if let Some(previous) = merged.last_mut()
            && previous.end == span.start
        {
            previous.end = span.end;
            continue;
        }
        merged.push(span);
    }
    merged
}

/// Whitespace ignore mode の比較用 representation を返す。
#[must_use]
pub fn normalize(input: &str, whitespace: WhitespaceChoice) -> String {
    normalize_text(input, whitespace)
}

/// Show mode で changed whitespace span を可視 glyph へ置換する。
///
/// `tab_width` はCLIでは`1..=16`に制限される。内部callerから範囲外の値が来てもrendererをpanic
/// させず、この関数の境界で同じ範囲へ丸める。
#[must_use]
pub fn visualize_changed_whitespace(text: &str, tab_width: usize) -> String {
    let tab_width = tab_width.clamp(1, 16);
    let mut rendered = String::with_capacity(text.len());
    let mut column = 0_usize;
    for grapheme in text.graphemes(true) {
        if grapheme.contains([' ', '\t']) {
            // ASCII whitespaceにcombining markが続くと1 graphemeになる。grapheme全体の一致判定では
            // spaceを見落とすため、この稀な形だけscalar単位で置換する。combining markの幅は0。
            for character in grapheme.chars() {
                match character {
                    ' ' => {
                        rendered.push('·');
                        column = column.saturating_add(1);
                    }
                    '\t' => {
                        let cells = tab_width - (column % tab_width);
                        rendered.push('→');
                        rendered.extend(std::iter::repeat_n('·', cells.saturating_sub(1)));
                        column = column.saturating_add(cells);
                    }
                    _ => {
                        rendered.push(character);
                        column = column.saturating_add(
                            unicode_width::UnicodeWidthChar::width(character).unwrap_or(0),
                        );
                    }
                }
            }
        } else {
            rendered.push_str(grapheme);
            column = column.saturating_add(unicode_width::UnicodeWidthStr::width(grapheme));
        }
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_normalization_keeps_token_boundaries() {
        assert_eq!(
            normalize("  foo\t  bar  ", WhitespaceChoice::Ignore),
            "foo bar"
        );
        assert_ne!(
            normalize("foo bar", WhitespaceChoice::Ignore),
            normalize("foobar", WhitespaceChoice::Ignore)
        );
    }

    #[test]
    fn ignored_whitespace_produces_no_inner_spans() {
        let result = compute(
            &["  foo\tbar".to_owned()],
            &["foo    bar  ".to_owned()],
            WhitespaceChoice::Ignore,
        );
        let IntralineResult::Computed(pairs) = result else {
            panic!("expected computed result");
        };
        assert_eq!(pairs.len(), 1);
        assert!(pairs[0].old_changed.is_empty());
        assert!(pairs[0].new_changed.is_empty());
    }

    #[test]
    fn changed_identifier_is_grapheme_precise() {
        let result = compute(
            &["calculate_total".to_owned()],
            &["calculate_sum".to_owned()],
            WhitespaceChoice::Show,
        );
        let IntralineResult::Computed(pairs) = result else {
            panic!("expected computed result");
        };
        assert_eq!(&"calculate_total"[pairs[0].old_changed[0].clone()], "total");
        assert_eq!(&"calculate_sum"[pairs[0].new_changed[0].clone()], "sum");
    }

    #[test]
    fn ignored_whitespace_is_not_highlighted_beside_content_changes() {
        let result = compute(
            &["  foo\t  old  ".to_owned()],
            &["foo      new\t".to_owned()],
            WhitespaceChoice::Ignore,
        );
        let IntralineResult::Computed(pairs) = result else {
            panic!("expected computed result");
        };
        assert_eq!(pairs.len(), 1);
        assert_eq!(
            pairs[0]
                .old_changed
                .iter()
                .map(|range| &"  foo\t  old  "[range.clone()])
                .collect::<Vec<_>>(),
            vec!["old"]
        );
        assert_eq!(
            pairs[0]
                .new_changed
                .iter()
                .map(|range| &"foo      new\t"[range.clone()])
                .collect::<Vec<_>>(),
            vec!["new"]
        );
    }

    #[test]
    fn token_separator_remains_semantically_significant() {
        let result = compute(
            &["foobar".to_owned()],
            &["foo\tbar".to_owned()],
            WhitespaceChoice::Ignore,
        );
        let IntralineResult::Computed(pairs) = result else {
            panic!("expected computed result");
        };
        assert!(pairs[0].old_changed.is_empty());
        assert_eq!(&"foo\tbar"[pairs[0].new_changed[0].clone()], "\t");
    }

    #[test]
    fn mapped_spans_keep_unicode_grapheme_boundaries() {
        let old = "a  e\u{301}";
        let new = "a\té";
        let result = compute(
            &[old.to_owned()],
            &[new.to_owned()],
            WhitespaceChoice::Ignore,
        );
        let IntralineResult::Computed(pairs) = result else {
            panic!("expected computed result");
        };
        assert_eq!(&old[pairs[0].old_changed[0].clone()], "e\u{301}");
        assert_eq!(&new[pairs[0].new_changed[0].clone()], "é");
    }

    #[test]
    fn whitespace_visualization_uses_tab_cells() {
        assert_eq!(visualize_changed_whitespace(" \t", 4), "·→··");
        assert_eq!(visualize_changed_whitespace(" \u{301}", 4), "·\u{301}");
        assert_eq!(visualize_changed_whitespace("\t", 0), "→");
        assert_eq!(
            visualize_changed_whitespace("\t", usize::MAX),
            "→···············"
        );
    }

    #[test]
    fn budget_skips_line_after_first_excess_grapheme() {
        let result = compute(
            &["a".repeat(MAX_LINE_GRAPHEMES + 1)],
            &[],
            WhitespaceChoice::Ignore,
        );
        assert_eq!(result, IntralineResult::Skipped(SkipReason::LineTooLong));
    }

    #[test]
    fn preflight_accounts_normalization_scalars_graphemes_and_ranges() {
        let estimate = preflight_workspace(
            &["  alpha\t e\u{301}"],
            &["alpha new"],
            WhitespaceChoice::Ignore,
        )
        .unwrap();
        assert_eq!(estimate.scalar_count(), 20);
        assert_eq!(estimate.grapheme_count(), 19);
        assert!(estimate.mapping_piece_count() > 0);
        assert_eq!(estimate.decoded_input_bytes(), 21);
        assert!(estimate.alignment_peak_bytes() > 0);
        assert!(estimate.pairing_peak_bytes() > 0);
        assert_eq!(
            estimate.workspace_bytes(),
            estimate
                .alignment_peak_bytes()
                .max(estimate.pairing_peak_bytes())
        );
        assert!(estimate.workspace_bytes() <= DECORATION_WORKSPACE_MEMORY_BYTES);
    }

    #[test]
    fn owned_api_checks_line_cap_before_collecting_borrowed_views() {
        let removed = vec![String::new(); MAX_CHANGED_LINES + 1];
        assert_eq!(
            compute(&removed, &[], WhitespaceChoice::Ignore),
            IntralineResult::Skipped(SkipReason::TooManyLines)
        );

        let ledger = DerivedMemoryLedger::new();
        assert_eq!(
            compute_with_budget(&removed, &[], WhitespaceChoice::Ignore, &ledger).into_result(),
            IntralineResult::Skipped(SkipReason::TooManyLines)
        );
        assert_eq!(ledger.snapshot().total_reserved_bytes(), Some(0));
    }

    #[test]
    fn natural_grapheme_limit_accepts_exact_boundary_and_rejects_next_cluster() {
        let exact = format!("é{}", "\u{301}".repeat((MAX_GRAPHEME_BYTES - 2) / 2));
        assert_eq!(exact.len(), MAX_GRAPHEME_BYTES);
        assert!(preflight_workspace(&[exact.as_str()], &[], WhitespaceChoice::Show).is_ok());

        let oversized = format!("{exact}\u{301}");
        assert_eq!(
            preflight_workspace(&[oversized.as_str()], &[], WhitespaceChoice::Show),
            Err(SkipReason::NaturalGraphemeTooLarge)
        );
    }

    #[test]
    fn mapping_heavy_cluster_is_rejected_before_mapping_vector_allocation() {
        let piece_bytes = size_of::<MappingPiece>();
        let exact_piece_count = MAX_MAPPING_BYTES_PER_GRAPHEME / piece_bytes;
        let exact = format!("a{}", "\u{301}".repeat(exact_piece_count - 1));
        let estimate =
            preflight_workspace(&[exact.as_str()], &["b"], WhitespaceChoice::Ignore).unwrap();
        assert_eq!(estimate.mapping_piece_count(), exact_piece_count + 1);

        let oversized = format!("{exact}\u{301}");
        assert_eq!(
            preflight_workspace(&[oversized.as_str()], &[], WhitespaceChoice::Ignore),
            Err(SkipReason::MappingClusterTooLarge)
        );
        assert_eq!(
            compute_refs(&[oversized.as_str()], &[], WhitespaceChoice::Ignore),
            IntralineResult::Skipped(SkipReason::MappingClusterTooLarge)
        );
        assert!(preflight_workspace(&[oversized.as_str()], &[], WhitespaceChoice::Show).is_ok());
    }

    #[test]
    fn workspace_capacity_arithmetic_reports_overflow() {
        assert_eq!(
            checked_add(usize::MAX, 1),
            Err(SkipReason::WorkspaceEstimateOverflow)
        );
        assert_eq!(
            checked_mul(usize::MAX, 2),
            Err(SkipReason::WorkspaceEstimateOverflow)
        );
        assert_eq!(
            estimate_workspace(
                BlockMetrics {
                    line_count: usize::MAX,
                    ..BlockMetrics::default()
                },
                BlockMetrics::default(),
            ),
            Err(SkipReason::WorkspaceEstimateOverflow)
        );
    }

    #[test]
    fn preflight_upper_bound_covers_visible_allocation_capacities() {
        let cases = [
            (vec!["old value", "same"], vec!["new value", "same"]),
            (vec!["a  e\u{301}", "🦀 rust"], vec!["a\té", "🐇 rust"]),
            (vec!["", "alpha beta gamma"], vec!["delta", "alpha\tbeta"]),
        ];

        for whitespace in [WhitespaceChoice::Ignore, WhitespaceChoice::Show] {
            for (removed, added) in &cases {
                let estimate = preflight_workspace(removed, added, whitespace).unwrap();
                let normalized_old = removed
                    .iter()
                    .map(|line| normalize_text(line, whitespace))
                    .collect::<Vec<_>>();
                let normalized_new = added
                    .iter()
                    .map(|line| normalize_text(line, whitespace))
                    .collect::<Vec<_>>();
                let line_operations =
                    capture_diff_slices(Algorithm::Patience, &normalized_old, &normalized_new);
                let visible_alignment = normalized_old.capacity() * size_of::<String>()
                    + normalized_new.capacity() * size_of::<String>()
                    + normalized_old
                        .iter()
                        .chain(&normalized_new)
                        .map(String::capacity)
                        .sum::<usize>()
                    + line_operations.capacity() * size_of::<DiffOp>();
                assert!(
                    estimate.alignment_peak_bytes() >= visible_alignment,
                    "alignment estimate {estimate:?} did not cover {visible_alignment} bytes"
                );

                let computed = compute_preflighted(removed, added, whitespace);
                let IntralineResult::Computed(pairs) = computed else {
                    panic!("preflighted corpus must compute");
                };
                let output_capacity = pairs.capacity() * size_of::<LinePairDiff>()
                    + pairs
                        .iter()
                        .flat_map(|pair| [&pair.old_changed, &pair.new_changed])
                        .map(|ranges| ranges.capacity() * size_of::<Range<usize>>())
                        .sum::<usize>();

                let mut largest_pair_capacity = 0_usize;
                for old in removed {
                    for new in added {
                        let old_comparable = ComparableLine::new(old, whitespace);
                        let new_comparable = ComparableLine::new(new, whitespace);
                        let old_graphemes = old_comparable.text.graphemes(true).collect::<Vec<_>>();
                        let new_graphemes = new_comparable.text.graphemes(true).collect::<Vec<_>>();
                        let operations =
                            capture_diff_slices(Algorithm::Myers, &old_graphemes, &new_graphemes);
                        let mapping_capacity = if whitespace == WhitespaceChoice::Ignore {
                            (old.chars().count() + new.chars().count()) * size_of::<MappingPiece>()
                        } else {
                            0
                        };
                        let visible_pair = old_comparable.text.capacity()
                            + new_comparable.text.capacity()
                            + mapping_capacity
                            + (old_comparable.original_ranges.capacity()
                                + new_comparable.original_ranges.capacity())
                                * size_of::<Range<usize>>()
                            + (old_graphemes.capacity() + new_graphemes.capacity())
                                * size_of::<&str>()
                            + operations.capacity() * size_of::<DiffOp>()
                            + operations.capacity() * 3 * size_of::<Range<usize>>();
                        largest_pair_capacity = largest_pair_capacity.max(visible_pair);
                    }
                }
                let visible_pairing = line_operations.capacity() * size_of::<DiffOp>()
                    + output_capacity
                    + largest_pair_capacity;
                assert!(
                    estimate.pairing_peak_bytes() >= visible_pairing,
                    "pairing estimate {estimate:?} did not cover {visible_pairing} bytes"
                );
            }
        }
    }

    #[test]
    fn owned_api_borrowed_view_capacity_uses_the_shared_workspace_pool() {
        let removed = vec!["old value".to_owned()];
        let added = vec!["new value".to_owned()];
        let estimate = preflight_workspace(
            &[removed[0].as_str()],
            &[added[0].as_str()],
            WhitespaceChoice::Ignore,
        )
        .unwrap();
        let ledger = DerivedMemoryLedger::new();
        let occupied = ledger
            .reserve(
                DerivedMemoryPool::DecorationWorkspace,
                DECORATION_WORKSPACE_MEMORY_BYTES - estimate.workspace_bytes(),
            )
            .unwrap();
        assert_eq!(
            compute_with_budget(&removed, &added, WhitespaceChoice::Ignore, &ledger).into_result(),
            IntralineResult::Skipped(SkipReason::WorkspaceBudgetExceeded)
        );
        assert_eq!(
            ledger
                .snapshot()
                .reserved_bytes(DerivedMemoryPool::DecorationWorkspace),
            DECORATION_WORKSPACE_MEMORY_BYTES - estimate.workspace_bytes()
        );
        drop(occupied);
    }

    #[test]
    fn computed_result_keeps_its_lease_until_atomic_result_handoff() {
        let ledger = DerivedMemoryLedger::new();
        let budgeted = compute_refs_with_budget(
            &["old value"],
            &["new value"],
            WhitespaceChoice::Ignore,
            &ledger,
        );
        assert!(matches!(budgeted.result(), IntralineResult::Computed(_)));
        let resident_bytes = budgeted.resident_bytes();
        assert!(resident_bytes > 0);
        assert_eq!(
            ledger
                .snapshot()
                .reserved_bytes(DerivedMemoryPool::DecorationWorkspace),
            resident_bytes
        );

        let (result, workspace) = budgeted.into_parts();
        let Some(mut workspace) = workspace else {
            panic!("computed result must retain a workspace lease");
        };
        let mut next = ledger
            .reserve(DerivedMemoryPool::NextDecoration, 0)
            .unwrap();
        ledger
            .transfer(&mut workspace, &mut next, resident_bytes)
            .unwrap();
        assert_eq!(workspace.bytes(), 0);
        assert_eq!(next.bytes(), resident_bytes);
        assert_eq!(
            ledger
                .snapshot()
                .reserved_bytes(DerivedMemoryPool::DecorationWorkspace),
            0
        );
        assert_eq!(
            ledger
                .snapshot()
                .reserved_bytes(DerivedMemoryPool::NextDecoration),
            resident_bytes
        );

        drop(result);
        drop(workspace);
        drop(next);
        assert_eq!(ledger.snapshot().total_reserved_bytes(), Some(0));
    }

    #[test]
    fn shared_workspace_exhaustion_skips_without_leaking_a_lease() {
        let ledger = DerivedMemoryLedger::new();
        let occupied = ledger
            .reserve(
                DerivedMemoryPool::DecorationWorkspace,
                DECORATION_WORKSPACE_MEMORY_BYTES,
            )
            .unwrap();
        assert_eq!(
            compute_refs_with_budget(
                &["old value"],
                &["new value"],
                WhitespaceChoice::Ignore,
                &ledger,
            )
            .into_result(),
            IntralineResult::Skipped(SkipReason::WorkspaceBudgetExceeded)
        );
        assert_eq!(
            ledger
                .snapshot()
                .reserved_bytes(DerivedMemoryPool::DecorationWorkspace),
            DECORATION_WORKSPACE_MEMORY_BYTES
        );

        drop(occupied);
        assert!(matches!(
            compute_refs_with_budget(
                &["old value"],
                &["new value"],
                WhitespaceChoice::Ignore,
                &ledger,
            )
            .into_result(),
            IntralineResult::Computed(_)
        ));
        assert_eq!(
            ledger
                .snapshot()
                .reserved_bytes(DerivedMemoryPool::DecorationWorkspace),
            0
        );
    }
}
