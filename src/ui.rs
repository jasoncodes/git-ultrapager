//! Interactive pager の画面構成と、semantic role から Ratatui widget への変換。
//!
//! この module は AppState の immutable snapshot だけを読み、sticky header、本文、status bar、
//! help/file-list overlay を1 frameへ描く。入力 event、terminal lifecycle、background worker は扱わない。
//! 検索 span は layout が保持する byte mapping を使い、tab 展開や折り返しをまたぐ一致も sanitized
//! logical line の offset から対応する visual row へ写す。

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::cli::WhitespaceChoice;
use crate::direct::VisualRow;
use crate::model::{Commit, CommitKind, DocumentLine, FileEntry, LineKind};
use crate::search::{
    CompiledSearch, IncrementalQueryStatus, SearchLine, SearchMatch, match_spans_in_line,
};
use crate::state::{AppState, FileListState, Mode, SearchInputState, TextInput};
use crate::text::wrap_hard;
use crate::theme::{SemanticRole, Theme};

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
/// 書き込まず、search/filter input 中だけ Ratatui の software cursor 位置を設定する。
pub fn render(frame: &mut Frame<'_>, state: &AppState, theme: &Theme) {
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
        Mode::Body | Mode::Search(_) => render_body(frame, content_area, state, theme),
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

fn render_body(frame: &mut Frame<'_>, area: Rect, state: &AppState, theme: &Theme) {
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
    let compiled = displayed_search(state);
    let mut matches_by_line = HashMap::new();
    for (screen_row, row) in rows
        .iter()
        .skip(start)
        .take(usize::from(body_area.height))
        .enumerate()
    {
        let y = body_area.y.saturating_add(screen_row as u16);
        let weak_matches = compiled.map_or(&[][..], |compiled| {
            matches_by_line
                .entry(row.line_id)
                .or_insert_with(|| {
                    state
                        .document()
                        .line(row.line_id)
                        .map(|line| {
                            match_spans_in_line(
                                compiled,
                                usize::try_from(line.id.get()).unwrap_or(usize::MAX),
                                SearchLine::new(line.id, &line.text),
                            )
                            .byte_ranges
                        })
                        .unwrap_or_default()
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
            )),
            Rect::new(body_area.x, y, body_area.width, 1),
        );
    }
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
) -> Line<'line> {
    let Some(logical_line) = state.document().line(row.line_id) else {
        return Line::from(Span::styled(row.text.clone(), theme.style(row.role)));
    };

    let gutter_end = source_content_start(row, logical_line)
        .map(|start| start.saturating_sub(1))
        .unwrap_or(0);
    let mut overlays = search_overlays(state, row, row_index, logical_line, weak_matches);
    overlays.sort_by_key(|overlay| (overlay.range.start, overlay.range.end));

    let mut boundaries = vec![0, row.text.len(), gutter_end];
    for overlay in &overlays {
        boundaries.push(overlay.range.start);
        boundaries.push(overlay.range.end);
    }
    boundaries.retain(|offset| *offset <= row.text.len() && row.text.is_char_boundary(*offset));
    boundaries.sort_unstable();
    boundaries.dedup();

    if boundaries.len() == 1 {
        return Line::from(Span::styled(row.text.clone(), theme.style(row.role)));
    }

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
            let overlay = overlays
                .iter()
                .filter(|candidate| {
                    candidate.range.start <= range.start && range.end <= candidate.range.end
                })
                .max_by_key(|candidate| candidate.priority);
            let style = overlay.map_or_else(
                || theme.style(base_role),
                |candidate| theme.style(base_role).patch(theme.style(candidate.role)),
            );
            Some(Span::styled(row.text[range].to_owned(), style))
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

#[derive(Clone, Debug)]
struct StyledRange {
    range: Range<usize>,
    role: SemanticRole,
    priority: u8,
}

fn search_overlays(
    state: &AppState,
    row: &VisualRow,
    row_index: usize,
    line: &DocumentLine,
    weak_matches: &[Range<usize>],
) -> Vec<StyledRange> {
    let current = displayed_current_match(state);
    let mut overlays = Vec::new();

    for found in weak_matches {
        if found.is_empty() {
            continue;
        }
        for mapped in map_logical_range(row, line, found.clone()) {
            overlays.push(StyledRange {
                range: mapped,
                role: SemanticRole::SearchMatch,
                priority: 1,
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
                overlays.push(StyledRange {
                    range: marker,
                    role: SemanticRole::SearchCurrent,
                    priority: 3,
                });
            }
        } else {
            for mapped in map_logical_range(row, line, current.byte_range.clone()) {
                overlays.push(StyledRange {
                    range: mapped,
                    role: SemanticRole::SearchCurrent,
                    priority: 3,
                });
            }
        }
    }
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
                let suffix = status_suffix(state.status());
                render_input_prompt(frame, area, "/", filter, &suffix, theme);
            }
        }
        Mode::FileList(_) => frame.render_widget(
            Paragraph::new(fit_end(
                &append_status(
                    "Tab/Esc close  / filter  Enter jump  j/k select",
                    state.status(),
                ),
                width,
            ))
            .style(theme.style(SemanticRole::Status)),
            area,
        ),
        Mode::Help { .. } => frame.render_widget(
            Paragraph::new(fit_end(
                &append_status("Esc/q/h close  j/k scroll", state.status()),
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
    let suffix = regex_error.unwrap_or_else(|| status_suffix(state.status()));
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

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::cli::ThemeChoice;
    use crate::parser::parse_bytes;
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

    #[test]
    fn body_renders_sticky_file_and_dual_gutter() {
        let document = parse_bytes(
            b"diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -10 +10 @@\n-old\n+new\n",
        );
        let state = AppState::new(Arc::new(document), 60, 8, 4, WhitespaceChoice::Ignore, true);
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal
            .draw(|frame| render(frame, &state, &theme))
            .unwrap();
        let screen = buffer_text(&terminal);
        assert!(screen.contains("src/main.rs"), "{screen}");
        assert!(screen.contains("10     │-old"), "{screen}");
        assert!(screen.contains("10 │+new"), "{screen}");
        assert!(screen.contains("row 1/"), "{screen}");
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
