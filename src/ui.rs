//! Interactive pager の画面構成と、semantic role から Ratatui widget への変換。
//!
//! この module はAppStateとDecorationSetのimmutable snapshotだけを読み、sticky header、本文、
//! status bar、help/file-list overlayを1 frameへ描く。入力event、terminal lifecycle、background workerは
//! 扱わない。Syntax foreground、intraline background、searchの順にstyleを重ね、全spanをlayoutが持つ
//! byte mappingでtab展開・折り返し後のvisual rowへ写す。

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::cli::WhitespaceChoice;
use crate::decorations::{DecorationSet, LineDecoration};
use crate::direct::VisualRow;
use crate::model::{Commit, CommitKind, DocumentLine, FileEntry, LineKind};
use crate::search::{CompiledSearch, IncrementalQueryStatus, SearchMatch};
use crate::state::{AppState, FileListState, Mode, SearchInputState, TextInput};
use crate::text::{MAX_SEARCH_LINE_BYTES, wrap_hard};
use crate::theme::{SemanticRole, Theme};

/// Weak matchは補助表示であり、current matchのnavigation contractには使わない。1 frameで保持する
/// range数を固定し、1 cellごとにmatchするpatternでもrendererのallocationを入力長へ比例させない。
const MAX_WEAK_MATCH_SPANS_PER_FRAME: usize = 16_384;
const MAX_WEAK_MATCH_LOOKBEHIND_BYTES: usize = 4 * 1024;

const HELP_LINES: [&str; 42] = [
    "git-ultrapager help",
    "",
    "Navigation",
    "  j, e, Down, Enter, Ctrl-E/N   down one visual row",
    "  k, y, Up, Ctrl-Y/P            up one visual row",
    "  Space, f, PageDown, Ctrl-F/V  down one page",
    "  b, PageUp, Ctrl-B             up one page",
    "  d, Ctrl-D             down half a page",
    "  u, Ctrl-U             up half a page",
    "  g, Home               beginning of input",
    "  G, End                loaded end / follow input",
    "  Ng, NG                input logical line N",
    "",
    "Structure",
    "  [                     previous hunk",
    "  ]                     next hunk",
    "  {                     previous file",
    "  }                     next file",
    "  (                     previous commit",
    "  )                     next commit",
    "",
    "Search",
    "  /                     forward regex search",
    "  ?                     backward regex search",
    "  n                     repeat in current direction",
    "  N                     repeat in opposite direction",
    "  Enter                 commit search input",
    "  Esc                   cancel search input",
    "",
    "Overlays",
    "  Tab                   open or close changed files",
    "  /                     filter paths in file list",
    "  h, H                  open or close this help",
    "  Esc, q                close the current overlay",
    "",
    "Other",
    "  W                     toggle whitespace comparison",
    "  Ctrl-L                repaint terminal",
    "  Ctrl-Z                suspend with job control",
    "  q                     quit from the pager body",
    "  Ctrl-C                interrupt and exit with 130",
    "  A numeric prefix repeats supported commands.",
];

/// 現在の state を frame 全体へ描く。
///
/// 呼び出し側は alternate screen の開始・終了と Terminal::draw を所有する。この関数は frame 外へ
/// 書き込まず、search/filter input 中だけ Ratatui の software cursor 位置を設定する。DecorationSetの
/// whitespace modeがstateと異なる間はstale intralineだけを隠し、mode非依存のsyntaxは表示を続ける。
pub fn render(frame: &mut Frame<'_>, state: &AppState, theme: &Theme, decorations: &DecorationSet) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(theme.style(SemanticRole::Plain)),
        area,
    );

    if area.height < 5 || area.width < 11 || state.layout_error().is_some() {
        render_too_small(frame, area, theme);
        return;
    }

    let status_area = Rect::new(
        area.x,
        area.y.saturating_add(area.height.saturating_sub(1)),
        area.width,
        1,
    );
    let content_area = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));

    match state.mode() {
        Mode::Body | Mode::Search(_) => {
            render_body(frame, content_area, state, theme, decorations);
        }
        Mode::Help { top } => render_help(frame, content_area, *top, theme),
        Mode::FileList(list) => render_file_list(frame, content_area, state, list, theme),
    }
    render_status(frame, status_area, state, theme);
}

fn render_too_small(frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let message = fit_end("terminal too small", usize::from(area.width));
    let y = area.y.saturating_add(area.height / 2);
    frame.render_widget(
        Paragraph::new(message)
            .style(theme.style(SemanticRole::Error))
            .alignment(ratatui::layout::Alignment::Center),
        Rect::new(area.x, y, area.width, u16::from(area.height > 0)),
    );
}

