//! Adjacent deletion/addition block の grapheme-level diff。
//!
//! Git の `-`/`+` 行を隠さず、対応付けと強調 span だけを生成する。block の line alignment は
//! whitespace policy 適用後の文字列へ Patience diff を使い、anchor 間の unmatched 行を入力順に
//! pair する。worker が budget 超過を検出した場合は部分結果を返さず block 全体を fallback する。

use std::ops::Range;

use similar::{Algorithm, DiffOp, capture_diff_slices};
use unicode_segmentation::UnicodeSegmentation;

use crate::cli::WhitespaceChoice;

/// 1 change block に含める削除・追加行の合計上限。
pub const MAX_CHANGED_LINES: usize = 5_000;
/// 1 logical line の grapheme 上限。
pub const MAX_LINE_GRAPHEMES: usize = 20_000;
/// 1 change block の UTF-8 byte 上限。
pub const MAX_BLOCK_BYTES: usize = 64 * 1024 * 1024;
/// 1 change block の grapheme 合計上限。
pub const MAX_BLOCK_GRAPHEMES: usize = 10_000_000;

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
}

/// Block 単位の計算結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntralineResult {
    Computed(Vec<LinePairDiff>),
    Skipped(SkipReason),
}

/// 削除・追加 line block を対応付け、changed byte span を返す。
#[must_use]
pub fn compute(
    removed: &[String],
    added: &[String],
    whitespace: WhitespaceChoice,
) -> IntralineResult {
    let removed = removed.iter().map(String::as_str).collect::<Vec<_>>();
    let added = added.iter().map(String::as_str).collect::<Vec<_>>();
    compute_refs(&removed, &added, whitespace)
}

/// Borrowした削除・追加lineをcopyせず対応付け、changed byte spanを返す。
///
/// Document snapshotを処理するworkerはraw textを所有し直さず、このAPIへ`DocumentLine::content()`を
/// 渡す。返すrangeは各入力`str`のUTF-8 byte offsetで、所有権には依存しない。
#[must_use]
pub fn compute_refs(
    removed: &[&str],
    added: &[&str],
    whitespace: WhitespaceChoice,
) -> IntralineResult {
    if removed.len().saturating_add(added.len()) > MAX_CHANGED_LINES {
        return IntralineResult::Skipped(SkipReason::TooManyLines);
    }

    let byte_count = removed
        .iter()
        .chain(added)
        .try_fold(0_usize, |sum, line| sum.checked_add(line.len()));
    if byte_count.is_none_or(|count| count > MAX_BLOCK_BYTES) {
        return IntralineResult::Skipped(SkipReason::BlockTooLarge);
    }

    let mut grapheme_total = 0_usize;
    for line in removed.iter().chain(added) {
        // 上限を1つ超えた時点で省略理由は確定する。巨大なASCII lineを末尾までsegmentしない。
        let count = line.graphemes(true).take(MAX_LINE_GRAPHEMES + 1).count();
        if count > MAX_LINE_GRAPHEMES {
            return IntralineResult::Skipped(SkipReason::LineTooLong);
        }
        grapheme_total = grapheme_total.saturating_add(count);
        if grapheme_total > MAX_BLOCK_GRAPHEMES {
            return IntralineResult::Skipped(SkipReason::TooManyGraphemes);
        }
    }

    let normalized_old: Vec<String> = removed
        .iter()
        .map(|line| normalize(line, whitespace))
        .collect();
    let normalized_new: Vec<String> = added
        .iter()
        .map(|line| normalize(line, whitespace))
        .collect();
    let operations = capture_diff_slices(Algorithm::Patience, &normalized_old, &normalized_new);

    let mut pairs = Vec::new();
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
    let old_graphemes: Vec<&str> = old_comparable.text.graphemes(true).collect();
    let new_graphemes: Vec<&str> = new_comparable.text.graphemes(true).collect();
    let operations = capture_diff_slices(Algorithm::Myers, &old_graphemes, &new_graphemes);
    let mut old_changed = Vec::new();
    let mut new_changed = Vec::new();

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
            return Self {
                text: input.to_owned(),
                original_ranges: input
                    .grapheme_indices(true)
                    .map(|(start, grapheme)| start..start + grapheme.len())
                    .collect(),
            };
        }

        let (text, pieces) = normalize_with_mapping(input);
        let mut original_ranges = Vec::new();
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
    let content = &input[content_start..content_end];
    let mut normalized = String::with_capacity(content.len());
    let mut pieces = Vec::new();
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
    if whitespace == WhitespaceChoice::Show {
        return input.to_owned();
    }
    normalize_with_mapping(input).0
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
}
