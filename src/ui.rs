//! Interactive pager の画面構成と、semantic role から Ratatui widget への変換。
//!
//! このmoduleはAppState、DecorationSet、draw前に構築した[`FrameLines`]だけを読み、sticky header、本文、
//! status bar、help/file-list overlayを1 frameへ描く。Raw store I/O、入力event、terminal lifecycle、
//! background workerは扱わない。Frame preloadはfixed metadataとbounded search rangeだけを保持し、検索用に
//! 一時decodeしたline textはdraw前に破棄する。Syntax foreground、intraline background、searchの順に
//! styleを重ね、全spanをlayoutが持つbyte mappingでtab展開・折り返し後のvisual rowへ写す。

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::bounded_text::{
    BoundedDocumentTextReader, BoundedTextError, BoundedTextRead, MAX_EAGER_TEXT_RAW_BYTES,
};
use crate::cli::WhitespaceChoice;
use crate::decorations::{DecorationSet, LineDecoration};
use crate::direct::VisualRow;
use crate::document::{DocumentError, DocumentLineMetadata, DocumentLineMetadataReader};
use crate::model::{Commit, CommitKind, FileEntry, LineId, LineKind};
use crate::search::{CompiledSearch, IncrementalQueryStatus, SearchMatch};
use crate::state::{AppState, FileListState, Mode, SearchInputState, TextInput};
use crate::text::MAX_SEARCH_LINE_BYTES;
use crate::theme::{SemanticRole, Theme};
use crate::viewport::ViewportRow;

/// Weak matchは補助表示であり、current matchのnavigation contractには使わない。1 frameで保持する
/// range数を固定し、1 cellごとにmatchするpatternでもrendererのallocationを入力長へ比例させない。
const MAX_WEAK_MATCH_SPANS_PER_FRAME: usize = 16_384;
const MAX_WEAK_MATCH_LOOKBEHIND_BYTES: usize = 4 * 1024;
const MAX_FRAME_LINE_ENTRIES: usize = 5_000;
const MAX_FRAME_TRANSIENT_TEXT_BYTES: usize = 1024 * 1024;
const MAX_FRAME_RETAINED_BYTES: usize = 4 * 1024 * 1024;
const MAX_PATH_SCAN_BYTES: usize = 64 * 1024;
const MAX_DETAIL_ROW_BYTES: usize = 64 * 1024;

/// 1 frameが参照するfixed metadataと事前計算済みsearch rangeを保持するbounded data。
///
/// [`Self::load`]は`Terminal::draw`より前に呼ぶ。Rendererはこのmapだけを読み、raw storeやspill fileへ
/// I/Oを発生させない。Weak matchとsearch columnにfull textが要るlineもreaderの借用buffer内で計算し、
/// `FrameLines`へはrangeと数値だけを移す。
#[derive(Debug, Default)]
pub struct FrameLines {
    lines: HashMap<LineId, FrameLine>,
    current_column: Option<(LineId, usize)>,
    transient_raw_bytes_read: u64,
}

#[derive(Debug)]
struct FrameLine {
    metadata: DocumentLineMetadata,
    weak_matches: Vec<Range<usize>>,
}