fn render_body(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    decorations: &DecorationSet,
) {
    let header_area = Rect::new(area.x, area.y, area.width, 1);
    let body_area = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1),
    );
    let header = sticky_header(state, usize::from(header_area.width));
    frame.render_widget(
        Paragraph::new(header).style(theme.style(SemanticRole::Sticky)),
        header_area,
    );

    let start = state.viewport_top();
    let rows = state.layout().rows();
    let visible_end = start
        .saturating_add(usize::from(body_area.height))
        .min(rows.len());
    let visible_rows = rows.get(start..visible_end).unwrap_or_default();
    let compiled = displayed_search(state);
    let visible_ranges = visible_logical_ranges(visible_rows);
    let mut matches_by_line = HashMap::new();
    let mut remaining_weak_matches = MAX_WEAK_MATCH_SPANS_PER_FRAME;
    for (screen_row, row) in visible_rows.iter().enumerate() {
        let y = body_area.y.saturating_add(screen_row as u16);
        let weak_matches = compiled.map_or(&[][..], |compiled| {
            matches_by_line
                .entry(row.line_id)
                .or_insert_with(|| {
                    let Some(line) = state.document().line(row.line_id) else {
                        return Vec::new();
                    };
                    let visible = visible_ranges.get(&row.line_id).cloned().unwrap_or(0..0);
                    weak_matches_in_visible_range(
                        compiled,
                        &line.text,
                        visible,
                        &mut remaining_weak_matches,
                    )
                })
                .as_slice()
        });
        frame.render_widget(
            Paragraph::new(render_visual_row(
                state,
                row,
                start + screen_row,
                weak_matches,
                theme,
                decorations,
            )),
            Rect::new(body_area.x, y, body_area.width, 1),
        );
    }
}

fn visible_logical_ranges(rows: &[VisualRow]) -> HashMap<crate::model::LineId, Range<usize>> {
    let mut ranges = HashMap::<_, Range<usize>>::new();
    for row in rows {
        ranges
            .entry(row.line_id)
            .and_modify(|range| {
                range.start = range.start.min(row.content_range.start);
                range.end = range.end.max(row.content_range.end);
            })
            .or_insert_with(|| row.content_range.clone());
    }
    ranges
}

fn weak_matches_in_visible_range(
    compiled: &CompiledSearch,
    text: &str,
    visible: Range<usize>,
    remaining: &mut usize,
) -> Vec<Range<usize>> {
    if text.len() > MAX_SEARCH_LINE_BYTES || visible.is_empty() || *remaining == 0 {
        return Vec::new();
    }

    // Full logical lineの先頭から毎frame走査すると、末尾viewportの`.`でも16 MiB分のmatchを辿る。
    // Visible直前4 KiBだけをlook-behindとして残し、haystack末尾もvisible endへ切る。Weak layerは
    // current matchの補助表示なので、それより前から始まる巨大matchは省略してmain threadを守る。
    let mut scan_start = visible
        .start
        .saturating_sub(MAX_WEAK_MATCH_LOOKBEHIND_BYTES);
    while scan_start > 0 && !text.is_char_boundary(scan_start) {
        scan_start -= 1;
    }
    let mut scan_end = visible.end.min(text.len());
    while scan_end > 0 && !text.is_char_boundary(scan_end) {
        scan_end -= 1;
    }
    let haystack = &text[..scan_end];
    let mut cursor = scan_start.min(haystack.len());
    let mut matches = Vec::new();
    while cursor <= haystack.len() && matches.len() < *remaining {
        let Some(found) = compiled.regex().find_at(haystack, cursor) else {
            break;
        };
        if !found.is_empty() && found.end() > visible.start {
            matches.push(found.range());
        }
        if found.is_empty() {
            let Some(character) = haystack
                .get(found.end()..)
                .and_then(|tail| tail.chars().next())
            else {
                break;
            };
            cursor = found.end().saturating_add(character.len_utf8());
        } else {
            cursor = found.end();
        }
    }
    *remaining = remaining.saturating_sub(matches.len());
    matches
}

fn sticky_header(state: &AppState, width: usize) -> String {
    let Some(row) = state.layout().rows().get(state.viewport_top()) else {
        return fit_end("git-ultrapager", width);
    };
    if let Some(file_id) = row.file_id
        && let Some(file) = state.document().file(file_id)
    {
        return sticky_file_header(state, file, width);
    }
    if let Some(commit_id) = row.commit_id
        && let Some(commit) = state.document().commit(commit_id)
    {
        return fit_end(&commit_label(commit), width);
    }
    fit_end("git-ultrapager", width)
}

fn sticky_file_header(state: &AppState, file: &FileEntry, width: usize) -> String {
    let occurrence = state
        .document()
        .files()
        .iter()
        .position(|candidate| candidate.id == file.id)
        .map_or(1, |index| index + 1);
    let total = if state.document().is_complete() {
        state.document().files().len().to_string()
    } else {
        "?".to_owned()
    };
    let additions = file
        .additions
        .map_or_else(|| "?".to_owned(), |value| value.to_string());
    let deletions = file
        .deletions
        .map_or_else(|| "?".to_owned(), |value| value.to_string());
    let commit = (state.document().commits().len() > 1)
        .then(|| state.document().commit(file.commit_id))
        .flatten()
        .and_then(|commit| commit.object_id.as_deref())
        .map(short_object_id);
    let suffix = commit.map_or_else(
        || format!("  {occurrence}/{total}  +{additions} -{deletions}"),
        |object| format!("  {occurrence}/{total}  +{additions} -{deletions}  @{object}"),
    );
    let prefix = format!("{} ", file.status.label());
    let fixed_width = display_width(&prefix).saturating_add(display_width(&suffix));
    if fixed_width >= width {
        return fit_end(&format!("{prefix}{suffix}"), width);
    }
    let path_width = width - fixed_width;
    let path = fit_file_path(file, path_width);
    format!("{prefix}{path}{suffix}")
}

