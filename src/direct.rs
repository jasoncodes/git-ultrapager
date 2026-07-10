//! Alternate screen を使わない direct renderer と共有 visual-row layout。
//!
//! `DocumentLayout` は terminal width と tab width から deterministic な visual row を作り、direct
//! output と interactive viewport が同じ折り返し・gutterを使う。syntax/search/intraline style は
//! byte span layerとして後から合成するため、ここでは構造 roleとstable IDだけを保持する。

use std::collections::HashMap;
use std::io::{self, Write};
use std::ops::Range;
use std::sync::Arc;

use ratatui::style::{Color, Modifier, Style};
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::decorations::{DecorationSet, LineDecoration};
use crate::model::{CommitId, Document, DocumentLine, FileId, HunkId, LineId, LineKind};
use crate::text::wrap_hard;
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
/// `layout_document`は同じrow生成関数へ自身の連続bufferを渡し、per-lineの`Arc`を作らない。
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

/// Sanitized済みの1 logical lineをterminal widthへhard-wrapする。
///
/// この関数は`Document`や前後行を参照しない。Source行のgutter幅はfile全体で揃える必要が
/// あるため、呼び出し側が同じfileから算出した`GutterSpec`を明示的に渡す。`tab_width == 0`
/// は`InvalidTabWidth`、現在行の番号を収められないgutterは`UndersizedGutter`、本文を1cell
/// も確保できないterminal幅は`TooNarrow`を返す。
pub fn layout_line(
    line: &DocumentLine,
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

/// Publicなper-line layoutとdocument-wide layoutが共有する唯一のrow生成経路。
///
/// 呼び出し側が所有する`Vec`へ直接追加するため、`layout_document`はlogical lineごとの一時
/// `Vec`と`Arc`を作らない。失敗したlineより前に追加済みのrowは残るが、この関数はmodule内
/// だけで使い、公開APIは失敗時に構築途中のbufferを外へ返さない。
fn append_line_rows(
    rows: &mut Vec<VisualRow>,
    line: &DocumentLine,
    width: usize,
    tab_width: usize,
    gutter: GutterSpec,
) -> Result<(), LayoutError> {
    validate_tab_width(tab_width)?;
    if is_source_line(line.kind) {
        validate_gutter(line, gutter)?;
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

/// Document を terminal width へhard-wrapする。
pub fn layout_document(
    document: &Document,
    width: usize,
    tab_width: usize,
) -> Result<DocumentLayout, LayoutError> {
    // Empty documentでも無効なlayout parameterを成功扱いにしない。これにより入力内容によって
    // 同じtab幅の成否が変わらず、`layout_line`とfailure contractが揃う。
    validate_tab_width(tab_width)?;
    let gutter_specs = file_gutter_specs(document);
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

        let gutter = line
            .file_id
            .and_then(|id| gutter_specs.get(&id).copied())
            .unwrap_or_default();
        append_line_rows(&mut layout.rows, line, width, tab_width, gutter)?;
    }
    Ok(layout)
}

fn validate_tab_width(tab_width: usize) -> Result<(), LayoutError> {
    if tab_width == 0 {
        return Err(LayoutError::InvalidTabWidth { actual: tab_width });
    }
    Ok(())
}

fn validate_gutter(line: &DocumentLine, gutter: GutterSpec) -> Result<(), LayoutError> {
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

fn file_gutter_specs(document: &Document) -> HashMap<FileId, GutterSpec> {
    let mut widths = HashMap::new();
    for line in document.lines() {
        let Some(file_id) = line.file_id else {
            continue;
        };
        let entry = widths.entry(file_id).or_insert(GutterSpec::MINIMUM);
        if let Some(number) = line.old_line_number {
            entry.old_width = entry.old_width.max(decimal_width(number));
        }
        if let Some(number) = line.new_line_number {
            entry.new_width = entry.new_width.max(decimal_width(number));
        }
    }
    widths
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
    write_direct_rows(writer, layout.rows(), theme)
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

/// Syntax・intralineを合成し、指定logical-line範囲のvisual rowだけをTTYへ書く。
///
/// Precedenceはinteractive rendererと同じく、diff構造、syntax foreground、intraline backgroundの
/// 順とする。Searchはinteractive stateだけのためdirect outputへは含めない。
pub fn write_direct_decorated_range<W: Write>(
    writer: &mut W,
    layout: &DocumentLayout,
    document: &Document,
    decorations: &DecorationSet,
    theme: &Theme,
    line_range: Range<usize>,
) -> io::Result<()> {
    for (row_index, row) in layout.rows().iter().enumerate() {
        let Ok(line_index) = usize::try_from(row.line_id.get()) else {
            continue;
        };
        if !line_range.contains(&line_index) {
            continue;
        }
        write_decorated_row(writer, layout, document, decorations, theme, row_index, row)?;
    }
    writer.flush()
}

fn write_decorated_row<W: Write>(
    writer: &mut W,
    layout: &DocumentLayout,
    document: &Document,
    decorations: &DecorationSet,
    theme: &Theme,
    row_index: usize,
    row: &VisualRow,
) -> io::Result<()> {
    let Some(line) = document.line(row.line_id) else {
        write_style(writer, theme.style(row.role))?;
        writer.write_all(row.text.as_bytes())?;
        return writer.write_all(b"\x1b[0m\n");
    };
    let decoration = decorations.line(row.line_id);
    let syntax = direct_syntax_ranges(row, decoration);
    let intraline = direct_intraline_ranges(row, line, decoration);
    let replacements =
        direct_whitespace_replacements(layout, row, row_index, line, decoration, decorations);
    let gutter_end = if is_source_line(line.kind) {
        row.rendered_content_range.start.saturating_sub(1)
    } else {
        0
    };
    let mut boundaries = vec![0, row.text.len(), gutter_end];
    for (range, _) in &syntax {
        boundaries.extend([range.start, range.end]);
    }
    for range in &intraline {
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
            let role = match line.kind {
                LineKind::Deletion => SemanticRole::IntralineRemoved,
                LineKind::Addition => SemanticRole::IntralineAdded,
                _ => row.role,
            };
            style = style.patch(theme.style(role));
        }
        write_style(writer, style)?;
        let rendered = replace_direct_whitespace(&row.text, range, &replacements);
        writer.write_all(rendered.as_bytes())?;
    }
    writer.write_all(b"\x1b[0m\n")
}

fn direct_syntax_ranges<'decoration>(
    row: &VisualRow,
    decoration: Option<&'decoration LineDecoration>,
) -> Vec<(Range<usize>, &'decoration crate::highlight::SyntaxSpan)> {
    decoration
        .into_iter()
        .flat_map(LineDecoration::syntax_spans)
        .flat_map(|span| {
            row.rendered_ranges_for_logical(span.byte_range.clone())
                .into_iter()
                .map(move |range| (range, span))
        })
        .collect()
}

fn direct_intraline_ranges(
    row: &VisualRow,
    line: &DocumentLine,
    decoration: Option<&LineDecoration>,
) -> Vec<Range<usize>> {
    if !matches!(line.kind, LineKind::Deletion | LineKind::Addition) {
        return Vec::new();
    }
    decoration
        .into_iter()
        .flat_map(LineDecoration::intraline_ranges)
        .flat_map(|range| row.rendered_ranges_for_logical(range.clone()))
        .collect()
}

fn direct_whitespace_replacements(
    layout: &DocumentLayout,
    row: &VisualRow,
    row_index: usize,
    line: &DocumentLine,
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
    for changed in decoration.intraline_ranges() {
        let Some(text) = line.text.get(changed.clone()) else {
            continue;
        };
        for (relative, character) in text.char_indices() {
            if !matches!(character, ' ' | '\t') {
                continue;
            }
            let logical = changed.start + relative;
            let first_row = layout.row_for_anchor(line.id, logical) == Some(row_index);
            for mapped in row.rendered_ranges_for_logical(logical..logical + character.len_utf8()) {
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
/// Source lineはfile entry単位のgutter確定が必要なので、このAPIへ渡さない。
pub fn write_direct_metadata_lines<W: Write>(
    writer: &mut W,
    lines: &[DocumentLine],
    width: usize,
    tab_width: usize,
    theme: &Theme,
) -> Result<(), DirectWriteError> {
    let mut rows = Vec::new();
    for line in lines {
        append_metadata_rows(&mut rows, line, width, tab_width)?;
    }
    write_direct_rows(writer, &rows, theme)?;
    Ok(())
}

/// Streaming direct layoutまたは出力のfailure。
#[derive(Debug, Error)]
pub enum DirectWriteError {
    #[error(transparent)]
    Layout(#[from] LayoutError),
    #[error(transparent)]
    Io(#[from] io::Error),
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
    use crate::parser::parse_bytes;
    use crate::theme::ColorLevel;

    #[test]
    fn gutter_spec_is_compact_and_keeps_the_minimum_alignment() {
        assert_eq!(std::mem::size_of::<GutterSpec>(), 2);
        assert_eq!(GutterSpec::new(0, 2), GutterSpec::MINIMUM);
        assert_eq!(GutterSpec::new(4, 7).old_width(), 4);
        assert_eq!(GutterSpec::new(4, 7).new_width(), 7);
    }

    #[test]
    fn zero_tab_width_is_rejected_for_tab_free_and_empty_documents() {
        let document = parse_bytes(b"metadata without tabs\n");
        let expected = LayoutError::InvalidTabWidth { actual: 0 };
        assert_eq!(
            layout_line(&document.lines()[0], 80, 0, GutterSpec::MINIMUM).unwrap_err(),
            expected,
        );
        assert_eq!(layout_document(&document, 80, 0).unwrap_err(), expected);

        let empty = parse_bytes(b"");
        assert_eq!(layout_document(&empty, 80, 0).unwrap_err(), expected);
    }

    #[test]
    fn line_layout_rejects_gutters_that_cannot_hold_current_line_numbers() {
        let document = parse_bytes(
            b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -12345 +54321 @@\n value\n",
        );
        let context = document
            .lines()
            .iter()
            .find(|line| line.kind == LineKind::Context)
            .unwrap();

        assert_eq!(
            layout_line(context, 0, 4, GutterSpec::new(3, 4)).unwrap_err(),
            LayoutError::UndersizedGutter {
                old_width: 3,
                required_old_width: 5,
                new_width: 4,
                required_new_width: 5,
            }
        );
    }

    #[test]
    fn line_layout_matches_document_layout_for_metadata_source_tabs_and_unicode() {
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
        let whole = layout_document(&document, width, tab_width).unwrap();
        let gutter_specs = file_gutter_specs(&document);
        let mut per_line = Vec::new();

        for line in document.lines() {
            let gutter = line
                .file_id
                .and_then(|id| gutter_specs.get(&id).copied())
                .unwrap_or_default();
            let line_layout = layout_line(line, width, tab_width, gutter).unwrap();
            let cloned = line_layout.clone();
            assert_eq!(line_layout.rows().as_ptr(), cloned.rows().as_ptr());

            let first = whole.row_for_line(line.id).unwrap();
            let count = whole.rows()[first..]
                .iter()
                .take_while(|row| row.line_id == line.id)
                .count();
            assert_eq!(line_layout.rows(), &whole.rows()[first..first + count]);
            per_line.push(line_layout);
        }

        let metadata = &per_line[0];
        assert_eq!(metadata.rows()[0].text, "ab  界cd");

        let deletion = document
            .lines()
            .iter()
            .find(|line| line.kind == LineKind::Deletion)
            .unwrap();
        let deletion_layout = &per_line[deletion.id.index()];
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
        let mut whole_output = Vec::new();
        write_direct(&mut whole_output, &whole, &theme).unwrap();
        let mut per_line_output = Vec::new();
        write_direct_rows(
            &mut per_line_output,
            per_line
                .iter()
                .flat_map(|line_layout| line_layout.rows().iter()),
            &theme,
        )
        .unwrap();
        assert_eq!(per_line_output, whole_output);
    }

    #[test]
    fn line_layout_reports_the_same_too_narrow_errors_as_document_layout() {
        let metadata = parse_bytes(b"metadata\n");
        assert_eq!(
            layout_line(
                &metadata.lines()[0],
                0,
                4,
                GutterSpec::new(u8::MAX, u8::MAX),
            ),
            Err(LayoutError::TooNarrow {
                actual: 0,
                required: 1,
            })
        );
        assert_eq!(
            layout_document(&metadata, 0, 4).unwrap_err(),
            LayoutError::TooNarrow {
                actual: 0,
                required: 1,
            }
        );

        let source = parse_bytes(
            b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +12345 @@\n+value\n",
        );
        let addition = source
            .lines()
            .iter()
            .find(|line| line.kind == LineKind::Addition)
            .unwrap();
        let expected = LayoutError::TooNarrow {
            actual: 12,
            required: 13,
        };
        assert_eq!(
            layout_line(addition, 12, 4, GutterSpec::new(3, 5)).unwrap_err(),
            expected,
        );
        assert_eq!(layout_document(&source, 12, 4).unwrap_err(), expected);
    }

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

    #[test]
    fn decorated_direct_output_visualizes_changed_whitespace() {
        let document = parse_bytes(
            b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-\tfoo\n+    foo\n",
        );
        let layout = layout_document(&document, 60, 4).unwrap();
        let theme = Theme::new(crate::cli::ThemeChoice::Dark, ColorLevel::None);
        let decorations = DecorationSet::compute(&document, &theme, WhitespaceChoice::Show);
        let mut output = Vec::new();
        write_direct_decorated_range(
            &mut output,
            &layout,
            &document,
            &decorations,
            &theme,
            0..document.lines().len(),
        )
        .unwrap();
        let plain = crate::ansi::strip_ansi(&output);
        let rendered = String::from_utf8(plain).unwrap();
        assert!(rendered.contains("-→···foo"), "{rendered}");
        assert!(rendered.contains("+····foo"), "{rendered}");
    }
}