/// Visible frameの事前decodeに失敗した理由。
#[derive(Debug, Error)]
pub enum FrameLinesError {
    #[error(transparent)]
    Document(#[from] DocumentError),
    #[error(transparent)]
    BoundedText(#[from] BoundedTextError),
    #[error("document snapshot does not contain frame logical line {line_id}")]
    MissingLine { line_id: LineId },
    #[error("could not allocate frame line index for {requested} logical lines")]
    Allocation { requested: usize },
    #[error("frame references {requested} logical lines, exceeding the {limit}-line bound")]
    LineLimit { requested: usize, limit: usize },
    #[error("frame metadata/ranges retain {requested} bytes, exceeding the {limit}-byte bound")]
    FrameBudget { requested: usize, limit: usize },
}

impl FrameLines {
    /// 現在viewportとsearch statusが参照するmetadata/search rangeをdraw前に構築する。
    /// Raw長がeager decoder上限を越えるlineは、display textが短くてもmain threadではdecodeせず、
    /// current matchのcolumnをbyte offsetへfallbackさせてweak matchだけを省略する。
    pub fn load(state: &AppState) -> Result<Self, FrameLinesError> {
        let visible = state.visible_rows();
        let current = displayed_current_match(state).map(|found| found.line_id);
        let requested = visible.len().saturating_add(usize::from(current.is_some()));
        let visible_ranges = visible_logical_ranges(visible)?;
        let mut requested_ids = Vec::new();
        requested_ids
            .try_reserve(requested.min(MAX_FRAME_LINE_ENTRIES))
            .map_err(|_| FrameLinesError::Allocation { requested })?;
        let mut requested_seen = HashSet::new();
        requested_seen
            .try_reserve(requested.min(MAX_FRAME_LINE_ENTRIES))
            .map_err(|_| FrameLinesError::Allocation { requested })?;
        for line_id in visible.iter().map(|row| row.line_id).chain(current) {
            if requested_seen.contains(&line_id) {
                continue;
            }
            if requested_ids.len() == MAX_FRAME_LINE_ENTRIES {
                return Err(FrameLinesError::LineLimit {
                    requested: requested_ids.len().saturating_add(1),
                    limit: MAX_FRAME_LINE_ENTRIES,
                });
            }
            requested_seen.insert(line_id);
            requested_ids.push(line_id);
        }
        let mut lines = HashMap::new();
        lines
            .try_reserve(requested_ids.len())
            .map_err(|_| FrameLinesError::Allocation { requested })?;
        let reserved = retained_frame_index_bytes(lines.capacity());
        if reserved > MAX_FRAME_RETAINED_BYTES {
            return Err(FrameLinesError::FrameBudget {
                requested: reserved,
                limit: MAX_FRAME_RETAINED_BYTES,
            });
        }
        let mut reader = BoundedDocumentTextReader::new();
        let mut metadata_reader = DocumentLineMetadataReader::new();
        let compiled = displayed_search(state);
        let current_match = displayed_current_match(state);
        let mut remaining_weak_matches = MAX_WEAK_MATCH_SPANS_PER_FRAME;
        let mut current_column =
            current_match.map(|found| (found.line_id, found.byte_range.start.saturating_add(1)));
        for line_id in requested_ids {
            let metadata = metadata_reader
                .read(state.document(), line_id)?
                .ok_or(FrameLinesError::MissingLine { line_id })?;
            let requires_text = (compiled.is_some() && visible_ranges.contains_key(&line_id))
                || current_match.is_some_and(|found| found.line_id == line_id);
            let raw_is_eager = u64::try_from(MAX_EAGER_TEXT_RAW_BYTES)
                .is_ok_and(|limit| metadata.raw_len <= limit);
            let mut weak_matches = Vec::new();
            if requires_text
                && raw_is_eager
                && let BoundedTextRead::Available { text, .. } = reader.read(
                    state.document(),
                    line_id,
                    MAX_FRAME_TRANSIENT_TEXT_BYTES,
                    &|| false,
                )?
            {
                if let Some(compiled) = compiled
                    && let Some(visible) = visible_ranges.get(&line_id)
                {
                    weak_matches = weak_matches_in_visible_range(
                        compiled,
                        text,
                        visible.clone(),
                        &mut remaining_weak_matches,
                    )?;
                }
                if let Some(found) = current_match.filter(|found| found.line_id == line_id) {
                    let column = text
                        .get(..found.byte_range.start)
                        .map_or(found.byte_range.start.saturating_add(1), |prefix| {
                            prefix.graphemes(true).count().saturating_add(1)
                        });
                    current_column = Some((line_id, column));
                }
            }
            let next_retained = retained_frame_index_bytes(lines.capacity())
                .saturating_add(retained_weak_match_bytes(&lines))
                .saturating_add(
                    weak_matches
                        .capacity()
                        .saturating_mul(std::mem::size_of::<Range<usize>>()),
                );
            if next_retained > MAX_FRAME_RETAINED_BYTES {
                return Err(FrameLinesError::FrameBudget {
                    requested: next_retained,
                    limit: MAX_FRAME_RETAINED_BYTES,
                });
            }
            lines.insert(
                line_id,
                FrameLine {
                    metadata,
                    weak_matches,
                },
            );
        }
        let frame = Self {
            lines,
            current_column,
            transient_raw_bytes_read: reader.large_line_stats().raw_bytes_read,
        };
        let retained = frame.retained_bytes();
        if retained > MAX_FRAME_RETAINED_BYTES {
            return Err(FrameLinesError::FrameBudget {
                requested: retained,
                limit: MAX_FRAME_RETAINED_BYTES,
            });
        }
        Ok(frame)
    }

    /// Stable IDに対応するfixed line metadataを返す。
    #[must_use]
    pub fn metadata(&self, line_id: LineId) -> Option<DocumentLineMetadata> {
        self.lines.get(&line_id).map(|line| line.metadata)
    }

    /// Stable IDに対応する事前計算済みweak search rangeを返す。
    #[must_use]
    pub fn weak_matches(&self, line_id: LineId) -> &[Range<usize>] {
        self.lines
            .get(&line_id)
            .map_or(&[], |line| line.weak_matches.as_slice())
    }

    /// Current search matchの1始まり表示columnを返す。
    #[must_use]
    pub fn current_column(&self, line_id: LineId) -> Option<usize> {
        self.current_column
            .filter(|(current, _)| *current == line_id)
            .map(|(_, column)| column)
    }

    /// Rendererへ保持するmetadata、range vector、hash tableの保守的byte数。
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        retained_frame_index_bytes(self.lines.capacity())
            .saturating_add(retained_weak_match_bytes(&self.lines))
    }

    /// Preload中にbounded streaming readerがraw storeから読んだbyte数。
    #[must_use]
    pub const fn transient_raw_bytes_read(&self) -> u64 {
        self.transient_raw_bytes_read
    }
}

fn retained_frame_index_bytes(capacity: usize) -> usize {
    std::mem::size_of::<FrameLines>().saturating_add(capacity.saturating_mul(
        std::mem::size_of::<(LineId, FrameLine)>() + 2 * std::mem::size_of::<usize>(),
    ))
}

fn retained_weak_match_bytes(lines: &HashMap<LineId, FrameLine>) -> usize {
    lines.values().fold(0_usize, |total, line| {
        total.saturating_add(
            line.weak_matches
                .capacity()
                .saturating_mul(std::mem::size_of::<Range<usize>>()),
        )
    })
}

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
pub fn render(
    frame: &mut Frame<'_>,
    state: &AppState,
    theme: &Theme,
    decorations: &DecorationSet,
    lines: &FrameLines,
) {
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
            render_body(frame, content_area, state, theme, decorations, lines);
        }
        Mode::Help { top } => render_help(frame, content_area, *top, theme),
        Mode::FileList(list) => render_file_list(frame, content_area, state, list, theme),
    }
    render_status(frame, status_area, state, theme, lines);
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
    lines: &FrameLines,
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

    let visible_rows = &state.visible_rows()[..state
        .visible_rows()
        .len()
        .min(usize::from(body_area.height))];
    for (screen_row, row) in visible_rows.iter().enumerate() {
        let y = body_area.y.saturating_add(screen_row as u16);
        let weak_matches = lines.weak_matches(row.line_id);
        frame.render_widget(
            Paragraph::new(render_visual_row(
                state,
                row,
                screen_row,
                weak_matches,
                theme,
                decorations,
                lines,
            )),
            Rect::new(body_area.x, y, body_area.width, 1),
        );
    }
}

