//! Invalid UTF-8、control byte、tab、Unicode width、hard wrapping の共通処理。
//!
//! 入力 byte と画面 cell の対応はこの module が所有する。terminal escape sequence の除去は
//! `ansi` layer が先に行い、この module は残った byte を表示可能な text へ変換する。変換後の
//! grapheme offset は search、viewport anchor、style span で共有するため、renderer 固有の補正を
//! 追加してはならない。

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub use crate::ansi::escape_interactive_bytes;

/// Syntax highlighting を行わず lazy wrapping へ切り替える logical-line byte 数。
pub const MAX_SYNTAX_LINE_BYTES: usize = 1024 * 1024;

/// Regex search から除外する logical-line byte 数。
pub const MAX_SEARCH_LINE_BYTES: usize = 16 * 1024 * 1024;

/// 1 grapheme がこの byte 数を超えた場合、Unicode scalar 境界で表示単位を分割する。
pub const MAX_GRAPHEME_BYTES: usize = 64 * 1024;

/// 画面上の1 visual rowを構成する source text の範囲。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrappedSegment {
    /// Tab 展開と control escape を終えた text 内の byte range。
    pub byte_range: Range<usize>,
    /// Segment が占有する terminal cell 数。
    pub width: usize,
}

/// Source 本文の tab を、本文先頭を column 0 とする tab stop へ展開する。
#[must_use]
pub fn expand_tabs(input: &str, tab_width: usize) -> String {
    debug_assert!(tab_width > 0);

    let mut output = String::with_capacity(input.len());
    let mut column = 0;
    for grapheme in input.graphemes(true) {
        if grapheme == "\t" {
            let spaces = tab_width - (column % tab_width);
            output.extend(std::iter::repeat_n(' ', spaces));
            column += spaces;
            continue;
        }

        output.push_str(grapheme);
        column += UnicodeWidthStr::width(grapheme);
    }
    output
}

/// Text を terminal cell 幅で hard wrap する。
///
/// Word boundary は考慮しない。幅0の combining grapheme は直前 segment に残し、1 grapheme が
/// `MAX_GRAPHEME_BYTES` を超えた場合だけ scalar 境界へ分割する。`available_width == 0` は表示不能
/// なので空 listを返す。
#[must_use]
pub fn wrap_hard(input: &str, available_width: usize) -> Vec<WrappedSegment> {
    if available_width == 0 || input.is_empty() {
        return if input.is_empty() {
            vec![WrappedSegment {
                byte_range: 0..0,
                width: 0,
            }]
        } else {
            Vec::new()
        };
    }

    let mut segments = Vec::new();
    let mut start = 0;
    let mut end = 0;
    let mut width: usize = 0;

    for (grapheme_start, grapheme) in input.grapheme_indices(true) {
        let grapheme_end = grapheme_start + grapheme.len();
        if grapheme.len() <= MAX_GRAPHEME_BYTES {
            push_display_unit(
                input,
                grapheme_start..grapheme_end,
                available_width,
                &mut segments,
                &mut start,
                &mut end,
                &mut width,
            );
            continue;
        }

        let mut chunk_start = grapheme_start;
        for (relative, character) in grapheme.char_indices() {
            let character_start = grapheme_start + relative;
            let character_end = character_start + character.len_utf8();
            if character_end - chunk_start > MAX_GRAPHEME_BYTES && character_start > chunk_start {
                push_display_unit(
                    input,
                    chunk_start..character_start,
                    available_width,
                    &mut segments,
                    &mut start,
                    &mut end,
                    &mut width,
                );
                chunk_start = character_start;
            }
        }
        if chunk_start < grapheme_end {
            push_display_unit(
                input,
                chunk_start..grapheme_end,
                available_width,
                &mut segments,
                &mut start,
                &mut end,
                &mut width,
            );
        }
    }

    if start < input.len() || segments.is_empty() {
        segments.push(WrappedSegment {
            byte_range: start..input.len(),
            width,
        });
    }
    segments
}

fn push_display_unit(
    input: &str,
    unit: Range<usize>,
    available_width: usize,
    segments: &mut Vec<WrappedSegment>,
    start: &mut usize,
    end: &mut usize,
    width: &mut usize,
) {
    let unit_width = UnicodeWidthStr::width(&input[unit.clone()]);
    if *width > 0 && width.saturating_add(unit_width) > available_width {
        segments.push(WrappedSegment {
            byte_range: *start..*end,
            width: *width,
        });
        *start = unit.start;
        *width = 0;
    }

    *end = unit.end;
    *width = width.saturating_add(unit_width);

    // Terminal幅より広い単一unitはそれ以上分割できないため、独立segmentとして確定する。
    if *width >= available_width {
        segments.push(WrappedSegment {
            byte_range: *start..*end,
            width: *width,
        });
        *start = *end;
        *width = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_utf8_and_controls_are_reversible_escapes() {
        let actual = escape_interactive_bytes(b"a\x00\xff\tb\r\n");
        assert_eq!(actual, "a\\x00\\xFF\tb\\x0D\n");
    }

    #[test]
    fn tabs_use_source_relative_stops() {
        assert_eq!(expand_tabs("a\tb\t", 4), "a   b   ");
    }

    #[test]
    fn hard_wrap_counts_wide_graphemes() {
        let text = "ab界cd";
        let segments = wrap_hard(text, 4);
        let rendered: Vec<&str> = segments
            .iter()
            .map(|segment| &text[segment.byte_range.clone()])
            .collect();
        assert_eq!(rendered, vec!["ab界", "cd"]);
    }

    #[test]
    fn empty_line_still_has_one_visual_row() {
        assert_eq!(
            wrap_hard("", 80),
            vec![WrappedSegment {
                byte_range: 0..0,
                width: 0
            }]
        );
    }
}
