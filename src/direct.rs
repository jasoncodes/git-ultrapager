//! Alternate screen を使わない direct renderer と共有 visual-row layout。
//!
//! `DocumentLayout` は terminal width と tab width から deterministic な visual row を作り、direct
//! output と interactive viewport が同じ折り返し・gutterを使う。syntax/search/intraline style は
//! byte span layerとして後から合成するため、ここでは構造 roleとstable IDだけを保持する。

use std::collections::HashMap;
use std::io::{self, Write};
use std::ops::Range;

use ratatui::style::{Color, Modifier, Style};
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::model::{CommitId, Document, DocumentLine, FileId, HunkId, LineId, LineKind};
use crate::text::wrap_hard;
use crate::theme::{SemanticRole, Theme};

/// Layout がsource本文1cellを確保できない場合のfailure。
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum LayoutError {
    #[error("terminal width {actual} is smaller than the required width {required}")]
    TooNarrow { actual: usize, required: usize },
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct SpanMapping {
    logical: Range<usize>,
    rendered: Range<usize>,
}

/// Document を特定幅へreflowしたimmutable layout。
#[derive(Clone, Debug, Default)]
pub struct DocumentLayout {
    rows: Vec<VisualRow>,
    first_row_by_line: HashMap<LineId, usize>,
    first_row_by_file: HashMap<FileId, usize>,
    first_row_by_hunk: HashMap<HunkId, usize>,
    first_row_by_commit: HashMap<CommitId, usize>,
}

impl DocumentLayout {
    #[must_use]
    pub fn rows(&self) -> &[VisualRow] {
        &self.rows
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    #[must_use]
    pub fn row_for_line(&self, id: LineId) -> Option<usize> {
        self.first_row_by_line.get(&id).copied()
    }

    #[must_use]
    pub fn row_for_file(&self, id: FileId) -> Option<usize> {
        self.first_row_by_file.get(&id).copied()
    }

    #[must_use]
    pub fn row_for_hunk(&self, id: HunkId) -> Option<usize> {
        self.first_row_by_hunk.get(&id).copied()
    }

    #[must_use]
    pub fn row_for_commit(&self, id: CommitId) -> Option<usize> {
        self.first_row_by_commit.get(&id).copied()
    }

    /// Reflow 前の anchor line とcontent byte offsetを含むrowを探す。
    #[must_use]
    pub fn row_for_anchor(&self, line: LineId, byte_offset: usize) -> Option<usize> {
        let start = self.row_for_line(line)?;
        let relative_end = self.rows[start..]
            .iter()
            .position(|row| row.line_id != line)
            .unwrap_or(self.rows.len() - start);
        let rows = &self.rows[start..start + relative_end];
        rows.iter()
            .position(|row| {
                row.content_range.contains(&byte_offset)
                    || (row.content_range.is_empty() && byte_offset == row.content_range.start)
            })
            .map(|relative| start + relative)
            .or_else(|| {
                rows.last()
                    .filter(|row| row.content_range.end == byte_offset)
                    .map(|_| start + rows.len().saturating_sub(1))
            })
            .or(Some(start))
    }
}

impl VisualRow {
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
}

/// Document を terminal width へhard-wrapする。
pub fn layout_document(
    document: &Document,
    width: usize,
    tab_width: usize,
) -> Result<DocumentLayout, LayoutError> {
    let gutter_widths = file_gutter_widths(document);
    let mut layout = DocumentLayout::default();

    for line in document.lines() {
        let row_start = layout.rows.len();
        layout.first_row_by_line.insert(line.id, row_start);
        if let Some(id) = line.commit_id {
            layout.first_row_by_commit.entry(id).or_insert(row_start);
        }
        if let Some(id) = line.file_id {
            layout.first_row_by_file.entry(id).or_insert(row_start);
        }
        if let Some(id) = line.hunk_id {
            layout.first_row_by_hunk.entry(id).or_insert(row_start);
        }

        if is_source_line(line.kind) {
            let (old_width, new_width) = line
                .file_id
                .and_then(|id| gutter_widths.get(&id).copied())
                .unwrap_or((3, 3));
            append_source_rows(
                &mut layout.rows,
                line,
                width,
                tab_width,
                old_width,
                new_width,
            )?;
        } else {
            append_metadata_rows(&mut layout.rows, line, width, tab_width)?;
        }
    }
    Ok(layout)
}

fn append_source_rows(
    rows: &mut Vec<VisualRow>,
    line: &DocumentLine,
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
    line: &DocumentLine,
    width: usize,
    tab_width: usize,
) -> Result<(), LayoutError> {
    if width == 0 {
        return Err(LayoutError::TooNarrow {
            actual: 0,
            required: 1,
        });
    }
    let expanded = expand_tabs_with_mapping(&line.text, tab_width);
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
            });
        }
        let rendered_start = text.len();
        let spaces = tab_width - (column % tab_width);
        text.extend(std::iter::repeat_n(' ', spaces));
        column += spaces;
        mappings.push(SpanMapping {
            logical: logical_start..logical_start + grapheme.len(),
            rendered: rendered_start..text.len(),
        });
        plain_start = logical_start + grapheme.len();
    }

    if plain_start < input.len() {
        let byte_count = input.len() - plain_start;
        mappings.push(SpanMapping {
            logical: plain_start..input.len(),
            rendered: text.len() - byte_count..text.len(),
        });
    }
    if input.is_empty() {
        mappings.push(SpanMapping {
            logical: 0..0,
            rendered: 0..0,
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
            })
        })
        .collect()
}