fn visible_logical_ranges(
    rows: &[ViewportRow],
) -> Result<HashMap<crate::model::LineId, Range<usize>>, FrameLinesError> {
    let requested = rows.len().min(MAX_FRAME_LINE_ENTRIES);
    let requested_bytes = requested.saturating_mul(
        std::mem::size_of::<(LineId, Range<usize>)>() + 2 * std::mem::size_of::<usize>(),
    );
    if requested_bytes > MAX_FRAME_RETAINED_BYTES {
        return Err(FrameLinesError::FrameBudget {
            requested: requested_bytes,
            limit: MAX_FRAME_RETAINED_BYTES,
        });
    }
    let mut ranges = HashMap::<_, Range<usize>>::new();
    ranges
        .try_reserve(requested)
        .map_err(|_| FrameLinesError::Allocation { requested })?;
    let reserved_bytes = retained_visible_range_bytes(ranges.capacity());
    if reserved_bytes > MAX_FRAME_RETAINED_BYTES {
        return Err(FrameLinesError::FrameBudget {
            requested: reserved_bytes,
            limit: MAX_FRAME_RETAINED_BYTES,
        });
    }
    for row in rows {
        if let Some(range) = ranges.get_mut(&row.line_id) {
            range.start = range.start.min(row.content_range.start);
            range.end = range.end.max(row.content_range.end);
            continue;
        }
        if ranges.len() == MAX_FRAME_LINE_ENTRIES {
            return Err(FrameLinesError::LineLimit {
                requested: ranges.len().saturating_add(1),
                limit: MAX_FRAME_LINE_ENTRIES,
            });
        }
        ranges.insert(row.line_id, row.content_range.clone());
    }
    Ok(ranges)
}

fn retained_visible_range_bytes(capacity: usize) -> usize {
    capacity.saturating_mul(
        std::mem::size_of::<(LineId, Range<usize>)>() + 2 * std::mem::size_of::<usize>(),
    )
}