fn commit_label(commit: &Commit) -> String {
    let identity = match commit.kind {
        CommitKind::WorkingTree => "working tree".to_owned(),
        CommitKind::Commit => commit
            .object_id
            .as_deref()
            .map(short_object_id)
            .map_or_else(|| "commit".to_owned(), |object| format!("commit {object}")),
    };
    commit
        .subject
        .as_deref()
        .map_or(identity.clone(), |subject| format!("{identity}  {subject}"))
}

fn short_object_id(object: &str) -> &str {
    object.get(..object.len().min(8)).unwrap_or(object)
}

fn render_visual_row<'line>(
    state: &AppState,
    row: &'line VisualRow,
    row_index: usize,
    weak_matches: &[Range<usize>],
    theme: &Theme,
    decorations: &DecorationSet,
) -> Line<'line> {
    let Some(logical_line) = state.document().line(row.line_id) else {
        return Line::from(Span::styled(row.text.clone(), theme.style(row.role)));
    };
    if row.text.is_empty()
        && displayed_current_match(state).is_some_and(|current| {
            current.line_id == row.line_id
                && current.is_zero_width()
                && row_contains_logical_offset(state, row_index, current.byte_range.start)
        })
    {
        // Empty logical lineには既存cellがないためstyleだけでは位置を示せない。検索中だけ1 cellの
        // markerを描き、NO_COLORでもbold glyphとしてcurrent matchを見失わないようにする。
        return Line::from(Span::styled("▏", theme.style(SemanticRole::SearchCurrent)));
    }

    let gutter_end = source_content_start(row, logical_line)
        .map(|start| start.saturating_sub(1))
        .unwrap_or(0);
    let line_decoration = decorations.line(row.line_id);
    let mut syntax = syntax_overlays(row, line_decoration);
    syntax.sort_by_key(|overlay| (overlay.range.start, overlay.range.end));
    let mut intraline = if decorations.whitespace() == state.whitespace() {
        intraline_overlays(row, logical_line, line_decoration)
    } else {
        Vec::new()
    };
    intraline.sort_by_key(|overlay| (overlay.range.start, overlay.range.end));
    let search = search_overlays(state, row, row_index, logical_line, weak_matches);
    let replacements = if decorations.whitespace() == WhitespaceChoice::Show
        && decorations.whitespace() == state.whitespace()
    {
        whitespace_replacements(state, row, row_index, logical_line, line_decoration)
    } else {
        HashMap::new()
    };

    let mut boundaries = vec![0, row.text.len(), gutter_end];
    extend_boundaries(&mut boundaries, &syntax);
    extend_boundaries(&mut boundaries, &intraline);
    extend_boundaries(&mut boundaries, &search.weak);
    extend_boundaries(&mut boundaries, &search.current);
    boundaries.retain(|offset| *offset <= row.text.len() && row.text.is_char_boundary(*offset));
    boundaries.sort_unstable();
    boundaries.dedup();

    if boundaries.len() == 1 {
        return Line::from(Span::styled(row.text.clone(), theme.style(row.role)));
    }

    let mut syntax_cursor = 0_usize;
    let mut intraline_cursor = 0_usize;
    let mut weak_search_cursor = 0_usize;
    let mut current_search_cursor = 0_usize;
    let spans = boundaries
        .windows(2)
        .filter_map(|window| {
            let range = window[0]..window[1];
            if range.is_empty() {
                return None;
            }
            let base_role = if range.start < gutter_end {
                SemanticRole::LineNumber
            } else {
                row.role
            };
            let mut style = theme.style(base_role);
            if let Some(candidate) = covering_overlay(&syntax, &mut syntax_cursor, &range) {
                style = style.patch(theme.syntax_style(
                    candidate.foreground,
                    candidate.bold,
                    candidate.italic,
                    candidate.underline,
                ));
            }
            if let Some(candidate) = covering_overlay(&intraline, &mut intraline_cursor, &range) {
                style = style.patch(theme.style(candidate.role));
            }
            if let Some(candidate) = covering_overlay(&search.weak, &mut weak_search_cursor, &range)
            {
                style = style.patch(theme.style(candidate.role));
            }
            if let Some(candidate) =
                covering_overlay(&search.current, &mut current_search_cursor, &range)
            {
                style = style.patch(theme.style(candidate.role));
            }
            Some(Span::styled(
                decorated_segment(&row.text, range, &replacements),
                style,
            ))
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

#[derive(Clone, Debug)]
struct MappedSyntax {
    range: Range<usize>,
    foreground: (u8, u8, u8),
    bold: bool,
    italic: bool,
    underline: bool,
}

trait RangedOverlay {
    fn range(&self) -> &Range<usize>;
}

impl RangedOverlay for MappedSyntax {
    fn range(&self) -> &Range<usize> {
        &self.range
    }
}

fn extend_boundaries<T: RangedOverlay>(boundaries: &mut Vec<usize>, overlays: &[T]) {
    for overlay in overlays {
        boundaries.push(overlay.range().start);
        boundaries.push(overlay.range().end);
    }
}

fn covering_overlay<'overlay, T: RangedOverlay>(
    overlays: &'overlay [T],
    cursor: &mut usize,
    segment: &Range<usize>,
) -> Option<&'overlay T> {
    while overlays
        .get(*cursor)
        .is_some_and(|overlay| overlay.range().end <= segment.start)
    {
        *cursor += 1;
    }
    overlays.get(*cursor).filter(|overlay| {
        overlay.range().start <= segment.start && segment.end <= overlay.range().end
    })
}