fn mapping_bounds(mappings: &[SpanMapping]) -> Option<Range<usize>> {
    Some(mappings.first()?.logical.start..mappings.last()?.logical.end)
}

fn file_gutter_widths(document: &Document) -> HashMap<FileId, (usize, usize)> {
    let mut widths = HashMap::new();
    for line in document.lines() {
        let Some(file_id) = line.file_id else {
            continue;
        };
        let entry = widths.entry(file_id).or_insert((3, 3));
        if let Some(number) = line.old_line_number {
            entry.0 = entry.0.max(decimal_width(number));
        }
        if let Some(number) = line.new_line_number {
            entry.1 = entry.1.max(decimal_width(number));
        }
    }
    widths
}

fn decimal_width(number: u64) -> usize {
    number.checked_ilog10().map_or(1, |log| log as usize + 1)
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
        LineKind::Fallback | LineKind::Binary | LineKind::Submodule => SemanticRole::Dim,
        _ => SemanticRole::Plain,
    }
}

/// Visual rows をTTYへ順に書き、各行の末尾でstyleをresetする。
pub fn write_direct<W: Write>(
    writer: &mut W,
    layout: &DocumentLayout,
    theme: &Theme,
) -> io::Result<()> {
    for row in layout.rows() {
        write_style(writer, theme.style(row.role))?;
        writer.write_all(row.text.as_bytes())?;
        writer.write_all(b"\x1b[0m\n")?;
    }
    writer.flush()
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
    use crate::parser::parse_bytes;
    use crate::theme::ColorLevel;

    #[test]
    fn source_rows_have_dual_numbers_and_blank_continuations() {
        let document = parse_bytes(
            b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -10,1 +10,1 @@\n-abcdefgh\n+abcdefXY\n",
        );
        let layout = layout_document(&document, 14, 4).unwrap();
        let deletion_rows: Vec<&VisualRow> = layout
            .rows()
            .iter()
            .filter(|row| row.role == SemanticRole::Removed)
            .collect();
        assert!(deletion_rows[0].text.starts_with(" 10     │-"));
        assert!(deletion_rows[1].text.starts_with("        │ "));
    }

    #[test]
    fn metadata_uses_full_width() {
        let document = parse_bytes(b"plain metadata line\n");
        let layout = layout_document(&document, 8, 4).unwrap();
        assert_eq!(layout.rows()[0].text, "plain me");
        assert_eq!(layout.rows()[1].text, "tadata l");
    }

    #[test]
    fn tab_span_maps_to_every_wrapped_expansion_cell() {
        let document = parse_bytes(
            b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -0,0 +1,1 @@\n+\tfoo\n",
        );
        let layout = layout_document(&document, 12, 4).unwrap();
        let addition_rows: Vec<&VisualRow> = layout
            .rows()
            .iter()
            .filter(|row| row.role == SemanticRole::Added)
            .collect();

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

        let addition = document
            .lines()
            .iter()
            .find(|line| line.kind == LineKind::Addition)
            .unwrap();
        let row = layout.row_for_anchor(addition.id, 2).unwrap();
        assert!(layout.rows()[row].text.ends_with("fo"));
    }

    #[test]
    fn colorless_direct_output_still_resets_attributes() {
        let document = parse_bytes(b"hello\n");
        let layout = layout_document(&document, 80, 4).unwrap();
        let mut output = Vec::new();
        write_direct(
            &mut output,
            &layout,
            &Theme::new(crate::cli::ThemeChoice::Dark, ColorLevel::None),
        )
        .unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "\x1b[0mhello\x1b[0m\n");
    }
}
