//! Interactive pager の画面構成と、semantic role から Ratatui widget への変換。
//!
//! このmoduleはAppState、DecorationSet、draw前に構築した[`FrameLines`]だけを読み、sticky stack、本文、
//! status bar、help/file-list overlayを1 frameへ描く。Raw store I/O、入力event、terminal lifecycle、
//! background workerは扱わない。Frame preloadはfixed metadataとbounded search rangeだけを保持し、検索用に
//! 一時decodeしたline textはdraw前に破棄する。Syntax foreground、intraline background、searchの順に
//! styleを重ね、全spanをlayoutが持つbyte mappingでtab展開・折り返し後のvisual rowへ写す。

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use ratatui::Frame;
use ratatui::buffer::{Buffer, CellWidth};
use ratatui::layout::Rect;
use ratatui::style::Modifier;
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
use crate::document::{
    DocumentError, DocumentIdentity, DocumentLineMetadata, DocumentLineMetadataReader,
};
use crate::model::{CommitId, CommitKind, FileId, LineId, LineKind};
use crate::search::{CompiledSearch, IncrementalQueryStatus, SearchMatch};
use crate::state::{
    AppState, FileListItem, FileListState, Mode, SearchInputState, TextInput,
    first_unfiltered_file_list_item, next_unfiltered_file_list_item,
    previous_unfiltered_file_list_item,
};
use crate::structure_store::{
    StoredCommitRef, StoredFileRef, StructureMetadataReader, StructureReader, StructureStoreError,
};
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
// Input全体はfilter contractの上限まで保持する一方、status rowが1 frameでsegment/width計算する
// 範囲はcursor周辺だけにする。幅0のformat characterでも描画時間と一時Stringを入力長へ比例させない。
const MAX_INPUT_WINDOW_SCAN_BYTES: usize = 4 * 1024;
const MAX_FALLBACK_FILES_PER_FRAME: u64 = 4_096;
/// 1 frameが参照するfixed metadataと事前計算済みsearch rangeを保持するbounded data。
///
/// [`FrameLoader::load`]は`Terminal::draw`より前に呼ぶ。Rendererはこの値だけを読み、raw storeや
/// structure spill fileへI/Oを発生させない。Weak matchとsearch columnにfull textが要るlineもreaderの
/// 借用buffer内で計算し、`FrameLines`へはrange、表示済み文字列、数値だけを移す。
#[derive(Debug, Default)]
pub struct FrameLines {
    lines: HashMap<LineId, FrameLine>,
    current_column: Option<(LineId, usize)>,
    transient_raw_bytes_read: u64,
    sticky_headers: Vec<String>,
    inline_context_headers: HashMap<LineId, Vec<String>>,
    fallback_found: bool,
    fallback_scan_pending: bool,
    file_list: Option<FileListFrame>,
}

#[derive(Debug)]
struct FrameLine {
    metadata: DocumentLineMetadata,
    weak_matches: Vec<Range<usize>>,
}

/// File-list modeがdraw中に参照するowned表示data。
///
/// `overlay_rows`はselection周辺だけをGit出力順で保持する。Path、commit subject、object IDは
/// [`FrameLoader`]内でdecodeと幅調整を終え、この値にはspill readerの借用を残さない。
#[derive(Debug, Default)]
struct FileListFrame {
    detail_rows: Vec<String>,
    overlay_rows: Vec<FileListRow>,
    total_matches: u64,
    loading: bool,
}

#[derive(Debug)]
struct FileListRow {
    text: String,
    role: SemanticRole,
    selected: bool,
}

/// Virtual file-list item 1件をowned表示行へ変換した値。
///
/// Commit headerは隣接itemと同じcommitなら挿入時に省く。選択行を先に`FileListFrame`へ保持してから
/// 前後itemを追加するため、長いsubject/pathが4 MiB枠を使い切ってもselectionは残る。
#[derive(Debug)]
struct PreparedFileListItem {
    commit_id: CommitId,
    header: FileListRow,
    entry: FileListRow,
}