fn syntax_overlays(row: &VisualRow, decoration: Option<&LineDecoration>) -> Vec<MappedSyntax> {
    let Some(decoration) = decoration else {
        return Vec::new();
    };
    let spans = decoration.syntax_spans();
    let first = spans.partition_point(|span| span.byte_range.end <= row.content_range.start);
    spans[first..]
        .iter()
        .take_while(|span| span.byte_range.start < row.content_range.end)
        .flat_map(|span| {
            row.rendered_ranges_for_logical(span.byte_range.clone())
                .into_iter()
                .map(|range| MappedSyntax {
                    range,
                    foreground: span.foreground,
                    bold: span.bold,
                    italic: span.italic,
                    underline: span.underline,
                })
        })
        .collect()
}

fn intraline_overlays(
    row: &VisualRow,
    line: &DocumentLine,
    decoration: Option<&LineDecoration>,
) -> Vec<StyledRange> {
    let role = match line.kind {
        LineKind::Deletion => SemanticRole::IntralineRemoved,
        LineKind::Addition => SemanticRole::IntralineAdded,
        _ => return Vec::new(),
    };
    let Some(decoration) = decoration else {
        return Vec::new();
    };
    let ranges = decoration.intraline_ranges();
    let first = ranges.partition_point(|range| range.end <= row.content_range.start);
    ranges[first..]
        .iter()
        .take_while(|range| range.start < row.content_range.end)
        .flat_map(|range| row.rendered_ranges_for_logical(range.clone()))
        .map(|range| StyledRange { range, role })
        .collect()
}