fn weak_matches_in_visible_range(
    compiled: &CompiledSearch,
    text: &str,
    visible: Range<usize>,
    remaining: &mut usize,
) -> Result<Vec<Range<usize>>, FrameLinesError> {
    if text.len() > MAX_SEARCH_LINE_BYTES || visible.is_empty() || *remaining == 0 {
        return Ok(Vec::new());
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
            matches
                .try_reserve(1)
                .map_err(|_| FrameLinesError::Allocation {
                    requested: matches.len().saturating_add(1),
                })?;
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
    Ok(matches)
}

fn sticky_header(state: &AppState, width: usize) -> String {
    let Some(row) = state.current_row() else {
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
    lines: &FrameLines,
) -> Line<'line> {
    let Some(metadata) = lines.metadata(row.line_id) else {
        return Line::from(Span::styled(row.text.clone(), theme.style(row.role)));
    };
    if row.text.is_empty()
        && displayed_current_match(state).is_some_and(|current| {
            current.line_id == row.line_id
                && current.is_zero_width()
                && row_contains_logical_offset(state, row_index, current.byte_range.start, metadata)
        })
    {
        // Empty logical lineには既存cellがないためstyleだけでは位置を示せない。検索中だけ1 cellの
        // markerを描き、NO_COLORでもbold glyphとしてcurrent matchを見失わないようにする。
        return Line::from(Span::styled("▏", theme.style(SemanticRole::SearchCurrent)));
    }

    let gutter_end = source_content_start(row, metadata.kind)
        .map(|start| start.saturating_sub(1))
        .unwrap_or(0);
    let line_decoration = decorations.line(row.line_id);
    let mut syntax = syntax_overlays(row, line_decoration);
    syntax.sort_by_key(|overlay| (overlay.range.start, overlay.range.end));
    let mut intraline = if decorations.whitespace() == state.whitespace() {
        intraline_overlays(row, metadata.kind, line_decoration)
    } else {
        Vec::new()
    };
    intraline.sort_by_key(|overlay| (overlay.range.start, overlay.range.end));
    let search = search_overlays(state, row, row_index, metadata, weak_matches);
    let replacements = if decorations.whitespace() == WhitespaceChoice::Show
        && decorations.whitespace() == state.whitespace()
    {
        whitespace_replacements(state, row, row_index, line_decoration)
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
    kind: LineKind,
    decoration: Option<&LineDecoration>,
) -> Vec<StyledRange> {
    let role = match kind {
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
        for mapping in row.span_mappings() {
            let logical_start = changed.start.max(mapping.logical.start);
            let logical_end = changed.end.min(mapping.logical.end);
            if logical_start >= logical_end {
                continue;
            }

            if mapping.is_tab() {
                let primary =
                    state.visible_row_is_primary_for_offset(row_index, mapping.logical.start);
                for rendered in mapping.rendered.clone() {
                    if row.text.as_bytes().get(rendered) == Some(&b' ') {
                        let glyph = if primary && rendered == mapping.rendered.start {
                            '→'
                        } else {
                            '·'
                        };
                        replacements.insert(rendered, glyph);
                    }
                }
                continue;
            }

            if mapping.logical.len() != mapping.rendered.len() {
                continue;
            }
            let rendered_start = mapping.rendered.start + logical_start - mapping.logical.start;
            let rendered_end = mapping.rendered.start + logical_end - mapping.logical.start;
            let Some(text) = row.text.get(rendered_start..rendered_end) else {
                continue;
            };
            for (relative, character) in text.char_indices() {
                if character == ' ' {
                    replacements.insert(rendered_start + relative, '·');
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
    metadata: DocumentLineMetadata,
    weak_matches: &[Range<usize>],
) -> SearchOverlays {
    let current = displayed_current_match(state);
    let mut overlays = SearchOverlays::default();

    let first = weak_matches.partition_point(|found| found.end <= row.content_range.start);
    for found in weak_matches[first..]
        .iter()
        .take_while(|found| found.start < row.content_range.end)
    {
        for mapped in map_logical_range(row, found.clone()) {
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
            if row_contains_logical_offset(state, row_index, current.byte_range.start, metadata)
                && let Some(marker) = current_row_marker(row, metadata.kind)
            {
                overlays.current.push(StyledRange {
                    range: marker,
                    role: SemanticRole::SearchCurrent,
                });
            }
        } else {
            for mapped in map_logical_range(row, current.byte_range.clone()) {
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

fn map_logical_range(row: &VisualRow, logical: Range<usize>) -> Vec<Range<usize>> {
    row.rendered_ranges_for_logical(logical)
}

fn source_content_start(row: &VisualRow, kind: LineKind) -> Option<usize> {
    if !is_source(kind) {
        return None;
    }
    Some(row.rendered_content_range.start)
}

fn current_row_marker(row: &VisualRow, kind: LineKind) -> Option<Range<usize>> {
    if let Some(content_start) = source_content_start(row, kind) {
        let start = content_start.saturating_sub(1);
        return next_grapheme_range(&row.text, start);
    }
    next_grapheme_range(&row.text, 0)
}

fn row_contains_logical_offset(
    state: &AppState,
    row_index: usize,
    offset: usize,
    metadata: DocumentLineMetadata,
) -> bool {
    if state.visible_row_is_primary_for_offset(row_index, offset) {
        return true;
    }
    let Some(row) = state.visible_rows().get(row_index) else {
        return false;
    };
    metadata
        .display_len
        .exact()
        .is_some_and(|length| offset == length)
        && row.line_id == metadata.id
        && row.content_range.end == offset
        && !state.visible_rows()[row_index + 1..]
            .iter()
            .any(|later| later.line_id == metadata.id && later.content_range.end == offset)
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
    let detail_rows = match selected_original.and_then(|index| state.document().files().get(index))
    {
        Some(file) => match (&file.old_path, &file.new_path) {
            (Some(old), Some(new)) if old != new => {
                render_detail_parts(frame, area, &[old, " → ", new], theme)
            }
            (Some(path), _) | (_, Some(path)) => render_detail_parts(frame, area, &[path], theme),
            (None, None) => render_detail_parts(frame, area, &["(unparsed path)"], theme),
        },
        None => render_detail_parts(frame, area, &["changed files"], theme),
    };

    let list_y = area.y.saturating_add(detail_rows as u16);
    let list_height = area.height.saturating_sub(detail_rows as u16);
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

fn render_detail_parts(frame: &mut Frame<'_>, area: Rect, parts: &[&str], theme: &Theme) -> usize {
    for_each_detail_line(
        parts,
        usize::from(area.width),
        usize::from(area.height),
        |row, text| {
            frame.render_widget(
                Paragraph::new(text).style(theme.style(SemanticRole::Sticky)),
                Rect::new(area.x, area.y.saturating_add(row as u16), area.width, 1),
            );
        },
    )
}

fn for_each_detail_line(
    parts: &[&str],
    width: usize,
    height: usize,
    mut emit: impl FnMut(usize, String),
) -> usize {
    if width == 0 || height == 0 {
        return 0;
    }
    let available = height.saturating_sub(1).max(1);
    let mut emitted = 0_usize;
    let mut row = String::new();
    let mut row_cells = 0_usize;
    let mut scanned = 0_usize;
    let mut truncated = false;

    'parts: for part in parts {
        let remaining_scan = MAX_PATH_SCAN_BYTES.saturating_sub(scanned);
        if remaining_scan == 0 {
            truncated = true;
            break;
        }
        let bounded = bounded_prefix(part, remaining_scan);
        scanned = scanned.saturating_add(bounded.len());
        truncated |= bounded.len() < part.len();
        for grapheme in bounded.graphemes(true) {
            let grapheme_cells = display_width(grapheme);
            if !row.is_empty() && row_cells.saturating_add(grapheme_cells) > width {
                emit(emitted, std::mem::take(&mut row));
                emitted = emitted.saturating_add(1);
                row_cells = 0;
                if emitted == available {
                    return emitted;
                }
            }
            if row.len().saturating_add(grapheme.len()) > MAX_DETAIL_ROW_BYTES {
                truncated = true;
                break 'parts;
            }
            row.push_str(grapheme);
            row_cells = row_cells.saturating_add(grapheme_cells);
        }
        if truncated {
            break;
        }
    }

    if truncated
        && row.len().saturating_add('…'.len_utf8()) <= MAX_DETAIL_ROW_BYTES
        && (row_cells < width || row.is_empty())
    {
        row.push('…');
    }
    if !row.is_empty() && emitted < available {
        emit(emitted, row);
        emitted = emitted.saturating_add(1);
    }
    emitted
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

fn render_status(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    lines: &FrameLines,
) {
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
            Paragraph::new(fit_end(&body_status(state, lines), width))
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

fn body_status(state: &AppState, lines: &FrameLines) -> String {
    let position = state.top_anchor().map_or_else(
        || "line 0/0".to_owned(),
        |anchor| {
            let line = anchor.line_id.get().saturating_add(1);
            let total = state.document().line_count();
            if anchor.fragment_ordinal == 0 {
                format!("line {line}/{total}")
            } else {
                format!(
                    "line {line}/{total} fragment {}",
                    anchor.fragment_ordinal.saturating_add(1)
                )
            }
        },
    );
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
    let current = displayed_current_match(state).map(|found| search_position(lines, found));
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

fn search_position(lines: &FrameLines, found: &SearchMatch<crate::model::LineId>) -> String {
    let column = lines
        .current_column(found.line_id)
        .unwrap_or_else(|| found.byte_range.start.saturating_add(1));
    format!("match {}:{}", found.line_index + 1, column)
}

fn fit_file_path(file: &FileEntry, width: usize) -> String {
    match (&file.old_path, &file.new_path) {
        (Some(old), Some(new)) if old != new => {
            let separator = " → ";
            let joined_bytes = old
                .len()
                .saturating_add(separator.len())
                .saturating_add(new.len());
            if joined_bytes <= MAX_PATH_SCAN_BYTES
                && display_width(old)
                    .saturating_add(display_width(separator))
                    .saturating_add(display_width(new))
                    <= width
            {
                return format!("{old}{separator}{new}");
            }
            let old_name = bounded_basename(old);
            let new_name = bounded_basename(new);
            let compact_bytes = old_name
                .len()
                .saturating_add(separator.len())
                .saturating_add(new_name.len());
            if compact_bytes <= MAX_PATH_SCAN_BYTES {
                return middle_elide(&format!("{old_name}{separator}{new_name}"), width);
            }
            let names_width = width.saturating_sub(display_width(separator));
            let old_width = names_width / 2;
            let new_width = names_width.saturating_sub(old_width);
            fit_end(
                &format!(
                    "{}{}{}",
                    left_elide(old_name, old_width),
                    separator,
                    left_elide(new_name, new_width)
                ),
                width,
            )
        }
        (Some(path), _) | (_, Some(path)) => {
            if fits_cells_bounded(path, width) {
                path.to_owned()
            } else {
                left_elide(path, width)
            }
        }
        (None, None) => fit_end("(unparsed path)", width),
    }
}

fn bounded_basename(path: &str) -> &str {
    let suffix = bounded_suffix(path, MAX_PATH_SCAN_BYTES);
    suffix.rsplit('/').next().unwrap_or(suffix)
}

fn fit_end(text: &str, width: usize) -> String {
    if fits_cells_bounded(text, width) {
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
    if fits_cells_bounded(text, width) {
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
    if fits_cells_bounded(text, width) {
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
    let bounded = if from_end {
        bounded_suffix(text, MAX_PATH_SCAN_BYTES)
    } else {
        bounded_prefix(text, MAX_PATH_SCAN_BYTES)
    };
    let mut used = 0_usize;
    if from_end {
        let mut start = bounded.len();
        for (index, grapheme) in bounded.grapheme_indices(true).rev() {
            let grapheme_width = display_width(grapheme);
            if used.saturating_add(grapheme_width) > width {
                break;
            }
            used = used.saturating_add(grapheme_width);
            start = index;
        }
        return bounded[start..].to_owned();
    }

    let mut end = 0_usize;
    for (index, grapheme) in bounded.grapheme_indices(true) {
        let grapheme_width = display_width(grapheme);
        if used.saturating_add(grapheme_width) > width {
            break;
        }
        used = used.saturating_add(grapheme_width);
        end = index.saturating_add(grapheme.len());
    }
    bounded[..end].to_owned()
}

fn fits_cells_bounded(text: &str, width: usize) -> bool {
    text.len() <= MAX_PATH_SCAN_BYTES && display_width(text) <= width
}

fn bounded_prefix(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn bounded_suffix(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len().saturating_sub(max_bytes);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
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
    use crate::model::{ChangeStatus, CommitId, FileId, FileMode, LineSpan};
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
        let _ = state
            .update(Event::Key(KeyEvent::new(
                KeyCode::Char('/'),
                KeyModifiers::NONE,
            )))
            .unwrap();
        let effects = state.update(Event::Paste(pattern.to_owned())).unwrap();
        let task = effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Search(task) => Some(task),
                _ => None,
            })
            .expect("search preview task");
        let result = state.execute_search_task(&task).unwrap();
        let _ = state
            .update(Event::SearchCompleted {
                generation: task.generation,
                document_generation: task.document_generation,
                result,
                preview: true,
            })
            .unwrap();
    }

    #[test]
    fn body_renders_sticky_file_and_dual_gutter() {
        let document = parse_bytes(
            b"diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -10 +10 @@\n-old\n+new\n",
        )
        .unwrap();
        let state =
            AppState::new(Arc::new(document), 60, 8, 4, WhitespaceChoice::Ignore, true).unwrap();
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        let decorations =
            DecorationSet::compute(state.document(), &theme, WhitespaceChoice::Ignore).unwrap();
        let lines = FrameLines::load(&state).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal
            .draw(|frame| render(frame, &state, &theme, &decorations, &lines))
            .unwrap();
        let screen = buffer_text(&terminal);
        assert!(screen.contains("src/main.rs"), "{screen}");
        assert!(screen.contains("10     │-old"), "{screen}");
        assert!(screen.contains("10 │+new"), "{screen}");
        assert!(screen.contains("line 1/6"), "{screen}");
    }

    #[test]
    fn show_whitespace_renders_changed_tab_cells() {
        let document = parse_bytes(
            b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-\tfoo\n+    foo\n",
        )
        .unwrap();
        let state =
            AppState::new(Arc::new(document), 60, 8, 4, WhitespaceChoice::Show, true).unwrap();
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        let decorations =
            DecorationSet::compute(state.document(), &theme, WhitespaceChoice::Show).unwrap();
        let lines = FrameLines::load(&state).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal
            .draw(|frame| render(frame, &state, &theme, &decorations, &lines))
            .unwrap();
        let screen = buffer_text(&terminal);
        assert!(screen.contains("-→···foo"), "{screen}");
        assert!(screen.contains("+····foo"), "{screen}");
    }

    #[test]
    fn width_streamed_source_rows_keep_intraline_and_whitespace_mapping() {
        let tail = "x".repeat(6_000);
        let input = format!(
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-\t {tail}\n+     {tail}\n"
        );
        let document = parse_bytes(input.as_bytes()).unwrap();
        let mut state =
            AppState::new(Arc::new(document), 11, 16, 4, WhitespaceChoice::Show, true).unwrap();
        state
            .update(Event::Key(KeyEvent::new(
                KeyCode::Char('5'),
                KeyModifiers::NONE,
            )))
            .unwrap();
        state
            .update(Event::Key(KeyEvent::new(
                KeyCode::Char('g'),
                KeyModifiers::NONE,
            )))
            .unwrap();
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::TrueColor);
        let decorations =
            DecorationSet::compute(state.document(), &theme, WhitespaceChoice::Show).unwrap();
        let lines = FrameLines::load(&state).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(11, 16)).unwrap();
        terminal
            .draw(|frame| render(frame, &state, &theme, &decorations, &lines))
            .unwrap();
        let screen = buffer_text(&terminal);

        assert!(state.viewport_layout_stats().streaming_navigators_created > 0);
        assert!(screen.contains('→'), "{screen}");
        assert!(screen.contains('·'), "{screen}");
        assert!(lines.retained_bytes() < 1024 * 1024);
    }

    #[test]
    fn streamed_search_overlay_uses_visual_row_mapping_without_frame_text() {
        let mut input = "target ".to_owned();
        input.push_str(&"x".repeat(200_000));
        input.push('\n');
        let document = parse_bytes(input.as_bytes()).unwrap();
        let mut state =
            AppState::new(Arc::new(document), 40, 8, 4, WhitespaceChoice::Ignore, true).unwrap();
        apply_search_preview(&mut state, "target");
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::TrueColor);
        let lines = FrameLines::load(&state).unwrap();
        let line_id = state.document().line_id_at(0).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
        terminal
            .draw(|frame| {
                render(frame, &state, &theme, &DecorationSet::default(), &lines);
            })
            .unwrap();
        let expected = theme.style(SemanticRole::SearchCurrent);

        assert!(state.viewport_layout_stats().streaming_navigators_created > 0);
        assert!(lines.retained_bytes() < 1024 * 1024);
        assert!(lines.weak_matches(line_id).contains(&(0..6)));
        assert!(terminal.backend().buffer().content.iter().any(|cell| {
            cell.symbol() == "t"
                && cell.fg == expected.fg.unwrap()
                && cell.bg == expected.bg.unwrap()
        }));
    }

    #[test]
    fn ansi_heavy_small_display_frame_keeps_only_metadata() {
        let mut input = Vec::with_capacity(1_100_032);
        input.extend_from_slice(b"\x1b]0;");
        input.extend(std::iter::repeat_n(b'x', 1_100_000));
        input.push(0x07);
        input.extend_from_slice(b"visible\n");
        let document = parse_bytes(&input).unwrap();
        let state =
            AppState::new(Arc::new(document), 40, 8, 4, WhitespaceChoice::Ignore, true).unwrap();
        let lines = FrameLines::load(&state).unwrap();
        let line_id = state.document().line_id_at(0).unwrap();
        let metadata = lines.metadata(line_id).unwrap();

        assert!(metadata.raw_len > 1024 * 1024);
        assert!(
            metadata
                .display_len
                .exact()
                .is_some_and(|length| length < 64)
        );
        assert!(lines.retained_bytes() < 1024 * 1024);
        assert!(
            state
                .visible_rows()
                .iter()
                .any(|row| row.text.contains("visible"))
        );
    }

    #[test]
    fn active_search_skips_raw_huge_display_short_frame_preload() {
        let hidden_bytes = 16 * 1024 * 1024;
        let mut input = Vec::with_capacity(hidden_bytes + 32);
        input.extend_from_slice(b"\x1b]0;");
        input.extend(std::iter::repeat_n(b'x', hidden_bytes));
        input.push(0x07);
        input.extend_from_slice("évisible\n".as_bytes());
        let document = parse_bytes(&input).unwrap();
        let mut state =
            AppState::new(Arc::new(document), 40, 8, 4, WhitespaceChoice::Ignore, true).unwrap();
        apply_search_preview(&mut state, "visible");
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::TrueColor);
        let lines = FrameLines::load(&state).unwrap();
        let line_id = state.document().line_id_at(0).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
        terminal
            .draw(|frame| {
                render(frame, &state, &theme, &DecorationSet::default(), &lines);
            })
            .unwrap();
        let expected = theme.style(SemanticRole::SearchCurrent);

        assert_eq!(lines.transient_raw_bytes_read(), 0);
        assert!(lines.weak_matches(line_id).is_empty());
        // `visible`はUTF-8で2 byteの`é`の後ろにある。Decodeしないfallbackはbyte+1の3になる。
        assert_eq!(lines.current_column(line_id), Some(3));
        assert!(terminal.backend().buffer().content.iter().any(|cell| {
            cell.symbol() == "v"
                && cell.fg == expected.fg.unwrap()
                && cell.bg == expected.bg.unwrap()
        }));
    }

    #[test]
    fn frame_preload_rejects_more_than_the_visible_line_cap() {
        let input = "x\n".repeat(MAX_FRAME_LINE_ENTRIES + 1);
        let document = parse_bytes(input.as_bytes()).unwrap();
        let height = u16::try_from(MAX_FRAME_LINE_ENTRIES + 3).unwrap();
        let state = AppState::new(
            Arc::new(document),
            20,
            height,
            4,
            WhitespaceChoice::Ignore,
            true,
        )
        .unwrap();

        assert_eq!(state.visible_rows().len(), MAX_FRAME_LINE_ENTRIES + 1);
        assert!(matches!(
            FrameLines::load(&state),
            Err(FrameLinesError::LineLimit {
                requested,
                limit: MAX_FRAME_LINE_ENTRIES,
            }) if requested == MAX_FRAME_LINE_ENTRIES + 1
        ));
    }

    #[test]
    fn frame_retained_bytes_do_not_scale_with_visible_line_length() {
        fn retained_for(length: usize) -> usize {
            let mut input = "x".repeat(length);
            input.push('\n');
            let document = parse_bytes(input.as_bytes()).unwrap();
            let state = AppState::new(Arc::new(document), 40, 8, 4, WhitespaceChoice::Ignore, true)
                .unwrap();
            FrameLines::load(&state).unwrap().retained_bytes()
        }

        let short = retained_for(8);
        let streamed = retained_for(200_000);
        assert_eq!(short, streamed);
        assert!(streamed <= MAX_FRAME_RETAINED_BYTES);
    }

    #[test]
    fn weak_matches_keep_only_visible_non_empty_ranges_with_a_frame_cap() {
        let dense = compile_smart_case(".").unwrap();
        let text = "x".repeat(1_000);
        let mut remaining = 3;
        let matches =
            weak_matches_in_visible_range(&dense, &text, 500..520, &mut remaining).unwrap();
        assert_eq!(matches, vec![500..501, 501..502, 502..503]);
        assert_eq!(remaining, 0);

        let crossing = compile_smart_case("x+").unwrap();
        let mut remaining = 3;
        assert_eq!(
            weak_matches_in_visible_range(&crossing, &text, 500..520, &mut remaining).unwrap(),
            vec![0..520]
        );

        let zero_width = compile_smart_case("").unwrap();
        let mut remaining = 3;
        assert!(
            weak_matches_in_visible_range(&zero_width, &text, 500..520, &mut remaining)
                .unwrap()
                .is_empty()
        );
        assert_eq!(remaining, 3);

        let large = "x".repeat(100_000);
        let mut remaining = 2;
        assert_eq!(
            weak_matches_in_visible_range(&dense, &large, 90_000..90_020, &mut remaining).unwrap(),
            vec![90_000..90_001, 90_001..90_002]
        );
    }

    #[test]
    fn current_search_style_wins_over_syntax_and_intraline_layers() {
        let document = parse_bytes(
            b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-let old = 1;\n+let new = 2;\n",
        )
        .unwrap();
        let mut state =
            AppState::new(Arc::new(document), 60, 8, 4, WhitespaceChoice::Ignore, true).unwrap();
        apply_search_preview(&mut state, "new");
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::TrueColor);
        let decorations =
            DecorationSet::compute(state.document(), &theme, WhitespaceChoice::Ignore).unwrap();
        let lines = FrameLines::load(&state).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal
            .draw(|frame| render(frame, &state, &theme, &decorations, &lines))
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
        let document = parse_bytes(b"\n").unwrap();
        let mut state =
            AppState::new(Arc::new(document), 20, 6, 4, WhitespaceChoice::Ignore, true).unwrap();
        apply_search_preview(&mut state, "^$");
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        let lines = FrameLines::load(&state).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(20, 6)).unwrap();
        terminal
            .draw(|frame| {
                render(frame, &state, &theme, &DecorationSet::default(), &lines);
            })
            .unwrap();
        let screen = buffer_text(&terminal);
        assert!(screen.contains('▏'), "{screen}");
    }

    #[test]
    fn path_elision_keeps_rename_basenames() {
        let document = parse_bytes(
            b"diff --git a/a/very/long/old-name.rs b/b/very/long/new-name.rs\nsimilarity index 100%\nrename from a/very/long/old-name.rs\nrename to b/very/long/new-name.rs\n",
        )
        .unwrap();
        let file = &document.files()[0];
        let fitted = fit_file_path(file, 28);
        assert!(fitted.contains("old-name"));
        assert!(fitted.contains("new-name"));
        assert!(display_width(&fitted) <= 28);
    }

    #[test]
    fn path_elision_scans_only_a_bounded_suffix_of_huge_slash_free_names() {
        let prefix = "x".repeat(2 * 1024 * 1024);
        let old_path = format!("{prefix}-old-name.rs");
        let new_path = format!("{prefix}-new-name.rs");
        let file = FileEntry {
            id: FileId::new(0),
            commit_id: CommitId::new(0),
            old_path: Some(old_path.clone()),
            new_path: Some(new_path.clone()),
            status: ChangeStatus::Renamed,
            mode: FileMode::Rich,
            span: LineSpan::open(LineId::new(0)),
            hunk_ids: Vec::new(),
            additions: Some(0),
            deletions: Some(0),
        };

        assert!(bounded_basename(&old_path).len() <= MAX_PATH_SCAN_BYTES);
        assert!(bounded_basename(&new_path).len() <= MAX_PATH_SCAN_BYTES);
        let fitted = fit_file_path(&file, 40);
        assert!(fitted.contains("old-name"), "{fitted}");
        assert!(fitted.contains("new-name"), "{fitted}");
        assert!(display_width(&fitted) <= 40);
    }

    #[test]
    fn detail_wrapping_stops_after_the_bounded_path_prefix() {
        let path = "x".repeat(4 * MAX_PATH_SCAN_BYTES);
        let mut emitted_bytes = 0_usize;
        let mut final_row = String::new();
        let rows = for_each_detail_line(&[&path], 80, 1_000, |_, row| {
            assert!(display_width(&row) <= 80);
            assert!(row.len() <= MAX_DETAIL_ROW_BYTES);
            emitted_bytes = emitted_bytes.saturating_add(row.len());
            final_row = row;
        });

        assert!(rows > 1);
        assert!(emitted_bytes <= MAX_PATH_SCAN_BYTES + '…'.len_utf8());
        assert!(final_row.ends_with('…'), "{final_row}");
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