/// Visible frameの事前decodeに失敗した理由。
#[derive(Debug, Error)]
pub enum FrameLinesError {
    #[error(transparent)]
    Document(#[from] DocumentError),
    #[error(transparent)]
    BoundedText(#[from] BoundedTextError),
    #[error(transparent)]
    Structure(#[from] StructureStoreError),
    #[error("document snapshot does not contain frame logical line {line_id}")]
    MissingLine { line_id: LineId },
    #[error("could not allocate frame line index for {requested} logical lines")]
    Allocation { requested: usize },
    #[error("frame references {requested} logical lines, exceeding the {limit}-line bound")]
    LineLimit { requested: usize, limit: usize },
    #[error("frame metadata/ranges retain {requested} bytes, exceeding the {limit}-byte bound")]
    FrameBudget { requested: usize, limit: usize },
    #[error("document snapshot does not contain file-list file {file_id}")]
    MissingFile { file_id: FileId },
    #[error("document snapshot does not contain file-list commit {commit_id}")]
    MissingCommit { commit_id: crate::model::CommitId },
}

/// Draw前のbounded preloadに使うreaderと、document世代をまたいで進めるfallback走査cursor。
///
/// 1 instanceをapp loop全体で再利用する。Document identityが変わった時だけfallback cursorを0へ戻し、
/// 同じbuilder lineageの追記snapshotでは前frameまでに調べたfinalized fileを再走査しない。
#[derive(Debug)]
pub struct FrameLoader {
    text_reader: BoundedDocumentTextReader,
    line_metadata_reader: DocumentLineMetadataReader,
    file_reader: StructureReader,
    file_metadata_reader: StructureMetadataReader,
    commit_reader: StructureReader,
    fallback_reader: StructureMetadataReader,
    fallback_identity: Option<DocumentIdentity>,
    fallback_cursor: u64,
    fallback_found: bool,
}

impl Default for FrameLoader {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameLoader {
    /// Allocation前のreader群と未走査のfallback stateを作る。
    #[must_use]
    pub fn new() -> Self {
        Self {
            text_reader: BoundedDocumentTextReader::new(),
            line_metadata_reader: DocumentLineMetadataReader::new(),
            file_reader: StructureReader::new(),
            file_metadata_reader: StructureMetadataReader::new(),
            commit_reader: StructureReader::new(),
            fallback_reader: StructureMetadataReader::new(),
            fallback_identity: None,
            fallback_cursor: 0,
            fallback_found: false,
        }
    }

    /// Current modeがdraw中に参照するmetadataと表示文字列をpreloadする。
    ///
    /// File-listではbody/search lineを読まず、Body/Searchではfile-list pathを読まない。Raw長がeager
    /// decoder上限を越えるlineはcurrent match columnをbyte offsetへfallbackさせ、weak matchだけを
    /// 省略する。戻り値のretained allocationは4 MiB以内に制限する。
    pub fn load(&mut self, state: &AppState) -> Result<FrameLines, FrameLinesError> {
        if let Mode::FileList(list) = state.mode() {
            return self.load_file_list(state, list);
        }

        if matches!(state.mode(), Mode::Help { .. }) {
            return Ok(FrameLines::default());
        }

        let sticky_headers = self.load_sticky_headers(state)?;
        let inline_context_headers = self.load_inline_context_headers(state)?;
        let (fallback_found, fallback_scan_pending) = self.advance_fallback_scan(state)?;
        let visible = state.visible_rows();
        let visible_ranges = visible_logical_ranges(visible)?;
        let requested = visible.len();
        let mut requested_ids = Vec::new();
        requested_ids
            .try_reserve(requested.min(MAX_FRAME_LINE_ENTRIES))
            .map_err(|_| FrameLinesError::Allocation { requested })?;
        let mut requested_seen = HashSet::new();
        requested_seen
            .try_reserve(requested.min(MAX_FRAME_LINE_ENTRIES))
            .map_err(|_| FrameLinesError::Allocation { requested })?;
        for line_id in visible.iter().map(|row| row.line_id) {
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
        let reserved = retained_frame_index_bytes(lines.capacity())
            .saturating_add(retained_header_rows(
                &sticky_headers,
                sticky_headers.capacity(),
            ))
            .saturating_add(retained_inline_context_headers(&inline_context_headers));
        if reserved > MAX_FRAME_RETAINED_BYTES {
            return Err(FrameLinesError::FrameBudget {
                requested: reserved,
                limit: MAX_FRAME_RETAINED_BYTES,
            });
        }
        let compiled = displayed_search(state);
        let current_match = displayed_current_match(state);
        let mut remaining_weak_matches = MAX_WEAK_MATCH_SPANS_PER_FRAME;
        let mut current_column =
            current_match.map(|found| (found.line_id, found.byte_range.start.saturating_add(1)));
        let raw_bytes_before = self.text_reader.large_line_stats().raw_bytes_read;
        for line_id in requested_ids {
            let metadata = self
                .line_metadata_reader
                .read(state.document(), line_id)?
                .ok_or(FrameLinesError::MissingLine { line_id })?;
            let weak_text_required = remaining_weak_matches > 0
                && compiled.is_some()
                && visible_ranges.contains_key(&line_id);
            let current_text_required = current_match.is_some_and(|found| {
                found.line_id == line_id && visible_ranges.contains_key(&line_id)
            });
            let requires_text = weak_text_required || current_text_required;
            let raw_is_eager = u64::try_from(MAX_EAGER_TEXT_RAW_BYTES)
                .is_ok_and(|limit| metadata.raw_len <= limit);
            let mut weak_matches = Vec::new();
            if requires_text
                && raw_is_eager
                && let BoundedTextRead::Available { text, .. } = self.text_reader.read(
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
                .saturating_add(retained_header_rows(
                    &sticky_headers,
                    sticky_headers.capacity(),
                ))
                .saturating_add(retained_inline_context_headers(&inline_context_headers))
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
        let frame = FrameLines {
            lines,
            current_column,
            transient_raw_bytes_read: self
                .text_reader
                .large_line_stats()
                .raw_bytes_read
                .saturating_sub(raw_bytes_before),
            sticky_headers,
            inline_context_headers,
            fallback_found,
            fallback_scan_pending,
            file_list: None,
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

    fn advance_fallback_scan(&mut self, state: &AppState) -> Result<(bool, bool), FrameLinesError> {
        let identity = state.document().identity();
        if self.fallback_identity.as_ref() != Some(&identity) {
            self.fallback_identity = Some(identity);
            self.fallback_cursor = 0;
            self.fallback_found = false;
            self.fallback_reader.reset_statistics();
        }

        let structure = state.document().structure();
        let finalized = structure.finalized_file_count();
        if !self.fallback_found {
            let end = self
                .fallback_cursor
                .saturating_add(MAX_FALLBACK_FILES_PER_FRAME)
                .min(finalized);
            while self.fallback_cursor < end {
                let file_id = FileId::new(self.fallback_cursor);
                let file = self
                    .fallback_reader
                    .read_file(structure, file_id)?
                    .ok_or(FrameLinesError::MissingFile { file_id })?;
                self.fallback_cursor = self.fallback_cursor.saturating_add(1);
                if !file.mode.is_rich() {
                    self.fallback_found = true;
                    break;
                }
            }
        }

        // Open overlayはfixed tableへ移るまで同じIDのままmodeがdowngradeし得るため、frameごとに
        // current 1件だけを見る。Fallbackからrichへ戻る遷移はparser contractにないので発見後は保持する。
        if structure
            .open_file()
            .is_some_and(|file| !file.mode.is_rich())
        {
            self.fallback_found = true;
        }
        let pending = !self.fallback_found && self.fallback_cursor < finalized;
        Ok((self.fallback_found, pending))
    }

    fn load_sticky_headers(&mut self, state: &AppState) -> Result<Vec<String>, FrameLinesError> {
        let width = usize::from(state.terminal_width());
        let Some(row) = state.current_row() else {
            return Ok(vec![fit_end("git-ultrapager", width)]);
        };
        let structure = state.document().structure();
        if let Some(file_id) = row.file_id {
            let counts = structure.visible_counts()?;
            let file = self
                .file_reader
                .read_file(structure, file_id)?
                .ok_or(FrameLinesError::MissingFile { file_id })?;
            let commit = self
                .commit_reader
                .read_commit(structure, file.commit_id)?
                .ok_or(FrameLinesError::MissingCommit {
                    commit_id: file.commit_id,
                })?;
            let total = state
                .document()
                .is_complete()
                .then(|| counts.files.to_string());
            return Ok(vec![
                fit_end(&commit_label(commit), width),
                sticky_file_header(file, total.as_deref(), width),
            ]);
        }
        if let Some(commit_id) = row.commit_id {
            let commit = self
                .commit_reader
                .read_commit(structure, commit_id)?
                .ok_or(FrameLinesError::MissingCommit { commit_id })?;
            return Ok(vec![fit_end(&commit_label(commit), width)]);
        }
        Ok(vec![fit_end("git-ultrapager", width)])
    }

    /// Body先頭より後にcommit/file先頭が現れる場合だけ、stickyと同じlabelを仮想行としてpreloadする。
    ///
    /// Body先頭のcontextは固定sticky stackがすでに担うため重複させない。同じlogical lineで
    /// working-tree commitとfileが始まる場合はcommit、fileの順に保持する。
    fn load_inline_context_headers(
        &mut self,
        state: &AppState,
    ) -> Result<HashMap<LineId, Vec<String>>, FrameLinesError> {
        let visible = state.visible_rows();
        let mut headers = HashMap::new();
        let requested = visible
            .windows(2)
            .filter(|pair| {
                !pair[1].continuation
                    && (pair[1].commit_id.is_some() && pair[1].commit_id != pair[0].commit_id
                        || pair[1].file_id.is_some() && pair[1].file_id != pair[0].file_id)
            })
            .count();
        headers
            .try_reserve(requested.min(MAX_FRAME_LINE_ENTRIES))
            .map_err(|_| FrameLinesError::Allocation { requested })?;
        if visible.len() < 2 {
            return Ok(headers);
        }

        let structure = state.document().structure();
        let counts = structure.visible_counts()?;
        let total = state
            .document()
            .is_complete()
            .then(|| counts.files.to_string());
        let width = usize::from(state.terminal_width());
        let mut previous_commit = visible[0].commit_id;
        let mut previous_file = visible[0].file_id;
        for row in &visible[1..] {
            if row.continuation {
                continue;
            }
            let mut row_headers = Vec::new();
            if let Some(commit_id) = row.commit_id
                && previous_commit != Some(commit_id)
            {
                let commit = self
                    .commit_reader
                    .read_commit(structure, commit_id)?
                    .ok_or(FrameLinesError::MissingCommit { commit_id })?;
                if row.line_id == commit.span.start {
                    row_headers.push(fit_end(&commit_label(commit), width));
                }
            }
            if let Some(file_id) = row.file_id
                && previous_file != Some(file_id)
            {
                let file = self
                    .file_reader
                    .read_file(structure, file_id)?
                    .ok_or(FrameLinesError::MissingFile { file_id })?;
                if row.line_id == file.span.start {
                    row_headers.push(sticky_file_header(file, total.as_deref(), width));
                }
            }
            previous_commit = row.commit_id;
            previous_file = row.file_id;
            if row_headers.is_empty() {
                continue;
            }
            headers.insert(row.line_id, row_headers);
            let retained = retained_frame_index_bytes(0)
                .saturating_add(retained_inline_context_headers(&headers));
            if retained > MAX_FRAME_RETAINED_BYTES {
                return Err(FrameLinesError::FrameBudget {
                    requested: retained,
                    limit: MAX_FRAME_RETAINED_BYTES,
                });
            }
        }
        Ok(headers)
    }

    fn load_file_list(
        &mut self,
        state: &AppState,
        list: &FileListState,
    ) -> Result<FrameLines, FrameLinesError> {
        let width = usize::from(state.terminal_width());
        let content_height = usize::from(state.terminal_height().saturating_sub(1));
        let filtered = list
            .filter
            .as_ref()
            .is_some_and(|filter| !filter.text().is_empty());
        let total_matches = list
            .window
            .as_ref()
            .map_or(0, |window| window.total_matches);
        let selected_file = if filtered {
            list.window
                .as_ref()
                .and_then(|window| window.selected_file())
                .or(list.selected_file)
        } else {
            list.selected_item
                .and_then(FileListItem::file_id)
                .or(list.selected_file)
        };
        let selected_item = if filtered {
            selected_file.map(FileListItem::File)
        } else if let Some(item) = list
            .selected_item
            .or_else(|| selected_file.map(FileListItem::File))
        {
            Some(item)
        } else {
            first_unfiltered_file_list_item(
                &mut self.file_metadata_reader,
                state.document().structure(),
            )?
        };
        let mut file_list = FileListFrame {
            detail_rows: Vec::new(),
            overlay_rows: Vec::new(),
            total_matches,
            loading: list.loading || !state.document().is_complete(),
        };

        file_list.detail_rows =
            self.load_file_list_detail(state, selected_item, width, content_height)?;
        let retained_base = retained_frame_index_bytes(0);
        let detail_retained = retained_base.saturating_add(retained_file_list_bytes(&file_list));
        if detail_retained > MAX_FRAME_RETAINED_BYTES {
            return Err(FrameLinesError::FrameBudget {
                requested: detail_retained,
                limit: MAX_FRAME_RETAINED_BYTES,
            });
        }

        let available_rows = content_height.saturating_sub(file_list.detail_rows.len());
        let entry_limit = available_rows.min(MAX_FRAME_LINE_ENTRIES);
        if !filtered {
            return self.load_unfiltered_file_list_rows(
                state,
                file_list,
                selected_item,
                width,
                entry_limit,
            );
        }

        let Some(window) = list.window.as_ref() else {
            let text = if file_list.loading {
                "(loading files…)"
            } else {
                "(no matching files)"
            };
            let _ = push_file_list_row(
                &mut file_list,
                FileListRow {
                    text: fit_end(text, width),
                    role: SemanticRole::Dim,
                    selected: false,
                },
            );
            return finish_file_list_frame(file_list);
        };
        if window.total_matches == 0 || window.file_ids.is_empty() || entry_limit == 0 {
            let text = if file_list.loading {
                "(loading files…)"
            } else {
                "(no matching files)"
            };
            let _ = push_file_list_row(
                &mut file_list,
                FileListRow {
                    text: fit_end(text, width),
                    role: SemanticRole::Dim,
                    selected: false,
                },
            );
            return finish_file_list_frame(file_list);
        }

        let selected_index = window
            .selected_rank
            .and_then(|rank| rank.checked_sub(window.start_rank))
            .and_then(|offset| usize::try_from(offset).ok())
            .filter(|index| *index < window.file_ids.len())
            .or_else(|| {
                selected_file.and_then(|selected| {
                    window
                        .file_ids
                        .iter()
                        .position(|candidate| *candidate == selected)
                })
            })
            .unwrap_or(0);
        self.load_filtered_file_list_rows(
            state,
            file_list,
            window,
            selected_index,
            width,
            entry_limit,
        )
    }

    fn load_file_list_detail(
        &mut self,
        state: &AppState,
        selected_item: Option<FileListItem>,
        width: usize,
        height: usize,
    ) -> Result<Vec<String>, FrameLinesError> {
        let structure = state.document().structure();
        match selected_item {
            Some(FileListItem::File(file_id)) => {
                let file = self
                    .file_reader
                    .read_file(structure, file_id)?
                    .ok_or(FrameLinesError::MissingFile { file_id })?;
                let parts = match (file.old_path, file.new_path) {
                    (Some(old), Some(new)) if old != new => vec![old, " → ", new],
                    (Some(path), _) | (_, Some(path)) => vec![path],
                    (None, None) => vec!["(unparsed path)"],
                };
                detail_lines(&parts, width, height)
            }
            Some(FileListItem::EmptyCommit(commit_id)) => {
                let commit = self
                    .commit_reader
                    .read_commit(structure, commit_id)?
                    .ok_or(FrameLinesError::MissingCommit { commit_id })?;
                let detail = format!("{}  (no file changes)", commit_label(commit));
                detail_lines(&[detail.as_str()], width, height)
            }
            None => detail_lines(&["changed files"], width, height),
        }
    }

    fn prepare_file_list_item(
        &mut self,
        state: &AppState,
        item: FileListItem,
        width: usize,
        selected: bool,
    ) -> Result<PreparedFileListItem, FrameLinesError> {
        let structure = state.document().structure();
        let (commit_id, entry) = match item {
            FileListItem::File(file_id) => {
                let file = self
                    .file_reader
                    .read_file(structure, file_id)?
                    .ok_or(FrameLinesError::MissingFile { file_id })?;
                (
                    file.commit_id,
                    FileListRow {
                        text: file_list_entry(file, width),
                        role: SemanticRole::Plain,
                        selected,
                    },
                )
            }
            FileListItem::EmptyCommit(commit_id) => (
                commit_id,
                FileListRow {
                    text: fit_end("  (no file changes)", width),
                    role: SemanticRole::Dim,
                    selected,
                },
            ),
        };
        let commit = self
            .commit_reader
            .read_commit(structure, commit_id)?
            .ok_or(FrameLinesError::MissingCommit { commit_id })?;
        if matches!(item, FileListItem::EmptyCommit(_))
            && (!commit.finalized || commit.files.count() != 0)
        {
            return Err(FrameLinesError::MissingCommit { commit_id });
        }
        Ok(PreparedFileListItem {
            commit_id,
            header: FileListRow {
                text: fit_end(&commit_label(commit), width),
                role: SemanticRole::CommitHeader,
                selected: false,
            },
            entry,
        })
    }

    fn load_unfiltered_file_list_rows(
        &mut self,
        state: &AppState,
        mut file_list: FileListFrame,
        selected_item: Option<FileListItem>,
        width: usize,
        entry_limit: usize,
    ) -> Result<FrameLines, FrameLinesError> {
        let Some(selected_item) = selected_item else {
            let text = if file_list.loading {
                "(loading files…)"
            } else {
                "(no matching files)"
            };
            let _ = push_file_list_row(
                &mut file_list,
                FileListRow {
                    text: fit_end(text, width),
                    role: SemanticRole::Dim,
                    selected: false,
                },
            );
            return finish_file_list_frame(file_list);
        };

        let selected = self.prepare_file_list_item(state, selected_item, width, true)?;
        let mut first_item = selected_item;
        let mut last_item = selected_item;
        let mut first_commit = selected.commit_id;
        let mut last_commit = selected.commit_id;
        install_selected_file_list_item(&mut file_list, selected)?;

        let row_limit = entry_limit.clamp(2, MAX_FRAME_LINE_ENTRIES);
        let mut before_complete = false;
        let mut after_complete = false;
        while file_list.overlay_rows.len() < row_limit && !(before_complete && after_complete) {
            let mut added = false;
            if !before_complete {
                match previous_unfiltered_file_list_item(
                    &mut self.file_metadata_reader,
                    state.document().structure(),
                    first_item,
                )? {
                    Some(previous) => {
                        let prepared =
                            self.prepare_file_list_item(state, previous, width, false)?;
                        let commit_id = prepared.commit_id;
                        if insert_prepared_file_list_item(
                            &mut file_list,
                            prepared,
                            first_commit,
                            true,
                            row_limit,
                        ) {
                            first_item = previous;
                            first_commit = commit_id;
                            added = true;
                        } else {
                            before_complete = true;
                        }
                    }
                    None => before_complete = true,
                }
            }
            if !after_complete && file_list.overlay_rows.len() < row_limit {
                match next_unfiltered_file_list_item(
                    &mut self.file_metadata_reader,
                    state.document().structure(),
                    last_item,
                )? {
                    Some(next) => {
                        let prepared = self.prepare_file_list_item(state, next, width, false)?;
                        let commit_id = prepared.commit_id;
                        if insert_prepared_file_list_item(
                            &mut file_list,
                            prepared,
                            last_commit,
                            false,
                            row_limit,
                        ) {
                            last_item = next;
                            last_commit = commit_id;
                            added = true;
                        } else {
                            after_complete = true;
                        }
                    }
                    None => after_complete = true,
                }
            }
            if !added && (before_complete || after_complete) {
                break;
            }
        }
        finish_file_list_frame(file_list)
    }

    fn load_filtered_file_list_rows(
        &mut self,
        state: &AppState,
        mut file_list: FileListFrame,
        window: &crate::state::FileFilterWindow,
        selected_index: usize,
        width: usize,
        entry_limit: usize,
    ) -> Result<FrameLines, FrameLinesError> {
        let selected_file = window.file_ids[selected_index];
        let selected =
            self.prepare_file_list_item(state, FileListItem::File(selected_file), width, true)?;
        let mut first_index = selected_index;
        let mut last_index = selected_index;
        let mut first_commit = selected.commit_id;
        let mut last_commit = selected.commit_id;
        install_selected_file_list_item(&mut file_list, selected)?;

        let row_limit = entry_limit.clamp(2, MAX_FRAME_LINE_ENTRIES);
        let mut before_complete = first_index == 0;
        let mut after_complete = last_index.saturating_add(1) == window.file_ids.len();
        while file_list.overlay_rows.len() < row_limit && !(before_complete && after_complete) {
            let mut added = false;
            if !before_complete {
                let previous_index = first_index - 1;
                let prepared = self.prepare_file_list_item(
                    state,
                    FileListItem::File(window.file_ids[previous_index]),
                    width,
                    false,
                )?;
                let commit_id = prepared.commit_id;
                if insert_prepared_file_list_item(
                    &mut file_list,
                    prepared,
                    first_commit,
                    true,
                    row_limit,
                ) {
                    first_index = previous_index;
                    first_commit = commit_id;
                    before_complete = first_index == 0;
                    added = true;
                } else {
                    before_complete = true;
                }
            }
            if !after_complete && file_list.overlay_rows.len() < row_limit {
                let next_index = last_index.saturating_add(1);
                let prepared = self.prepare_file_list_item(
                    state,
                    FileListItem::File(window.file_ids[next_index]),
                    width,
                    false,
                )?;
                let commit_id = prepared.commit_id;
                if insert_prepared_file_list_item(
                    &mut file_list,
                    prepared,
                    last_commit,
                    false,
                    row_limit,
                ) {
                    last_index = next_index;
                    last_commit = commit_id;
                    after_complete = last_index.saturating_add(1) == window.file_ids.len();
                    added = true;
                } else {
                    after_complete = true;
                }
            }
            if !added && (before_complete || after_complete) {
                break;
            }
        }
        finish_file_list_frame(file_list)
    }
}

impl FrameLines {
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

    /// Commit/file開始行の直前へ表示する、事前整形済みcontext header stackを返す。
    #[must_use]
    pub fn inline_context_headers(&self, line_id: LineId) -> &[String] {
        self.inline_context_headers
            .get(&line_id)
            .map_or(&[], Vec::as_slice)
    }

    /// Rendererへ保持するmetadata、range vector、hash tableの保守的byte数。
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        retained_frame_index_bytes(self.lines.capacity())
            .saturating_add(retained_weak_match_bytes(&self.lines))
            .saturating_add(retained_header_rows(
                &self.sticky_headers,
                self.sticky_headers.capacity(),
            ))
            .saturating_add(retained_inline_context_headers(
                &self.inline_context_headers,
            ))
            .saturating_add(self.file_list.as_ref().map_or(0, retained_file_list_bytes))
    }

    /// Preload中にbounded streaming readerがraw storeから読んだbyte数。
    #[must_use]
    pub const fn transient_raw_bytes_read(&self) -> u64 {
        self.transient_raw_bytes_read
    }

    /// Bounded fallback走査に未処理fileがあり、eventを待たず次frameをpreloadする必要があるかを返す。
    #[must_use]
    pub const fn needs_follow_up_frame(&self) -> bool {
        self.fallback_scan_pending
    }
}

fn detail_lines(
    parts: &[&str],
    width: usize,
    height: usize,
) -> Result<Vec<String>, FrameLinesError> {
    let mut rows = Vec::new();
    rows.try_reserve(height.min(MAX_FRAME_LINE_ENTRIES))
        .map_err(|_| FrameLinesError::Allocation { requested: height })?;
    for_each_detail_line(parts, width, height, |_, row| rows.push(row));
    Ok(rows)
}

fn install_selected_file_list_item(
    file_list: &mut FileListFrame,
    prepared: PreparedFileListItem,
) -> Result<(), FrameLinesError> {
    let PreparedFileListItem { header, entry, .. } = prepared;
    if push_file_list_rows(file_list, vec![header, entry]) {
        return Ok(());
    }
    Err(FrameLinesError::FrameBudget {
        requested: MAX_FRAME_RETAINED_BYTES.saturating_add(1),
        limit: MAX_FRAME_RETAINED_BYTES,
    })
}

/// Selectionを含む既存行を先に保持したまま、隣接itemを前後へatomicに挿入する。
///
/// 同じcommitのfileは既存headerを共有する。前側へ追加する場合はheaderの直後へfile行を置くため、
/// 同一commit内を逆向きに広げてもGit順を保つ。
fn insert_prepared_file_list_item(
    file_list: &mut FileListFrame,
    prepared: PreparedFileListItem,
    adjacent_commit: CommitId,
    before: bool,
    row_limit: usize,
) -> bool {
    let same_commit = prepared.commit_id == adjacent_commit;
    let PreparedFileListItem { header, entry, .. } = prepared;
    let rows = if same_commit {
        vec![entry]
    } else {
        vec![header, entry]
    };
    if file_list.overlay_rows.len().saturating_add(rows.len()) > row_limit {
        return false;
    }
    let insert_at = if before {
        usize::from(same_commit)
    } else {
        file_list.overlay_rows.len()
    };
    insert_file_list_rows(file_list, insert_at, rows)
}

fn insert_file_list_rows(
    file_list: &mut FileListFrame,
    insert_at: usize,
    rows: Vec<FileListRow>,
) -> bool {
    let previous_len = file_list.overlay_rows.len();
    let inserted = rows.len();
    if !push_file_list_rows(file_list, rows) {
        return false;
    }
    file_list.overlay_rows[insert_at..].rotate_right(inserted);
    debug_assert_eq!(file_list.overlay_rows.len(), previous_len + inserted);
    true
}

fn push_file_list_rows(file_list: &mut FileListFrame, rows: Vec<FileListRow>) -> bool {
    let previous_len = file_list.overlay_rows.len();
    for row in rows {
        if !push_file_list_row(file_list, row) {
            file_list.overlay_rows.truncate(previous_len);
            file_list.overlay_rows.shrink_to_fit();
            return false;
        }
    }
    true
}

fn push_file_list_row(file_list: &mut FileListFrame, row: FileListRow) -> bool {
    file_list.overlay_rows.push(row);
    let retained =
        retained_frame_index_bytes(0).saturating_add(retained_file_list_bytes(file_list));
    if retained <= MAX_FRAME_RETAINED_BYTES {
        return true;
    }
    file_list.overlay_rows.pop();
    // `Vec::push`で増えたcapacityもretained budgetに含む。失敗行をpopするだけではcapacityが残るため、
    // 次frameへ4 MiB超のallocationを渡さないよう失敗時だけ縮める。
    file_list.overlay_rows.shrink_to_fit();
    false
}

fn finish_file_list_frame(file_list: FileListFrame) -> Result<FrameLines, FrameLinesError> {
    let frame = FrameLines {
        file_list: Some(file_list),
        ..FrameLines::default()
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

fn retained_header_rows(headers: &[String], capacity: usize) -> usize {
    capacity
        .saturating_mul(std::mem::size_of::<String>())
        .saturating_add(headers.iter().fold(0_usize, |total, header| {
            total.saturating_add(header.capacity())
        }))
}

fn retained_inline_context_headers(headers: &HashMap<LineId, Vec<String>>) -> usize {
    headers
        .capacity()
        .saturating_mul(
            std::mem::size_of::<(LineId, Vec<String>)>() + 2 * std::mem::size_of::<usize>(),
        )
        .saturating_add(headers.values().fold(0_usize, |total, rows| {
            total.saturating_add(retained_header_rows(rows, rows.capacity()))
        }))
}

fn retained_file_list_bytes(file_list: &FileListFrame) -> usize {
    let detail_rows = file_list
        .detail_rows
        .capacity()
        .saturating_mul(std::mem::size_of::<String>());
    let detail_text = file_list
        .detail_rows
        .iter()
        .fold(0_usize, |total, row| total.saturating_add(row.capacity()));
    let overlay_rows = file_list
        .overlay_rows
        .capacity()
        .saturating_mul(std::mem::size_of::<FileListRow>());
    let overlay_text = file_list.overlay_rows.iter().fold(0_usize, |total, row| {
        total.saturating_add(row.text.capacity())
    });
    detail_rows
        .saturating_add(detail_text)
        .saturating_add(overlay_rows)
        .saturating_add(overlay_text)
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
/// Exact document revisionがstateと異なるDecorationSetはset全体を無視する。同じparser lineageでも
/// open file/hunkのfallback化でline semanticsが変わるため、syntaxだけを後続snapshotへ流用しない。
/// Revisionが一致し、whitespace modeだけが異なる間はstale intralineを隠してsyntaxは表示する。
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
        preserve_wide_cell_styles(frame.buffer_mut());
        return;
    }
    let empty_decorations = DecorationSet::default();
    let decorations = if decorations.belongs_to(state.document()) {
        decorations
    } else {
        &empty_decorations
    };

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
        Mode::FileList(_) => render_file_list(frame, content_area, lines.file_list.as_ref(), theme),
    }
    render_status(frame, status_area, state, theme, lines);
    preserve_wide_cell_styles(frame.buffer_mut());
}

/// Wide glyphが実端末で占有するtrailing cellへlead cellのstyleを記録する。
///
/// Ratatuiのbufferはwide glyphの後半をdefaultの空白cellとして保持する一方、実端末はlead cellの
/// foreground、background、modifierをglyph幅全体へ描く。後続frameのwide glyphが1列左から旧lead
/// cellへ重なると、diff iteratorはその位置を新glyphの後半として読み飛ばす。この関数で物理的な占有
/// styleをbufferにも残しておけば、重なりから露出した旧trailing cellがstyle差分となり空白で消去される。
/// Symbolとdiff optionは変えず、通常frameでbackendへ余分なtrailing cellを書かせない。
fn preserve_wide_cell_styles(buffer: &mut Buffer) {
    let area = buffer.area;

    for y in area.y..area.bottom() {
        let mut x = area.x;
        while x < area.right() {
            let (width, style) = {
                let cell = &buffer[(x, y)];
                (cell.cell_width().max(1), cell.style())
            };
            let next = x.saturating_add(width).min(area.right());
            for trailing_x in x.saturating_add(1)..next {
                let trailing = &mut buffer[(trailing_x, y)];
                if trailing.symbol() == " " {
                    trailing.modifier = Modifier::empty();
                    trailing.set_style(style);
                }
            }
            x = next.max(x.saturating_add(1));
        }
    }
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
    let sticky_height = u16::try_from(lines.sticky_headers.len())
        .unwrap_or(u16::MAX)
        .min(area.height);
    let body_area = Rect::new(
        area.x,
        area.y.saturating_add(sticky_height),
        area.width,
        area.height.saturating_sub(sticky_height),
    );
    for (row, header) in lines.sticky_headers.iter().enumerate() {
        frame.render_widget(
            Paragraph::new(header.as_str()).style(theme.style(SemanticRole::Sticky)),
            Rect::new(area.x, area.y.saturating_add(row as u16), area.width, 1),
        );
    }

    let visible_rows = &state.visible_rows()[..state
        .visible_rows()
        .len()
        .min(usize::from(body_area.height))];
    let mut screen_row = 0_usize;
    for (row_index, row) in visible_rows.iter().enumerate() {
        for header in lines.inline_context_headers(row.line_id) {
            frame.render_widget(
                Paragraph::new(header.as_str()).style(theme.style(SemanticRole::Sticky)),
                Rect::new(
                    body_area.x,
                    body_area.y.saturating_add(screen_row as u16),
                    body_area.width,
                    1,
                ),
            );
            screen_row = screen_row.saturating_add(1);
            if screen_row >= usize::from(body_area.height) {
                break;
            }
        }
        if screen_row >= usize::from(body_area.height) {
            break;
        }
        let y = body_area.y.saturating_add(screen_row as u16);
        let weak_matches = lines.weak_matches(row.line_id);
        frame.render_widget(
            Paragraph::new(render_visual_row(
                state,
                row,
                row_index,
                weak_matches,
                theme,
                decorations,
                lines,
            )),
            Rect::new(body_area.x, y, body_area.width, 1),
        );
        screen_row = screen_row.saturating_add(1);
        if screen_row >= usize::from(body_area.height) {
            break;
        }
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
    // Visible直前4 KiBだけをlook-behindとして残す。Weak layerはcurrent matchの補助表示なので、
    // それより前から始まる巨大matchは省略してmain threadを守る。Regex自体はboundedな
    // logical line全体に対して評価する。Visual sliceでhaystackを切ると`$`、`\z`、`\b`が
    // 折り返し位置をlogical end/boundaryと誤認するため、visible end以降のmatchを見つけた時点で止める。
    let mut scan_start = visible
        .start
        .saturating_sub(MAX_WEAK_MATCH_LOOKBEHIND_BYTES);
    while scan_start > 0 && !text.is_char_boundary(scan_start) {
        scan_start -= 1;
    }
    let mut visible_end = visible.end.min(text.len());
    while visible_end > 0 && !text.is_char_boundary(visible_end) {
        visible_end -= 1;
    }
    if visible.start >= visible_end {
        return Ok(Vec::new());
    }
    let haystack = text;
    let mut cursor = scan_start.min(haystack.len());
    let mut matches = Vec::new();
    while cursor <= haystack.len() && matches.len() < *remaining {
        let Some(found) = compiled.regex().find_at(haystack, cursor) else {
            break;
        };
        if found.start() >= visible_end {
            break;
        }
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

fn sticky_file_header(file: StoredFileRef<'_>, total: Option<&str>, width: usize) -> String {
    let occurrence = file.id.get().saturating_add(1);
    let total = total.unwrap_or("?");
    let additions = file
        .additions
        .map_or_else(|| "?".to_owned(), |value| value.to_string());
    let deletions = file
        .deletions
        .map_or_else(|| "?".to_owned(), |value| value.to_string());
    let suffix = format!("  {occurrence}/{total}  +{additions} -{deletions}");
    let prefix = format!("{} ", file.status.label());
    let fixed_width = display_width(&prefix).saturating_add(display_width(&suffix));
    if fixed_width >= width {
        return fit_end(&format!("{prefix}{suffix}"), width);
    }
    let path_width = width - fixed_width;
    let path = fit_file_path(file, path_width);
    format!("{prefix}{path}{suffix}")
}

fn commit_label(commit: StoredCommitRef<'_>) -> String {
    let identity = match commit.kind {
        CommitKind::WorkingTree => "working tree".to_owned(),
        CommitKind::Commit => commit
            .object_id
            .map(short_object_id)
            .map_or_else(|| "commit".to_owned(), |object| format!("commit {object}")),
    };
    let Some(subject) = commit.subject else {
        return identity;
    };
    let subject_limit = MAX_PATH_SCAN_BYTES.saturating_sub(identity.len().saturating_add(2));
    let bounded = bounded_prefix(subject, subject_limit);
    if bounded.len() == subject.len() {
        format!("{identity}  {bounded}")
    } else {
        format!("{identity}  {bounded}…")
    }
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
    list: Option<&FileListFrame>,
    theme: &Theme,
) {
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(theme.style(SemanticRole::Plain)),
        area,
    );

    let Some(list) = list else {
        frame.render_widget(
            Paragraph::new(fit_end("(loading files…)", usize::from(area.width)))
                .style(theme.style(SemanticRole::Dim)),
            area,
        );
        return;
    };
    for (row, text) in list
        .detail_rows
        .iter()
        .take(usize::from(area.height))
        .enumerate()
    {
        frame.render_widget(
            Paragraph::new(text.as_str()).style(theme.style(SemanticRole::Sticky)),
            Rect::new(area.x, area.y.saturating_add(row as u16), area.width, 1),
        );
    }
    let detail_rows = list.detail_rows.len().min(usize::from(area.height));

    let list_y = area.y.saturating_add(detail_rows as u16);
    let list_height = area.height.saturating_sub(detail_rows as u16);
    let selected_line = list
        .overlay_rows
        .iter()
        .position(|line| line.selected)
        .unwrap_or(0);
    let available = usize::from(list_height);
    let start = selected_line
        .saturating_sub(available / 2)
        .min(list.overlay_rows.len().saturating_sub(available));
    for (screen_row, overlay) in list
        .overlay_rows
        .iter()
        .skip(start)
        .take(available)
        .enumerate()
    {
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

fn file_list_entry(file: StoredFileRef<'_>, width: usize) -> String {
    let prefix = if matches!(file.status, crate::model::ChangeStatus::Binary) {
        "  binary ".to_owned()
    } else {
        let additions = file
            .additions
            .map_or_else(|| "?".to_owned(), |value| value.to_string());
        let deletions = file
            .deletions
            .map_or_else(|| "?".to_owned(), |value| value.to_string());
        format!("  {} +{additions} -{deletions} ", file.status.label())
    };
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
        Mode::Search(search) => render_search_prompt(frame, area, state, search, theme, lines),
        Mode::FileList(list) if list.filter.is_some() => {
            if let Some(filter) = list.filter.as_ref() {
                let file_status = file_list_status(lines);
                let status = append_status(&combined_status(state), &file_status);
                let suffix = status_suffix(&status);
                render_input_prompt(frame, area, "/", filter, &suffix, theme);
            }
        }
        Mode::FileList(_) => frame.render_widget(
            Paragraph::new(fit_end(
                &append_status(
                    &combined_status(state),
                    &append_status(
                        &file_list_status(lines),
                        "Tab/Esc close  / filter  Enter jump  j/k select",
                    ),
                ),
                width,
            ))
            .style(theme.style(SemanticRole::Status)),
            area,
        ),
        Mode::Help { .. } => frame.render_widget(
            Paragraph::new(fit_end(
                &append_status(&combined_status(state), "Esc/q/h close  j/k scroll"),
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

fn file_list_status(lines: &FrameLines) -> String {
    lines.file_list.as_ref().map_or_else(String::new, |list| {
        if list.loading {
            format!("files:{} loading", list.total_matches)
        } else {
            format!("files:{}", list.total_matches)
        }
    })
}

fn render_search_prompt(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    search: &SearchInputState,
    theme: &Theme,
    lines: &FrameLines,
) {
    let prefix = match search.direction {
        crate::search::SearchDirection::Forward => "/",
        crate::search::SearchDirection::Backward => "?",
    };
    let regex_error = state
        .search()
        .incremental()
        .and_then(|query| match query.status() {
            IncrementalQueryStatus::Invalid(error) => Some(format!("regex: {error}")),
            IncrementalQueryStatus::Empty | IncrementalQueryStatus::Valid => None,
        });
    let preview_status = state
        .search()
        .incremental()
        .and_then(|query| query.displayed_preview())
        .map(|preview| {
            let skipped = (preview.skipped_oversized_lines > 0)
                .then(|| format!("skipped:{}", preview.skipped_oversized_lines));
            let current = preview
                .found
                .as_ref()
                .map(|found| search_position(lines, found));
            skipped
                .into_iter()
                .chain(current)
                .collect::<Vec<_>>()
                .join("  ")
        })
        .unwrap_or_default();
    let status = append_status(
        &combined_status(state),
        &append_status(&preview_status, search_activity_status(state)),
    );
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
    match (base.is_empty(), status.is_empty()) {
        (_, true) => base.to_owned(),
        (true, false) => status.to_owned(),
        (false, false) => format!("{base}  {status}"),
    }
}

fn search_activity_status(state: &AppState) -> &'static str {
    if state.is_search_in_flight() || state.search().pending_forward().is_some() {
        "searching"
    } else {
        ""
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
    // Unicode graphemeの境界にはcombining mark数の上限がない。入力先頭まで境界を探すと幅0の
    // queryでscan上限自体を回避できるため、左端だけscalar境界で切り、ellipsisで省略を示す。
    let left = bounded_suffix(&text[..cursor], MAX_INPUT_WINDOW_SCAN_BYTES);
    let left_base = cursor.saturating_sub(left.len());
    let mut left_cells = 0_usize;
    for (relative, grapheme) in left.grapheme_indices(true).rev() {
        let candidate = left_cells.saturating_add(display_width(grapheme));
        if candidate > target_left {
            break;
        }
        start = left_base + relative;
        left_cells = candidate;
    }
    let leading = usize::from(start > 0);
    let available = width.saturating_sub(leading);
    let mut end = start;
    let mut used = 0_usize;
    let scanned = bounded_prefix(&text[start..], MAX_INPUT_WINDOW_SCAN_BYTES);
    for (relative, grapheme) in scanned.grapheme_indices(true) {
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
    InputWindow {
        text: fit_end(&visible, width),
        cursor_cells: leading
            .saturating_add(left_cells)
            .min(width.saturating_sub(1)),
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
    let pending = (!search_activity_status(state).is_empty()).then_some("searching");
    let fallback = lines.fallback_found.then_some("fallback");
    let fallback_pending = lines.fallback_scan_pending.then_some("fallback:pending");
    let skipped = (state.search().last_skipped_oversized_lines() > 0)
        .then(|| format!("skipped:{}", state.search().last_skipped_oversized_lines()));
    let current = displayed_current_match(state).map(|found| search_position(lines, found));
    // `fit_end`は左側を残すため、操作結果とworker errorを固定情報より前へ置く。狭いterminalでも
    // unknown key、BOF/EOF、worker failureがhelp文やpositionに押し出されない。
    let mut parts = Vec::new();
    if !state.status().is_empty() {
        parts.push(state.status().to_owned());
    }
    parts.extend(state.background_status().map(str::to_owned));
    parts.extend([position, input.to_owned(), whitespace.to_owned()]);
    parts.extend(follow.map(str::to_owned));
    parts.extend(pending.map(str::to_owned));
    parts.extend(fallback.map(str::to_owned));
    parts.extend(fallback_pending.map(str::to_owned));
    parts.extend(skipped);
    parts.extend(current);
    parts.join("  ")
}

fn search_position(lines: &FrameLines, found: &SearchMatch<crate::model::LineId>) -> String {
    let column = lines
        .current_column(found.line_id)
        .unwrap_or_else(|| found.byte_range.start.saturating_add(1));
    format!("match {}:{}", found.line_index + 1, column)
}

fn fit_file_path(file: StoredFileRef<'_>, width: usize) -> String {
    fit_file_path_parts(file.old_path, file.new_path, width)
}

fn fit_file_path_parts(old_path: Option<&str>, new_path: Option<&str>, width: usize) -> String {
    match (old_path, new_path) {
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
    use ratatui::buffer::Buffer;
    use ratatui::style::{Color, Style};

    use super::*;
    use crate::cli::ThemeChoice;
    use crate::decorations::DecorationSet;
    use crate::input::AppendStore;
    use crate::parser::parse_bytes;
    use crate::search::compile_smart_case;
    use crate::state::{Effect, Event, FileFilterGeneration, FileFilterWindow};
    use crate::storage::{StorageConfig, StructuralStorage};
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
    fn wide_glyph_shift_exposes_previous_trailing_background_to_buffer_diff() {
        let area = Rect::new(0, 0, 4, 1);
        let highlighted = Style::new()
            .bg(Color::Green)
            .add_modifier(Modifier::REVERSED);
        let mut previous = Buffer::empty(area);
        previous.set_string(1, 0, "界", highlighted);
        preserve_wide_cell_styles(&mut previous);
        assert_eq!(previous[(1, 0)].style(), previous[(2, 0)].style());

        let mut next = Buffer::empty(area);
        next.set_string(0, 0, "界", highlighted);
        preserve_wide_cell_styles(&mut next);

        let updates = previous.diff(&next);
        assert!(
            updates.iter().any(|(x, y, cell)| {
                *x == 2 && *y == 0 && cell.symbol() == " " && cell.bg == Color::Reset
            }),
            "1列左へ移った全角文字の旧末尾cellが消去対象に含まれていない: {updates:?}"
        );
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

    fn load_frame(state: &AppState) -> FrameLines {
        FrameLoader::new().load(state).unwrap()
    }

    fn parse_with_storage(
        input: &[u8],
        storage: &StructuralStorage,
    ) -> crate::document::DocumentSnapshot {
        let raw = AppendStore::new();
        let mut parser = crate::parser::GitParser::new(raw.clone(), storage).unwrap();
        let raw_start = raw.append(input).unwrap();
        parser.push(raw_start, input).unwrap();
        raw.seal().unwrap();
        parser.into_document().unwrap()
    }

    fn append_rich_file(input: &mut String, index: usize) {
        let path = format!("src/file-{index:05}.rs");
        input.push_str(&format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-old\n+new\n"
        ));
    }

    fn append_empty_commit(input: &mut String, index: usize, subject: &str) {
        input.push_str(&format!(
            "commit {index:08x}00000000000000000000000000000000\n\n    {subject}\n"
        ));
    }

    fn file_list_state(
        selected_item: Option<FileListItem>,
        window: Option<FileFilterWindow>,
        loading: bool,
    ) -> FileListState {
        FileListState {
            selected_item,
            selected_file: selected_item.and_then(FileListItem::file_id),
            window,
            filter: None,
            generation: FileFilterGeneration::default(),
            loading,
        }
    }

    #[test]
    fn body_renders_sticky_file_and_dual_gutter() {
        let document = parse_bytes(
            b"diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -10 +10 @@\n-old\n+new\n",
        )
        .unwrap();
        let state =
            AppState::new(Arc::new(document), 60, 9, 4, WhitespaceChoice::Ignore, true).unwrap();
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        let decorations =
            DecorationSet::compute(state.document(), &theme, WhitespaceChoice::Ignore).unwrap();
        let lines = load_frame(&state);
        let mut terminal = Terminal::new(TestBackend::new(60, 9)).unwrap();
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
    fn body_inserts_file_header_before_each_file_section() {
        let document = parse_bytes(
            b"diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-old\n+new\ndiff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+second-new\n",
        )
        .unwrap();
        let mut state = AppState::new(
            Arc::new(document),
            80,
            12,
            4,
            WhitespaceChoice::Ignore,
            true,
        )
        .unwrap();
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        let decorations =
            DecorationSet::compute(state.document(), &theme, WhitespaceChoice::Ignore).unwrap();
        let lines = load_frame(&state);
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| render(frame, &state, &theme, &decorations, &lines))
            .unwrap();
        let screen = buffer_text(&terminal);
        let rows = screen.lines().collect::<Vec<_>>();

        assert!(rows[0].contains("working tree"), "{screen}");
        assert!(rows[1].contains("src/main.rs  1/2"), "{screen}");
        assert!(rows[8].contains("src/lib.rs  2/2"), "{screen}");
        assert!(
            rows[9].contains("diff --git a/src/lib.rs b/src/lib.rs"),
            "{screen}"
        );

        state
            .update(Event::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)))
            .unwrap();
        let lines = load_frame(&state);
        terminal
            .draw(|frame| render(frame, &state, &theme, &decorations, &lines))
            .unwrap();
        let screen = buffer_text(&terminal);
        assert!(screen.contains("second-new"), "{screen}");
    }

    #[test]
    fn body_stacks_commit_above_file_context() {
        let document = parse_bytes(
            br#"commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa

    First change

diff --git a/first.rs b/first.rs
--- a/first.rs
+++ b/first.rs
@@ -1 +1 @@
-old
+first
commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb

    Second change

diff --git a/second.rs b/second.rs
--- a/second.rs
+++ b/second.rs
@@ -1 +1 @@
-old
+second
"#,
        )
        .unwrap();
        let mut state = AppState::new(
            Arc::new(document),
            100,
            40,
            4,
            WhitespaceChoice::Ignore,
            true,
        )
        .unwrap();
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        let decorations =
            DecorationSet::compute(state.document(), &theme, WhitespaceChoice::Ignore).unwrap();

        let lines = load_frame(&state);
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        terminal
            .draw(|frame| render(frame, &state, &theme, &decorations, &lines))
            .unwrap();
        let screen = buffer_text(&terminal);
        let rows = screen.lines().map(str::trim_end).collect::<Vec<_>>();
        let second_commit_header = rows
            .iter()
            .position(|row| *row == "commit bbbbbbbb  Second change")
            .expect("inline commit header");
        let second_commit_source = rows
            .iter()
            .position(|row| *row == "commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
            .expect("source commit header");
        let second_file_header = rows
            .iter()
            .position(|row| row.contains("second.rs  2/2"))
            .expect("inline file header");
        let second_file_source = rows
            .iter()
            .position(|row| *row == "diff --git a/second.rs b/second.rs")
            .expect("source file header");
        assert_eq!(second_commit_header + 1, second_commit_source, "{screen}");
        assert_eq!(second_file_header + 1, second_file_source, "{screen}");

        state
            .update(Event::Resize {
                width: 100,
                height: 8,
            })
            .unwrap();
        state
            .update(Event::Key(KeyEvent::new(
                KeyCode::Char('}'),
                KeyModifiers::NONE,
            )))
            .unwrap();
        let lines = load_frame(&state);
        let mut terminal = Terminal::new(TestBackend::new(100, 8)).unwrap();
        terminal
            .draw(|frame| render(frame, &state, &theme, &decorations, &lines))
            .unwrap();
        let screen = buffer_text(&terminal);
        let rows = screen.lines().map(str::trim_end).collect::<Vec<_>>();
        assert_eq!(rows[0], "commit aaaaaaaa  First change", "{screen}");
        assert!(rows[1].contains("first.rs  1/2"), "{screen}");
        assert_eq!(rows[2], "diff --git a/first.rs b/first.rs", "{screen}");
    }

    #[test]
    fn narrow_overlay_status_keeps_transient_message_before_fixed_help() {
        let document = parse_bytes(b"plain\n").unwrap();
        let mut state =
            AppState::new(Arc::new(document), 30, 6, 4, WhitespaceChoice::Ignore, true).unwrap();
        state
            .update(Event::Key(KeyEvent::new(
                KeyCode::Char('h'),
                KeyModifiers::NONE,
            )))
            .unwrap();
        state
            .update(Event::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            )))
            .unwrap();
        let lines = load_frame(&state);
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        let mut terminal = Terminal::new(TestBackend::new(30, 6)).unwrap();
        terminal
            .draw(|frame| {
                render(frame, &state, &theme, &DecorationSet::default(), &lines);
            })
            .unwrap();
        let screen = buffer_text(&terminal);

        assert!(screen.contains("unknown key"), "{screen}");
    }

    #[test]
    fn search_status_tracks_current_worker_without_repeating_regex_errors() {
        let document = parse_bytes(b"plain text\n").unwrap();
        let mut state = AppState::new(
            Arc::new(document),
            120,
            8,
            4,
            WhitespaceChoice::Ignore,
            true,
        )
        .unwrap();
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        state
            .update(Event::Key(KeyEvent::new(
                KeyCode::Char('/'),
                KeyModifiers::NONE,
            )))
            .unwrap();
        let effects = state.update(Event::Paste("plain".to_owned())).unwrap();
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Search(_)))
        );
        assert!(state.is_search_in_flight());

        let lines = load_frame(&state);
        let mut terminal = Terminal::new(TestBackend::new(120, 8)).unwrap();
        terminal
            .draw(|frame| {
                render(frame, &state, &theme, &DecorationSet::default(), &lines);
            })
            .unwrap();
        let searching = buffer_text(&terminal);
        assert!(searching.contains("searching"), "{searching}");

        state.update(Event::Paste("[".to_owned())).unwrap();
        assert!(!state.is_search_in_flight());
        assert!(state.status().is_empty());
        let lines = load_frame(&state);
        terminal
            .draw(|frame| {
                render(frame, &state, &theme, &DecorationSet::default(), &lines);
            })
            .unwrap();
        let invalid = buffer_text(&terminal);
        assert_eq!(invalid.matches("regex:").count(), 1, "{invalid}");
        assert!(!invalid.contains("searching"), "{invalid}");
    }

    #[test]
    fn committed_search_worker_is_visible_in_body_status() {
        let document = parse_bytes(b"plain text\n").unwrap();
        let mut state =
            AppState::new(Arc::new(document), 80, 8, 4, WhitespaceChoice::Ignore, true).unwrap();
        state
            .update(Event::Key(KeyEvent::new(
                KeyCode::Char('/'),
                KeyModifiers::NONE,
            )))
            .unwrap();
        state.update(Event::Paste("missing".to_owned())).unwrap();
        let effects = state
            .update(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .unwrap();
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Search(_)))
        );
        assert!(matches!(state.mode(), Mode::Body));
        assert!(state.is_search_in_flight());
        assert!(body_status(&state, &load_frame(&state)).contains("searching"));
    }

    #[test]
    fn sticky_header_formats_rename_occurrence_and_multi_commit_identity() {
        let document = parse_bytes(
            br#"commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa

    Rename source

diff --git a/old-name.rs b/new-name.rs
similarity index 100%
rename from old-name.rs
rename to new-name.rs
commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb

    Change more files

diff --git a/second.rs b/second.rs
--- a/second.rs
+++ b/second.rs
@@ -1 +1 @@
-old
+new
diff --git a/third.rs b/third.rs
--- a/third.rs
+++ b/third.rs
@@ -1 +1 @@
-old
+new
"#,
        )
        .unwrap();
        let mut state = AppState::new(
            Arc::new(document),
            100,
            8,
            4,
            WhitespaceChoice::Ignore,
            true,
        )
        .unwrap();
        state
            .update(Event::Key(KeyEvent::new(
                KeyCode::Char('}'),
                KeyModifiers::NONE,
            )))
            .unwrap();
        assert_eq!(
            state.current_row().and_then(|row| row.file_id),
            Some(FileId::new(0))
        );

        let lines = load_frame(&state);
        assert_eq!(lines.sticky_headers.len(), 2);
        assert!(lines.sticky_headers[0].contains("commit aaaaaaaa"));
        assert!(lines.sticky_headers[1].contains("old-name.rs → new-name.rs"));
        assert!(lines.sticky_headers[1].contains("1/3"));
        assert!(!lines.sticky_headers[1].contains("@aaaaaaaa"));
    }

    #[test]
    fn file_list_preload_keeps_selected_detail_git_order_and_commit_headers() {
        let document = parse_bytes(
            br#"commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
diff --git a/old.rs b/new.rs
similarity index 100%
rename from old.rs
rename to new.rs
diff --git a/second.rs b/second.rs
--- a/second.rs
+++ b/second.rs
commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
    no file changes here
commit cccccccccccccccccccccccccccccccccccccccc
diff --git a/third.rs b/third.rs
--- a/third.rs
+++ b/third.rs
"#,
        )
        .unwrap();
        let state =
            AppState::new(Arc::new(document), 80, 9, 4, WhitespaceChoice::Ignore, true).unwrap();
        let window = FileFilterWindow {
            total_matches: 3,
            selected_rank: Some(0),
            start_rank: 0,
            file_ids: vec![FileId::new(0), FileId::new(1), FileId::new(2)],
        };
        let list = file_list_state(
            Some(FileListItem::File(FileId::new(0))),
            Some(window),
            false,
        );
        let mut loader = FrameLoader::new();
        let lines = loader.load_file_list(&state, &list).unwrap();
        let frame = lines.file_list.as_ref().unwrap();
        let detail = frame.detail_rows.join("");
        let file_rows = frame
            .overlay_rows
            .iter()
            .filter(|row| row.role == SemanticRole::Plain)
            .collect::<Vec<_>>();

        assert!(detail.contains("old.rs → new.rs"), "{detail}");
        assert_eq!(frame.total_matches, 3);
        assert!(!frame.loading);
        assert_eq!(file_rows.len(), 3);
        assert!(file_rows[0].selected);
        assert!(file_rows[0].text.contains("old.rs"));
        assert!(file_rows[1].text.contains("second.rs"));
        assert!(file_rows[2].text.contains("third.rs"));
        assert_eq!(
            frame
                .overlay_rows
                .iter()
                .filter(|row| row.role == SemanticRole::CommitHeader)
                .count(),
            3
        );
        assert!(
            frame
                .overlay_rows
                .iter()
                .any(|row| row.text.contains("(no file changes)"))
        );
    }

    #[test]
    fn nonempty_file_filter_hides_empty_commit_items() {
        let document = parse_bytes(
            br#"commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
diff --git a/first.rs b/first.rs
--- a/first.rs
+++ b/first.rs
commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
    no file changes
commit cccccccccccccccccccccccccccccccccccccccc
diff --git a/last.rs b/last.rs
--- a/last.rs
+++ b/last.rs
"#,
        )
        .unwrap();
        let state =
            AppState::new(Arc::new(document), 80, 9, 4, WhitespaceChoice::Ignore, true).unwrap();
        let window = FileFilterWindow {
            total_matches: 2,
            selected_rank: Some(0),
            start_rank: 0,
            file_ids: vec![FileId::new(0), FileId::new(1)],
        };
        let mut list = file_list_state(
            Some(FileListItem::File(FileId::new(0))),
            Some(window),
            false,
        );
        let mut filter = TextInput::default();
        filter.set("rs");
        list.filter = Some(filter);
        let frame = FrameLoader::new().load_file_list(&state, &list).unwrap();
        let rows = &frame.file_list.as_ref().unwrap().overlay_rows;

        assert!(rows.iter().any(|row| row.text.contains("first.rs")));
        assert!(rows.iter().any(|row| row.text.contains("last.rs")));
        assert!(!rows.iter().any(|row| row.text.contains("no file changes")));
    }

    #[test]
    fn file_list_shows_a_commit_when_the_document_has_no_files() {
        let document =
            parse_bytes(b"commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n    empty commit\n")
                .unwrap();
        let state =
            AppState::new(Arc::new(document), 80, 7, 4, WhitespaceChoice::Ignore, true).unwrap();
        let window = FileFilterWindow {
            total_matches: 0,
            selected_rank: None,
            start_rank: 0,
            file_ids: Vec::new(),
        };
        let list = file_list_state(None, Some(window), false);
        let lines = FrameLoader::new().load_file_list(&state, &list).unwrap();
        let rows = &lines.file_list.as_ref().unwrap().overlay_rows;

        assert!(rows.iter().any(|row| row.text.contains("commit aaaaaaaa")));
        assert!(
            rows.iter()
                .any(|row| row.text.contains("(no file changes)"))
        );
        assert!(
            !rows
                .iter()
                .any(|row| row.text.contains("no matching files"))
        );
    }

    #[test]
    fn distant_empty_commit_selection_builds_only_a_local_navigable_window() {
        let mut input = String::new();
        for index in 0..30 {
            append_empty_commit(&mut input, index, &format!("empty {index}"));
        }
        let document = parse_bytes(input.as_bytes()).unwrap();
        let state = AppState::new(
            Arc::new(document),
            80,
            10,
            4,
            WhitespaceChoice::Ignore,
            true,
        )
        .unwrap();
        let selected = FileListItem::EmptyCommit(CommitId::new(20));
        let list = file_list_state(
            Some(selected),
            Some(FileFilterWindow {
                total_matches: 0,
                selected_rank: None,
                start_rank: 0,
                file_ids: Vec::new(),
            }),
            false,
        );
        let frame = FrameLoader::new().load_file_list(&state, &list).unwrap();
        let frame = frame.file_list.as_ref().unwrap();

        assert!(
            frame.detail_rows.join("").contains("commit 00000014"),
            "{:?}",
            frame.detail_rows
        );
        assert!(frame.overlay_rows.len() <= 8);
        assert_eq!(
            frame.overlay_rows.iter().filter(|row| row.selected).count(),
            1
        );
        assert!(
            frame
                .overlay_rows
                .iter()
                .any(|row| row.text.contains("commit 00000014"))
        );
        assert!(
            !frame
                .overlay_rows
                .iter()
                .any(|row| row.text.contains("commit 00000000"))
        );
    }

    #[test]
    fn empty_commit_selection_inside_a_long_gap_does_not_scan_from_a_file_edge() {
        let mut input =
            String::from("commit ffffffff00000000000000000000000000000000\n\n    first file\n\n");
        append_rich_file(&mut input, 0);
        for index in 1..26 {
            append_empty_commit(&mut input, index, &format!("gap {index}"));
        }
        input.push_str("commit eeeeeeee00000000000000000000000000000000\n\n    last file\n\n");
        append_rich_file(&mut input, 1);

        let document = parse_bytes(input.as_bytes()).unwrap();
        let state =
            AppState::new(Arc::new(document), 72, 9, 4, WhitespaceChoice::Ignore, true).unwrap();
        let selected = FileListItem::EmptyCommit(CommitId::new(13));
        let list = file_list_state(selected.into(), None, false);
        let frame = FrameLoader::new().load_file_list(&state, &list).unwrap();
        let frame = frame.file_list.as_ref().unwrap();

        assert!(frame.detail_rows.join("").contains("commit 0000000d"));
        assert!(
            frame
                .overlay_rows
                .iter()
                .any(|row| row.selected && row.text.contains("no file changes"))
        );
        assert!(
            !frame
                .overlay_rows
                .iter()
                .any(|row| row.text.contains("file-00000.rs") || row.text.contains("file-00001.rs"))
        );
    }

    #[test]
    fn open_zero_file_commit_stays_loading_until_eof_finalizes_its_placeholder() {
        let raw = AppendStore::new();
        let storage = StructuralStorage::new().unwrap();
        let mut parser = crate::parser::GitParser::new(raw.clone(), &storage).unwrap();
        let input = b"commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\n    still open\n";
        let raw_start = raw.append(input).unwrap();
        parser.push(raw_start, input).unwrap();

        let open = parser.snapshot().unwrap();
        let open_state =
            AppState::new(Arc::new(open), 70, 8, 4, WhitespaceChoice::Ignore, false).unwrap();
        let open_frame = FrameLoader::new()
            .load_file_list(&open_state, &file_list_state(None, None, false))
            .unwrap();
        let open_frame = open_frame.file_list.as_ref().unwrap();
        assert!(open_frame.loading);
        assert!(
            open_frame
                .overlay_rows
                .iter()
                .any(|row| row.text.contains("loading files"))
        );
        assert!(
            !open_frame
                .overlay_rows
                .iter()
                .any(|row| row.text.contains("no file changes"))
        );

        raw.seal().unwrap();
        parser.finish().unwrap();
        let complete = parser.snapshot().unwrap();
        let complete_state =
            AppState::new(Arc::new(complete), 70, 8, 4, WhitespaceChoice::Ignore, true).unwrap();
        let complete_frame = FrameLoader::new()
            .load_file_list(&complete_state, &file_list_state(None, None, false))
            .unwrap();
        let complete_frame = complete_frame.file_list.as_ref().unwrap();
        assert!(!complete_frame.loading);
        assert!(
            complete_frame
                .overlay_rows
                .iter()
                .any(|row| row.selected && row.text.contains("no file changes"))
        );
    }

    #[test]
    fn trailing_open_zero_commit_keeps_prior_file_window_in_loading_state() {
        let raw = AppendStore::new();
        let storage = StructuralStorage::new().unwrap();
        let mut parser = crate::parser::GitParser::new(raw.clone(), &storage).unwrap();
        let input = br#"commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
diff --git a/ready.rs b/ready.rs
--- a/ready.rs
+++ b/ready.rs
commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
    still open
"#;
        let raw_start = raw.append(input).unwrap();
        parser.push(raw_start, input).unwrap();

        let document = parser.snapshot().unwrap();
        let state = AppState::new(
            Arc::new(document),
            70,
            8,
            4,
            WhitespaceChoice::Ignore,
            false,
        )
        .unwrap();
        let list = file_list_state(
            Some(FileListItem::File(FileId::new(0))),
            Some(FileFilterWindow {
                total_matches: 1,
                selected_rank: Some(0),
                start_rank: 0,
                file_ids: vec![FileId::new(0)],
            }),
            false,
        );
        let frame = FrameLoader::new().load_file_list(&state, &list).unwrap();
        let frame = frame.file_list.as_ref().unwrap();

        assert!(frame.loading);
        assert!(
            frame
                .overlay_rows
                .iter()
                .any(|row| row.text.contains("ready.rs"))
        );
        assert!(
            !frame
                .overlay_rows
                .iter()
                .any(|row| row.text.contains("no file changes"))
        );
    }

    #[test]
    fn selected_empty_placeholder_survives_large_subject_frame_clipping() {
        let mut input = String::new();
        let subject = "s".repeat(MAX_PATH_SCAN_BYTES);
        for index in 0..80 {
            append_empty_commit(&mut input, index, &subject);
        }
        let document = parse_bytes(input.as_bytes()).unwrap();
        let state = AppState::new(
            Arc::new(document),
            u16::MAX,
            100,
            4,
            WhitespaceChoice::Ignore,
            true,
        )
        .unwrap();
        let selected = FileListItem::EmptyCommit(CommitId::new(40));
        let list = file_list_state(selected.into(), None, false);
        let frame = FrameLoader::new().load_file_list(&state, &list).unwrap();

        assert!(frame.retained_bytes() <= MAX_FRAME_RETAINED_BYTES);
        let file_list = frame.file_list.as_ref().unwrap();
        assert!(file_list.detail_rows.join("").contains("commit 00000028"));
        assert!(
            file_list
                .overlay_rows
                .iter()
                .any(|row| row.selected && row.text.contains("no file changes"))
        );
    }

    #[test]
    fn binary_file_list_entry_does_not_repeat_the_status_label() {
        let document = parse_bytes(
            b"diff --git a/image.bin b/image.bin\nBinary files a/image.bin and b/image.bin differ\n",
        )
        .unwrap();
        let state =
            AppState::new(Arc::new(document), 80, 7, 4, WhitespaceChoice::Ignore, true).unwrap();
        let window = FileFilterWindow {
            total_matches: 1,
            selected_rank: Some(0),
            start_rank: 0,
            file_ids: vec![FileId::new(0)],
        };
        let list = file_list_state(
            Some(FileListItem::File(FileId::new(0))),
            Some(window),
            false,
        );
        let lines = FrameLoader::new().load_file_list(&state, &list).unwrap();
        let row = lines
            .file_list
            .as_ref()
            .unwrap()
            .overlay_rows
            .iter()
            .find(|row| row.role == SemanticRole::Plain)
            .unwrap();
        assert!(row.text.contains("binary image.bin"), "{}", row.text);
        assert!(!row.text.contains("binary binary"), "{}", row.text);
    }

    #[test]
    fn file_list_preload_distinguishes_loading_and_no_matches() {
        let document = parse_bytes(b"plain\n").unwrap();
        let state =
            AppState::new(Arc::new(document), 40, 7, 4, WhitespaceChoice::Ignore, true).unwrap();
        let mut loader = FrameLoader::new();

        let loading = loader
            .load_file_list(&state, &file_list_state(None, None, true))
            .unwrap();
        let loading = loading.file_list.as_ref().unwrap();
        assert!(loading.loading);
        assert_eq!(loading.total_matches, 0);
        assert!(loading.overlay_rows[0].text.contains("loading files"));

        let empty_window = FileFilterWindow {
            total_matches: 0,
            selected_rank: None,
            start_rank: 0,
            file_ids: Vec::new(),
        };
        let empty = loader
            .load_file_list(&state, &file_list_state(None, Some(empty_window), false))
            .unwrap();
        let empty = empty.file_list.as_ref().unwrap();
        assert!(!empty.loading);
        assert!(empty.overlay_rows[0].text.contains("no matching files"));
    }

    #[test]
    fn show_whitespace_renders_changed_tab_cells() {
        let document = parse_bytes(
            b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-\tfoo\n+    foo\n",
        )
        .unwrap();
        let state =
            AppState::new(Arc::new(document), 60, 9, 4, WhitespaceChoice::Show, true).unwrap();
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        let decorations =
            DecorationSet::compute(state.document(), &theme, WhitespaceChoice::Show).unwrap();
        let lines = load_frame(&state);
        let mut terminal = Terminal::new(TestBackend::new(60, 9)).unwrap();
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
        let lines = load_frame(&state);
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
        let lines = load_frame(&state);
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
        let lines = load_frame(&state);
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
        let lines = load_frame(&state);
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
    fn tall_terminal_frame_stays_within_viewport_and_frame_bounds() {
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

        // Viewportの64 MiB cache budgetが先にrow数を抑える。FrameLoaderはそのreachable setだけを
        // preloadし、独立した5,000件/4 MiB境界内へ収める。
        assert!(state.visible_rows().len() < MAX_FRAME_LINE_ENTRIES + 1);
        let frame = FrameLoader::new().load(&state).unwrap();
        assert!(frame.lines.len() <= MAX_FRAME_LINE_ENTRIES);
        assert!(frame.retained_bytes() <= MAX_FRAME_RETAINED_BYTES);
    }

    #[test]
    fn frame_retained_bytes_do_not_scale_with_visible_line_length() {
        fn retained_for(length: usize) -> usize {
            let mut input = "x".repeat(length);
            input.push('\n');
            let document = parse_bytes(input.as_bytes()).unwrap();
            let state = AppState::new(Arc::new(document), 40, 8, 4, WhitespaceChoice::Ignore, true)
                .unwrap();
            load_frame(&state).retained_bytes()
        }

        let short = retained_for(8);
        let streamed = retained_for(200_000);
        assert_eq!(short, streamed);
        assert!(streamed <= MAX_FRAME_RETAINED_BYTES);
    }

    #[test]
    fn oversized_file_window_preloads_only_screen_rows_around_selection() {
        let mut input = String::new();
        for index in 0..5_001 {
            append_rich_file(&mut input, index);
        }
        let document = parse_bytes(input.as_bytes()).unwrap();
        let state = AppState::new(
            Arc::new(document),
            80,
            12,
            4,
            WhitespaceChoice::Ignore,
            true,
        )
        .unwrap();
        let selected_rank = 2_500_u64;
        let window = FileFilterWindow {
            total_matches: 5_001,
            selected_rank: Some(selected_rank),
            start_rank: 0,
            file_ids: (0..5_001).map(FileId::new).collect(),
        };
        let list = file_list_state(
            Some(FileListItem::File(FileId::new(selected_rank))),
            Some(window),
            false,
        );
        let frame = FrameLoader::new().load_file_list(&state, &list).unwrap();
        let frame = frame.file_list.as_ref().unwrap();
        let file_rows = frame
            .overlay_rows
            .iter()
            .filter(|row| row.role == SemanticRole::Plain)
            .collect::<Vec<_>>();

        assert_eq!(file_rows.len(), 9);
        assert!(frame.overlay_rows[0].role == SemanticRole::CommitHeader);
        assert!(file_rows.iter().any(|row| row.selected));
        assert!(file_rows[0].text.contains("file-02496.rs"));
        assert!(file_rows[8].text.contains("file-02504.rs"));
        assert_eq!(frame.total_matches, 5_001);
    }

    #[test]
    fn file_list_rows_stop_before_the_shared_frame_budget() {
        let mut file_list = FileListFrame::default();
        let payload = "x".repeat(MAX_PATH_SCAN_BYTES);
        let mut accepted = 0_usize;
        for _ in 0..MAX_FRAME_LINE_ENTRIES {
            if !push_file_list_row(
                &mut file_list,
                FileListRow {
                    text: payload.clone(),
                    role: SemanticRole::Plain,
                    selected: false,
                },
            ) {
                break;
            }
            accepted = accepted.saturating_add(1);
        }
        assert!(accepted < MAX_FRAME_LINE_ENTRIES);

        let frame = finish_file_list_frame(file_list).unwrap();
        assert!(frame.retained_bytes() <= MAX_FRAME_RETAINED_BYTES);
    }

    #[test]
    fn fallback_scan_advances_by_4096_files_and_resets_for_a_new_document() {
        let mut input = String::new();
        for index in 0..4_096 {
            append_rich_file(&mut input, index);
        }
        let path = "src/fallback.rs";
        input.push_str(&format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ malformed @@\n-old\n+new\n"
        ));
        let document = parse_bytes(input.as_bytes()).unwrap();
        let state =
            AppState::new(Arc::new(document), 60, 7, 4, WhitespaceChoice::Ignore, true).unwrap();
        let mut loader = FrameLoader::new();

        let first = loader.load(&state).unwrap();
        assert!(!first.fallback_found);
        assert!(first.fallback_scan_pending);
        assert!(first.needs_follow_up_frame());
        assert_eq!(loader.fallback_cursor, 4_096);

        let second = loader.load(&state).unwrap();
        assert!(second.fallback_found);
        assert!(!second.fallback_scan_pending);
        assert!(!second.needs_follow_up_frame());

        let replacement = parse_bytes(
            b"diff --git a/rich.rs b/rich.rs\n--- a/rich.rs\n+++ b/rich.rs\n@@ -1 +1 @@\n-old\n+new\n",
        )
        .unwrap();
        let replacement = AppState::new(
            Arc::new(replacement),
            60,
            7,
            4,
            WhitespaceChoice::Ignore,
            true,
        )
        .unwrap();
        let reset = loader.load(&replacement).unwrap();
        assert!(!reset.fallback_found);
        assert!(!reset.fallback_scan_pending);
        assert_eq!(loader.fallback_cursor, 1);
    }

    #[test]
    fn offscreen_current_match_uses_byte_column_without_preloading_its_line() {
        let mut input = (0..20)
            .map(|index| format!("plain-{index:02}\n"))
            .collect::<String>();
        input.push_str("tail\n");
        let document = parse_bytes(input.as_bytes()).unwrap();
        let mut state =
            AppState::new(Arc::new(document), 40, 6, 4, WhitespaceChoice::Ignore, true).unwrap();
        apply_search_preview(&mut state, "plain-15");
        state
            .update(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .unwrap();
        let current = state.search().current_match().cloned().unwrap();
        assert_eq!(current.byte_range.start, 0);
        let _ = state
            .update(Event::Key(KeyEvent::new(
                KeyCode::Char('g'),
                KeyModifiers::NONE,
            )))
            .unwrap();
        assert!(
            !state
                .visible_rows()
                .iter()
                .any(|row| row.line_id == current.line_id)
        );

        let frame = FrameLoader::new().load(&state).unwrap();
        assert!(!frame.lines.contains_key(&current.line_id));
        assert_eq!(frame.current_column(current.line_id), Some(1));
    }

    #[test]
    fn exhausted_weak_match_cap_stops_later_visible_line_decodes() {
        let input = format!(
            "{}\n{}\n{}\n",
            "a".repeat(10_000),
            "b".repeat(10_000),
            "c".repeat(10_000)
        );
        let document = parse_bytes(input.as_bytes()).unwrap();
        let mut state = AppState::new(
            Arc::new(document),
            20_000,
            5,
            4,
            WhitespaceChoice::Ignore,
            true,
        )
        .unwrap();
        apply_search_preview(&mut state, ".");
        let frame = FrameLoader::new().load(&state).unwrap();
        let ids = state
            .visible_rows()
            .iter()
            .map(|row| row.line_id)
            .collect::<Vec<_>>();

        assert_eq!(ids.len(), 3);
        assert_eq!(frame.weak_matches(ids[0]).len(), 10_000);
        assert_eq!(frame.weak_matches(ids[1]).len(), 6_384);
        assert!(frame.weak_matches(ids[2]).is_empty());
        assert!(frame.transient_raw_bytes_read() < 25_000);
    }

    #[test]
    fn file_list_preload_reads_paths_after_structure_and_blob_spill() {
        let limit = 512 * 1024;
        let config = StorageConfig::new(limit, 480 * 1024, 256, 4 * 1024).unwrap();
        let storage = StructuralStorage::with_config(config).unwrap();
        let mut input = "commit 1111111111111111111111111111111111111111\n".to_owned();
        for index in 0..800 {
            let path = format!("spill/{index:04}/component/a-long-source-file-name.rs");
            input.push_str(&format!(
                "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n"
            ));
        }
        let document = parse_with_storage(input.as_bytes(), &storage);
        let state =
            AppState::new(Arc::new(document), 90, 8, 4, WhitespaceChoice::Ignore, true).unwrap();
        let selected = FileId::new(799);
        let window = FileFilterWindow {
            total_matches: 800,
            selected_rank: Some(799),
            start_rank: 799,
            file_ids: vec![selected],
        };
        let list = file_list_state(Some(FileListItem::File(selected)), Some(window), false);
        let frame = FrameLoader::new().load_file_list(&state, &list).unwrap();
        let frame = frame.file_list.as_ref().unwrap();

        assert!(
            frame.detail_rows.join("").contains("spill/0799/component"),
            "{:?}",
            frame.detail_rows
        );
        assert!(
            frame
                .overlay_rows
                .iter()
                .any(|row| row.text.contains("spill/0799/component"))
        );
        assert!(storage.resident_bytes().unwrap().total <= limit);
    }

    #[test]
    fn render_does_not_advance_preload_structure_readers() {
        let document = parse_bytes(
            b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n",
        )
        .unwrap();
        let state =
            AppState::new(Arc::new(document), 60, 8, 4, WhitespaceChoice::Ignore, true).unwrap();
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        let mut loader = FrameLoader::new();
        let lines = loader.load(&state).unwrap();
        let before = loader.fallback_reader.statistics();
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();

        terminal
            .draw(|frame| {
                render(frame, &state, &theme, &DecorationSet::default(), &lines);
            })
            .unwrap();

        assert_eq!(loader.fallback_reader.statistics(), before);
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
            vec![0..1_000]
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
    fn weak_matches_preserve_logical_line_anchor_and_word_boundary_semantics() {
        for pattern in [r"foo$", r"foo\z", r"foo\b"] {
            let compiled = compile_smart_case(pattern).unwrap();
            let mut remaining = 3;
            assert_eq!(
                weak_matches_in_visible_range(&compiled, "foobar", 0..3, &mut remaining).unwrap(),
                Vec::<Range<usize>>::new(),
                "pattern {pattern:?} must not treat a visual slice end as a logical boundary"
            );
            assert_eq!(remaining, 3);

            let mut remaining = 3;
            assert_eq!(
                weak_matches_in_visible_range(&compiled, "foo", 0..3, &mut remaining).unwrap(),
                vec![0..3],
                "pattern {pattern:?} must still match at the logical line end"
            );
            assert_eq!(remaining, 2);
        }

        let word_boundary = compile_smart_case(r"foo\b").unwrap();
        let mut remaining = 3;
        assert_eq!(
            weak_matches_in_visible_range(&word_boundary, "foo bar", 0..3, &mut remaining).unwrap(),
            vec![0..3]
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
        let lines = load_frame(&state);
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
    fn stale_same_lineage_decorations_are_ignored_after_snapshot_fallback() {
        let raw = AppendStore::new();
        let storage = StructuralStorage::new().unwrap();
        let mut parser = crate::parser::GitParser::new(raw.clone(), &storage).unwrap();
        let input = b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n-let old = 1;\n+let new = 2;\n";
        let start = raw.append(input).unwrap();
        parser.push(start, input).unwrap();
        let rich = parser.snapshot().unwrap();
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::TrueColor);
        let stale = DecorationSet::compute(&rich, &theme, WhitespaceChoice::Ignore).unwrap();
        assert!(stale.belongs_to(&rich));

        raw.seal().unwrap();
        let fallback = parser.into_document().unwrap();
        assert_eq!(rich.identity(), fallback.identity());
        assert!(!stale.belongs_to(&fallback));
        let state =
            AppState::new(Arc::new(fallback), 60, 9, 4, WhitespaceChoice::Ignore, true).unwrap();
        let lines = load_frame(&state);
        let mut stale_terminal = Terminal::new(TestBackend::new(60, 9)).unwrap();
        stale_terminal
            .draw(|frame| render(frame, &state, &theme, &stale, &lines))
            .unwrap();
        let mut plain_terminal = Terminal::new(TestBackend::new(60, 9)).unwrap();
        plain_terminal
            .draw(|frame| {
                render(frame, &state, &theme, &DecorationSet::default(), &lines);
            })
            .unwrap();

        assert_eq!(
            stale_terminal.backend().buffer(),
            plain_terminal.backend().buffer()
        );
    }

    #[test]
    fn zero_width_match_on_empty_plain_line_gets_a_visible_marker() {
        let document = parse_bytes(b"\n").unwrap();
        let mut state =
            AppState::new(Arc::new(document), 20, 6, 4, WhitespaceChoice::Ignore, true).unwrap();
        apply_search_preview(&mut state, "^$");
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        let lines = load_frame(&state);
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
        let mut reader = StructureReader::new();
        let file = reader
            .read_file(document.structure(), FileId::new(0))
            .unwrap()
            .unwrap();
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

        assert!(bounded_basename(&old_path).len() <= MAX_PATH_SCAN_BYTES);
        assert!(bounded_basename(&new_path).len() <= MAX_PATH_SCAN_BYTES);
        let fitted = fit_file_path_parts(Some(&old_path), Some(&new_path), 40);
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
    fn input_window_bounds_zero_width_text_around_the_cursor() {
        let text = "\u{200b}".repeat(MAX_INPUT_WINDOW_SCAN_BYTES);
        let window = input_window(&text, text.len(), 12);

        assert!(window.text.starts_with('…'));
        assert!(window.text.len() <= MAX_INPUT_WINDOW_SCAN_BYTES + '…'.len_utf8());
        assert!(window.cursor_cells < 12);
    }

    #[test]
    fn help_contract_matches_state_scroll_count() {
        assert_eq!(HELP_LINES.len(), 42);
    }
}