fn whitespace_replacements(
    state: &AppState,
    row: &VisualRow,
    row_index: usize,
    line: &DocumentLine,
    decoration: Option<&LineDecoration>,
) -> HashMap<usize, char> {
    let mut replacements = HashMap::new();
    let Some(decoration) = decoration else {
        return replacements;
    };
    let ranges = decoration.intraline_ranges();
    let first = ranges.partition_point(|range| range.end <= row.content_range.start);
    for changed in ranges[first..]
        .iter()
        .take_while(|range| range.start < row.content_range.end)
    {
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
            let first_visual_row =
                state.layout().row_for_anchor(line.id, logical) == Some(row_index);
            for mapped in row.rendered_ranges_for_logical(logical..logical + character.len_utf8()) {
                for rendered in mapped.clone() {
                    if row.text.as_bytes().get(rendered) != Some(&b' ') {
                        continue;
                    }
                    let glyph = if character == '\t' && first_visual_row && rendered == mapped.start
                    {
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

fn decorated_segment(
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

#[derive(Clone, Debug)]
struct StyledRange {
    range: Range<usize>,
    role: SemanticRole,
}

impl RangedOverlay for StyledRange {
    fn range(&self) -> &Range<usize> {
        &self.range
    }
}

#[derive(Clone, Debug, Default)]
struct SearchOverlays {
    weak: Vec<StyledRange>,
    current: Vec<StyledRange>,
}

fn search_overlays(
    state: &AppState,
    row: &VisualRow,
    row_index: usize,
    line: &DocumentLine,
    weak_matches: &[Range<usize>],
) -> SearchOverlays {
    let current = displayed_current_match(state);
    let mut overlays = SearchOverlays::default();

    let first = weak_matches.partition_point(|found| found.end <= row.content_range.start);
    for found in weak_matches[first..]
        .iter()
        .take_while(|found| found.start < row.content_range.end)
    {
        for mapped in map_logical_range(row, line, found.clone()) {
            overlays.weak.push(StyledRange {
                range: mapped,
                role: SemanticRole::SearchMatch,
            });
        }
    }

    if let Some(current) = current
        && current.line_id == row.line_id
    {
        if current.is_zero_width() {
            if row_contains_logical_offset(state, row_index, current.byte_range.start)
                && let Some(marker) = current_row_marker(row, line)
            {
                overlays.current.push(StyledRange {
                    range: marker,
                    role: SemanticRole::SearchCurrent,
                });
            }
        } else {
            for mapped in map_logical_range(row, line, current.byte_range.clone()) {
                overlays.current.push(StyledRange {
                    range: mapped,
                    role: SemanticRole::SearchCurrent,
                });
            }
        }
    }
    overlays
        .weak
        .sort_by_key(|overlay| (overlay.range.start, overlay.range.end));
    overlays
        .current
        .sort_by_key(|overlay| (overlay.range.start, overlay.range.end));
    overlays
}

fn displayed_search(state: &AppState) -> Option<&CompiledSearch> {
    if matches!(state.mode(), Mode::Search(_))
        && let Some(compiled) = state
            .search()
            .incremental()
            .and_then(|query| query.displayed_compiled())
    {
        return Some(compiled);
    }
    state.search().active().map(|active| active.compiled())
}

fn displayed_current_match(state: &AppState) -> Option<&SearchMatch<crate::model::LineId>> {
    if matches!(state.mode(), Mode::Search(_))
        && let Some(preview) = state
            .search()
            .incremental()
            .and_then(|query| query.displayed_preview())
    {
        return preview.found.as_ref();
    }
    state.search().current_match()
}

fn map_logical_range(
    row: &VisualRow,
    _line: &DocumentLine,
    logical: Range<usize>,
) -> Vec<Range<usize>> {
    row.rendered_ranges_for_logical(logical)
}

fn source_content_start(row: &VisualRow, line: &DocumentLine) -> Option<usize> {
    if !is_source(line.kind) {
        return None;
    }
    Some(row.rendered_content_range.start)
}

fn current_row_marker(row: &VisualRow, line: &DocumentLine) -> Option<Range<usize>> {
    if let Some(content_start) = source_content_start(row, line) {
        let start = content_start.saturating_sub(1);
        return next_grapheme_range(&row.text, start);
    }
    next_grapheme_range(&row.text, 0)
}

fn row_contains_logical_offset(state: &AppState, row_index: usize, offset: usize) -> bool {
    let Some(row) = state.layout().rows().get(row_index) else {
        return false;
    };
    if state.document().line(row.line_id).is_none() {
        return false;
    }
    state.layout().row_for_anchor(row.line_id, offset) == Some(row_index)
}

fn next_grapheme_range(text: &str, start: usize) -> Option<Range<usize>> {
    let suffix = text.get(start..)?;
    suffix
        .graphemes(true)
        .next()
        .map(|grapheme| start..start + grapheme.len())
}

fn is_source(kind: LineKind) -> bool {
    matches!(
        kind,
        LineKind::Context | LineKind::Deletion | LineKind::Addition
    )
}

fn render_help(frame: &mut Frame<'_>, area: Rect, top: usize, theme: &Theme) {
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(theme.style(SemanticRole::Plain)),
        area,
    );
    for (screen_row, text) in HELP_LINES
        .iter()
        .skip(top)
        .take(usize::from(area.height))
        .enumerate()
    {
        let role = if matches!(
            *text,
            "Navigation" | "Structure" | "Search" | "Overlays" | "Other"
        ) {
            SemanticRole::HunkHeader
        } else if screen_row == 0 && top == 0 {
            SemanticRole::Sticky
        } else {
            SemanticRole::Plain
        };
        frame.render_widget(
            Paragraph::new(fit_end(text, usize::from(area.width))).style(theme.style(role)),
            Rect::new(
                area.x,
                area.y.saturating_add(screen_row as u16),
                area.width,
                1,
            ),
        );
    }
}

fn render_file_list(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    list: &FileListState,
    theme: &Theme,
) {
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(theme.style(SemanticRole::Plain)),
        area,
    );

    let selected_original = list
        .filtered_file_indices
        .get(list.selected_file_index)
        .copied();
    let detail = selected_original
        .and_then(|index| state.document().files().get(index))
        .map_or_else(|| "changed files".to_owned(), FileEntry::display_path);
    let detail_lines = wrap_detail(&detail, usize::from(area.width), usize::from(area.height));
    for (row, text) in detail_lines.iter().enumerate() {
        frame.render_widget(
            Paragraph::new(text.clone()).style(theme.style(SemanticRole::Sticky)),
            Rect::new(area.x, area.y.saturating_add(row as u16), area.width, 1),
        );
    }

    let list_y = area.y.saturating_add(detail_lines.len() as u16);
    let list_height = area.height.saturating_sub(detail_lines.len() as u16);
    let overlay_lines = file_overlay_lines(state, list, usize::from(area.width));
    let selected_line = overlay_lines
        .iter()
        .position(|line| line.selected)
        .unwrap_or(0);
    let available = usize::from(list_height);
    let start = selected_line
        .saturating_sub(available / 2)
        .min(overlay_lines.len().saturating_sub(available));
    for (screen_row, overlay) in overlay_lines.iter().skip(start).take(available).enumerate() {
        let role = if overlay.selected {
            SemanticRole::Selected
        } else {
            overlay.role
        };
        frame.render_widget(
            Paragraph::new(overlay.text.clone()).style(theme.style(role)),
            Rect::new(
                area.x,
                list_y.saturating_add(screen_row as u16),
                area.width,
                1,
            ),
        );
    }
}

fn wrap_detail(text: &str, width: usize, height: usize) -> Vec<String> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let available = height.saturating_sub(1).max(1);
    let segments = wrap_hard(text, width);
    segments
        .iter()
        .take(available)
        .map(|segment| text[segment.byte_range.clone()].to_owned())
        .collect()
}

#[derive(Clone, Debug)]
struct OverlayLine {
    text: String,
    role: SemanticRole,
    selected: bool,
}

fn file_overlay_lines(state: &AppState, list: &FileListState, width: usize) -> Vec<OverlayLine> {
    let visible = list
        .filtered_file_indices
        .iter()
        .copied()
        .collect::<HashSet<_>>();
    let selected_original = list
        .filtered_file_indices
        .get(list.selected_file_index)
        .copied();
    let filtering = list
        .filter
        .as_ref()
        .is_some_and(|filter| !filter.text().is_empty());
    let mut output = Vec::new();

    for commit in state.document().commits() {
        let entries = commit
            .file_ids
            .iter()
            .filter_map(|id| {
                state
                    .document()
                    .files()
                    .iter()
                    .position(|file| file.id == *id)
            })
            .filter(|index| visible.contains(index))
            .collect::<Vec<_>>();
        if entries.is_empty() && (filtering || !commit.file_ids.is_empty()) {
            continue;
        }
        output.push(OverlayLine {
            text: fit_end(&commit_label(commit), width),
            role: SemanticRole::CommitHeader,
            selected: false,
        });
        if commit.file_ids.is_empty() {
            output.push(OverlayLine {
                text: fit_end("  (no file changes)", width),
                role: SemanticRole::Dim,
                selected: false,
            });
        }
        for index in entries {
            if let Some(file) = state.document().files().get(index) {
                output.push(OverlayLine {
                    text: file_list_entry(file, width),
                    role: SemanticRole::Plain,
                    selected: selected_original == Some(index),
                });
            }
        }
    }

    if output.is_empty() {
        output.push(OverlayLine {
            text: fit_end("(no matching files)", width),
            role: SemanticRole::Dim,
            selected: false,
        });
    }
    output
}

fn file_list_entry(file: &FileEntry, width: usize) -> String {
    let counts = if matches!(file.status, crate::model::ChangeStatus::Binary) {
        "binary".to_owned()
    } else {
        let additions = file
            .additions
            .map_or_else(|| "?".to_owned(), |value| value.to_string());
        let deletions = file
            .deletions
            .map_or_else(|| "?".to_owned(), |value| value.to_string());
        format!("+{additions} -{deletions}")
    };
    let prefix = format!("  {} {counts} ", file.status.label());
    let path_width = width.saturating_sub(display_width(&prefix));
    let path = fit_file_path(file, path_width);
    fit_end(&format!("{prefix}{path}"), width)
}

fn render_status(frame: &mut Frame<'_>, area: Rect, state: &AppState, theme: &Theme) {
    let width = usize::from(area.width);
    match state.mode() {
        Mode::Search(search) => render_search_prompt(frame, area, state, search, theme),
        Mode::FileList(list) if list.filter.is_some() => {
            if let Some(filter) = list.filter.as_ref() {
                let suffix = status_suffix(&combined_status(state));
                render_input_prompt(frame, area, "/", filter, &suffix, theme);
            }
        }
        Mode::FileList(_) => frame.render_widget(
            Paragraph::new(fit_end(
                &append_status(
                    "Tab/Esc close  / filter  Enter jump  j/k select",
                    &combined_status(state),
                ),
                width,
            ))
            .style(theme.style(SemanticRole::Status)),
            area,
        ),
        Mode::Help { .. } => frame.render_widget(
            Paragraph::new(fit_end(
                &append_status("Esc/q/h close  j/k scroll", &combined_status(state)),
                width,
            ))
            .style(theme.style(SemanticRole::Status)),
            area,
        ),
        Mode::Body => frame.render_widget(
            Paragraph::new(fit_end(&body_status(state), width))
                .style(theme.style(SemanticRole::Status)),
            area,
        ),
    }
}

fn render_search_prompt(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    search: &SearchInputState,
    theme: &Theme,
) {
    let prefix = match search.direction {
        crate::search::SearchDirection::Forward => "/",
        crate::search::SearchDirection::Backward => "?",
    };
    let regex_error = state
        .search()
        .incremental()
        .and_then(|query| match query.status() {
            IncrementalQueryStatus::Invalid(error) => Some(format!("  regex: {error}")),
            IncrementalQueryStatus::Empty | IncrementalQueryStatus::Valid => None,
        });
    let status = combined_status(state);
    let suffix = regex_error.map_or_else(
        || status_suffix(&status),
        |error| status_suffix(&append_status(&error, &status)),
    );
    render_input_prompt(frame, area, prefix, &search.input, &suffix, theme);
}

fn status_suffix(status: &str) -> String {
    if status.is_empty() {
        String::new()
    } else {
        format!("  {status}")
    }
}

fn append_status(base: &str, status: &str) -> String {
    if status.is_empty() {
        base.to_owned()
    } else {
        format!("{base}  {status}")
    }
}

fn combined_status(state: &AppState) -> String {
    match (state.status().is_empty(), state.background_status()) {
        (true, Some(background)) => background.to_owned(),
        (false, Some(background)) => format!("{}  {background}", state.status()),
        (_, None) => state.status().to_owned(),
    }
}

fn render_input_prompt(
    frame: &mut Frame<'_>,
    area: Rect,
    prefix: &str,
    input: &TextInput,
    suffix: &str,
    theme: &Theme,
) {
    let width = usize::from(area.width);
    let prefix_width = display_width(prefix);
    let suffix = fit_end(suffix, width.saturating_sub(prefix_width + 1));
    let suffix_width = display_width(&suffix);
    let input_width = width.saturating_sub(prefix_width + suffix_width);
    let window = input_window(input.text(), input.cursor(), input_width);
    let text = format!("{prefix}{}{suffix}", window.text);
    frame.render_widget(
        Paragraph::new(fit_end(&text, width)).style(theme.style(SemanticRole::Status)),
        area,
    );
    if area.width > 0 {
        let cursor_x = area.x.saturating_add(
            (prefix_width + window.cursor_cells).min(width.saturating_sub(1)) as u16,
        );
        frame.set_cursor_position((cursor_x, area.y));
    }
}

struct InputWindow {
    text: String,
    cursor_cells: usize,
}

fn input_window(text: &str, cursor: usize, width: usize) -> InputWindow {
    if width == 0 {
        return InputWindow {
            text: String::new(),
            cursor_cells: 0,
        };
    }
    let cursor = cursor.min(text.len());
    let mut start = cursor;
    let target_left = width.saturating_sub(4).max(1);
    for (index, grapheme) in text[..cursor].grapheme_indices(true).rev() {
        let candidate = display_width(&text[index..cursor]);
        if candidate > target_left {
            break;
        }
        start = index;
        if grapheme.is_empty() {
            break;
        }
    }
    let leading = usize::from(start > 0);
    let available = width.saturating_sub(leading);
    let mut end = start;
    let mut used = 0_usize;
    for (relative, grapheme) in text[start..].grapheme_indices(true) {
        let grapheme_width = display_width(grapheme);
        if used.saturating_add(grapheme_width) > available {
            break;
        }
        end = start + relative + grapheme.len();
        used = used.saturating_add(grapheme_width);
    }
    let trailing = end < text.len();
    if trailing
        && used == available
        && end > start
        && let Some((relative, _)) = text[start..end].grapheme_indices(true).next_back()
    {
        end = start + relative;
    }
    let mut visible = String::new();
    if leading > 0 {
        visible.push('…');
    }
    visible.push_str(&text[start..end]);
    if trailing {
        visible.push('…');
    }
    let cursor_end = cursor.min(end).max(start);
    InputWindow {
        text: fit_end(&visible, width),
        cursor_cells: leading + display_width(&text[start..cursor_end]),
    }
}

fn body_status(state: &AppState) -> String {
    let position = if state.layout().is_empty() {
        "row 0/0".to_owned()
    } else {
        format!(
            "row {}/{}",
            state.viewport_top().saturating_add(1),
            state.layout().len()
        )
    };
    let input = if state.document().is_complete() {
        "EOF"
    } else {
        "loading"
    };
    let whitespace = match state.whitespace() {
        WhitespaceChoice::Ignore => "ws:ignore",
        WhitespaceChoice::Show => "ws:show",
    };
    let follow = state.is_following().then_some("follow");
    let pending = state.search().pending_forward().map(|_| "searching");
    let fallback = state
        .document()
        .files()
        .iter()
        .any(|file| !file.mode.is_rich())
        .then_some("fallback");
    let skipped = (state.search().last_skipped_oversized_lines() > 0)
        .then(|| format!("skipped:{}", state.search().last_skipped_oversized_lines()));
    let current = displayed_current_match(state).map(|found| search_position(state, found));
    let mut parts = vec![position, input.to_owned(), whitespace.to_owned()];
    parts.extend(follow.map(str::to_owned));
    parts.extend(pending.map(str::to_owned));
    parts.extend(fallback.map(str::to_owned));
    parts.extend(skipped);
    parts.extend(current);
    parts.extend(state.background_status().map(str::to_owned));
    if !state.status().is_empty() {
        parts.push(state.status().to_owned());
    }
    parts.join("  ")
}

fn search_position(state: &AppState, found: &SearchMatch<crate::model::LineId>) -> String {
    let column = state
        .document()
        .line(found.line_id)
        .and_then(|line| line.text.get(..found.byte_range.start))
        .map_or(found.byte_range.start + 1, |prefix| {
            prefix.graphemes(true).count() + 1
        });
    format!("match {}:{}", found.line_index + 1, column)
}

fn fit_file_path(file: &FileEntry, width: usize) -> String {
    let full = file.display_path();
    if display_width(&full) <= width {
        return full;
    }
    if let (Some(old), Some(new)) = (&file.old_path, &file.new_path)
        && old != new
    {
        let compact = format!("{} → {}", basename(old), basename(new));
        return middle_elide(&compact, width);
    }
    left_elide(&full, width)
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn fit_end(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }
    let mut output = take_cells(text, width - 1, false);
    output.push('…');
    output
}

fn left_elide(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }
    format!("…{}", take_cells(text, width - 1, true))
}

fn middle_elide(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_owned();
    }
    if width <= 2 {
        return fit_end(text, width);
    }
    let left = (width - 1) / 2;
    let right = width - 1 - left;
    format!(
        "{}…{}",
        take_cells(text, left, false),
        take_cells(text, right, true)
    )
}

fn take_cells(text: &str, width: usize, from_end: bool) -> String {
    let mut graphemes = if from_end {
        text.graphemes(true).rev().collect::<Vec<_>>()
    } else {
        text.graphemes(true).collect::<Vec<_>>()
    };
    let mut used = 0_usize;
    let mut taken = Vec::new();
    for grapheme in graphemes.drain(..) {
        let grapheme_width = display_width(grapheme);
        if used.saturating_add(grapheme_width) > width {
            break;
        }
        used = used.saturating_add(grapheme_width);
        taken.push(grapheme);
    }
    if from_end {
        taken.reverse();
    }
    taken.concat()
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::cli::ThemeChoice;
    use crate::decorations::DecorationSet;
    use crate::parser::parse_bytes;
    use crate::search::compile_smart_case;
    use crate::state::{Effect, Event};
    use crate::theme::ColorLevel;

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let area = terminal.backend().buffer().area;
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn apply_search_preview(state: &mut AppState, pattern: &str) {
        state.update(Event::Key(KeyEvent::new(
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        )));
        let effects = state.update(Event::Paste(pattern.to_owned()));
        let task = effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Search(task) => Some(task),
                _ => None,
            })
            .expect("search preview task");
        let result = state.execute_search_task(&task);
        state.update(Event::SearchCompleted {
            generation: task.generation,
            result,
            preview: true,
        });
    }

    #[test]
    fn body_renders_sticky_file_and_dual_gutter() {
        let document = parse_bytes(
            b"diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -10 +10 @@\n-old\n+new\n",
        );
        let state = AppState::new(Arc::new(document), 60, 8, 4, WhitespaceChoice::Ignore, true);
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        let decorations =
            DecorationSet::compute(state.document(), &theme, WhitespaceChoice::Ignore);
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal
            .draw(|frame| render(frame, &state, &theme, &decorations))
            .unwrap();
        let screen = buffer_text(&terminal);
        assert!(screen.contains("src/main.rs"), "{screen}");
        assert!(screen.contains("10     │-old"), "{screen}");
        assert!(screen.contains("10 │+new"), "{screen}");
        assert!(screen.contains("row 1/"), "{screen}");
    }

    #[test]
    fn show_whitespace_renders_changed_tab_cells() {
        let document = parse_bytes(
            b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-\tfoo\n+    foo\n",
        );
        let state = AppState::new(Arc::new(document), 60, 8, 4, WhitespaceChoice::Show, true);
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        let decorations = DecorationSet::compute(state.document(), &theme, WhitespaceChoice::Show);
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal
            .draw(|frame| render(frame, &state, &theme, &decorations))
            .unwrap();
        let screen = buffer_text(&terminal);
        assert!(screen.contains("-→···foo"), "{screen}");
        assert!(screen.contains("+····foo"), "{screen}");
    }

    #[test]
    fn weak_matches_keep_only_visible_non_empty_ranges_with_a_frame_cap() {
        let dense = compile_smart_case(".").unwrap();
        let text = "x".repeat(1_000);
        let mut remaining = 3;
        let matches = weak_matches_in_visible_range(&dense, &text, 500..520, &mut remaining);
        assert_eq!(matches, vec![500..501, 501..502, 502..503]);
        assert_eq!(remaining, 0);

        let crossing = compile_smart_case("x+").unwrap();
        let mut remaining = 3;
        assert_eq!(
            weak_matches_in_visible_range(&crossing, &text, 500..520, &mut remaining),
            vec![0..520]
        );

        let zero_width = compile_smart_case("").unwrap();
        let mut remaining = 3;
        assert!(
            weak_matches_in_visible_range(&zero_width, &text, 500..520, &mut remaining).is_empty()
        );
        assert_eq!(remaining, 3);

        let large = "x".repeat(100_000);
        let mut remaining = 2;
        assert_eq!(
            weak_matches_in_visible_range(&dense, &large, 90_000..90_020, &mut remaining),
            vec![90_000..90_001, 90_001..90_002]
        );
    }

    #[test]
    fn current_search_style_wins_over_syntax_and_intraline_layers() {
        let document = parse_bytes(
            b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-let old = 1;\n+let new = 2;\n",
        );
        let mut state = AppState::new(Arc::new(document), 60, 8, 4, WhitespaceChoice::Ignore, true);
        apply_search_preview(&mut state, "new");
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::TrueColor);
        let decorations =
            DecorationSet::compute(state.document(), &theme, WhitespaceChoice::Ignore);
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal
            .draw(|frame| render(frame, &state, &theme, &decorations))
            .unwrap();
        let expected = theme.style(SemanticRole::SearchCurrent);
        assert!(terminal.backend().buffer().content.iter().any(|cell| {
            cell.symbol() == "n"
                && cell.fg == expected.fg.unwrap()
                && cell.bg == expected.bg.unwrap()
        }));
    }

    #[test]
    fn zero_width_match_on_empty_plain_line_gets_a_visible_marker() {
        let document = parse_bytes(b"\n");
        let mut state = AppState::new(Arc::new(document), 20, 6, 4, WhitespaceChoice::Ignore, true);
        apply_search_preview(&mut state, "^$");
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        let mut terminal = Terminal::new(TestBackend::new(20, 6)).unwrap();
        terminal
            .draw(|frame| render(frame, &state, &theme, &DecorationSet::default()))
            .unwrap();
        let screen = buffer_text(&terminal);
        assert!(screen.contains('▏'), "{screen}");
    }

    #[test]
    fn path_elision_keeps_rename_basenames() {
        let mut parser = crate::parser::GitParser::new();
        parser.push(
            b"diff --git a/a/very/long/old-name.rs b/b/very/long/new-name.rs\nsimilarity index 100%\nrename from a/very/long/old-name.rs\nrename to b/very/long/new-name.rs\n",
        );
        parser.finish();
        let file = &parser.document().files()[0];
        let fitted = fit_file_path(file, 28);
        assert!(fitted.contains("old-name"));
        assert!(fitted.contains("new-name"));
        assert!(display_width(&fitted) <= 28);
    }

    #[test]
    fn input_window_keeps_cursor_visible() {
        let window = input_window("0123456789abcdef", 12, 8);
        assert!(window.text.starts_with('…'));
        assert!(window.cursor_cells < 8);
        assert!(display_width(&window.text) <= 8);
    }

    #[test]
    fn help_contract_matches_state_scroll_count() {
        assert_eq!(HELP_LINES.len(), 42);
    }
}
