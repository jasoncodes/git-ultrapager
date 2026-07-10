//! Interactive pager の一方向 state machine。
//!
//! Terminal event、streaming document snapshot、background search resultを `Event` として受け、
//! state変更と外部副作用 `Effect` を分離する。viewportはvisual-row indexを持つが、reflow時には
//! top rowのstable `LineId + content byte offset`へ戻す。rendererは公開getterだけを読み、keyや
//! worker channelを直接扱わない。

use std::num::NonZeroUsize;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

use crate::ansi::escape_path_bytes;
use crate::cli::WhitespaceChoice;
use crate::direct::{DocumentLayout, LayoutError, layout_document};
use crate::model::{Document, FileId, LineId};
use crate::search::{
    CompiledSearch, IncrementalQueryStatus, RepeatCommand, ScanRequest, ScanResult, SearchAnchor,
    SearchDirection, SearchGeneration, SearchLine, SearchSession, repeat_anchor, scan_lines,
};

const HELP_LINE_COUNT: usize = 42;

/// Main loopからstateへ入るevent。
#[derive(Clone, Debug)]
pub enum Event {
    Key(KeyEvent),
    Paste(String),
    Resize {
        width: u16,
        height: u16,
    },
    DocumentUpdated {
        document: Arc<Document>,
        eof: bool,
    },
    SearchCompleted {
        generation: SearchGeneration,
        result: ScanResult<LineId>,
        preview: bool,
    },
    /// Background task の panic/error を terminal loop が文字列へ変換して渡す。
    WorkerFailed {
        message: String,
    },
}

/// State updateがmain loopへ要求する副作用。
#[derive(Clone, Debug)]
pub enum Effect {
    Redraw,
    Repaint,
    Suspend,
    Quit(u8),
    Search(SearchTask),
}

/// Background scanへ渡すowned request。
#[derive(Clone, Debug)]
pub struct SearchTask {
    pub generation: SearchGeneration,
    pub compiled: CompiledSearch,
    pub request: ScanRequest,
    pub preview: bool,
}

/// 現在表示している画面mode。
#[derive(Clone, Debug)]
pub enum Mode {
    Body,
    Help { top: usize },
    FileList(FileListState),
    Search(SearchInputState),
}

/// File overlayのselection/filter state。
#[derive(Clone, Debug)]
pub struct FileListState {
    pub selected_file_index: usize,
    pub filtered_file_indices: Vec<usize>,
    pub filter: Option<TextInput>,
}

/// Incremental search入力中のlocal text state。
#[derive(Clone, Debug)]
pub struct SearchInputState {
    pub direction: SearchDirection,
    pub input: TextInput,
    pub origin_top: usize,
    pub count: NonZeroUsize,
    // SearchHistory の cursor は query update で reset されるため、input mode の履歴巡回位置は
    // state 側で保持する。worker request の世代更新と Up/Down の巡回を互いに干渉させない。
    history_cursor: Option<usize>,
    history_draft: Option<String>,
}

/// Grapheme単位の1行text editor。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextInput {
    text: String,
    cursor: usize,
}

impl TextInput {
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set(&mut self, value: impl Into<String>) {
        self.text = value.into();
        self.cursor = self.text.len();
    }

    pub fn insert(&mut self, value: &str) {
        self.text.insert_str(self.cursor, value);
        self.cursor += value.len();
    }

    pub fn move_left(&mut self) {
        self.cursor = previous_grapheme_boundary(&self.text, self.cursor).unwrap_or(0);
    }

    pub fn move_right(&mut self) {
        self.cursor = next_grapheme_boundary(&self.text, self.cursor).unwrap_or(self.text.len());
    }

    pub fn delete_previous(&mut self) {
        let Some(previous) = previous_grapheme_boundary(&self.text, self.cursor) else {
            return;
        };
        self.text.replace_range(previous..self.cursor, "");
        self.cursor = previous;
    }

    pub fn delete_next(&mut self) {
        let Some(next) = next_grapheme_boundary(&self.text, self.cursor) else {
            return;
        };
        self.text.replace_range(self.cursor..next, "");
    }

    pub fn delete_previous_word(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end_matches(char::is_whitespace);
        let start = trimmed.rfind(char::is_whitespace).map_or(0, |index| {
            index + trimmed[index..].chars().next().map_or(0, char::len_utf8)
        });
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }
}

/// Pagerのdocument/layout/modeを所有するstate。
pub struct AppState {
    document: Arc<Document>,
    layout: Arc<DocumentLayout>,
    layout_error: Option<LayoutError>,
    width: u16,
    height: u16,
    tab_width: usize,
    viewport_top: usize,
    mode: Mode,
    count_prefix: String,
    whitespace: WhitespaceChoice,
    follow: bool,
    eof: bool,
    pending_line: Option<usize>,
    search: SearchSession<LineId>,
    status: String,
    // 幅不足で layout を作れない間も、最後に見えていた論理位置を次の resize まで保持する。
    // 空 layout からは anchor を再構築できないため、viewport index だけでは復帰できない。
    deferred_layout_anchor: Option<(LineId, usize)>,
}

impl AppState {
    /// Initial documentをterminal sizeへlayoutしてbody modeを作る。
    pub fn new(
        document: Arc<Document>,
        width: u16,
        height: u16,
        tab_width: usize,
        whitespace: WhitespaceChoice,
        eof: bool,
    ) -> Self {
        let layout_result = layout_document(&document, usize::from(width), tab_width);
        let (layout, layout_error) = match layout_result {
            Ok(layout) => (Arc::new(layout), None),
            Err(error) => (Arc::new(DocumentLayout::default()), Some(error)),
        };
        Self {
            document,
            layout,
            layout_error,
            width,
            height,
            tab_width,
            viewport_top: 0,
            mode: Mode::Body,
            count_prefix: String::new(),
            whitespace,
            follow: false,
            eof,
            pending_line: None,
            search: SearchSession::default(),
            status: String::new(),
            deferred_layout_anchor: None,
        }
    }

    #[must_use]
    pub fn document(&self) -> &Document {
        &self.document
    }

    /// Background workerへ現在snapshotを渡す。Document本体をcloneせずArcの参照countだけを増やす。
    #[must_use]
    pub fn document_snapshot(&self) -> Arc<Document> {
        Arc::clone(&self.document)
    }

    #[must_use]
    pub fn layout(&self) -> &DocumentLayout {
        &self.layout
    }

    #[must_use]
    pub const fn layout_error(&self) -> Option<&LayoutError> {
        self.layout_error.as_ref()
    }

    #[must_use]
    pub const fn viewport_top(&self) -> usize {
        self.viewport_top
    }

    #[must_use]
    pub const fn mode(&self) -> &Mode {
        &self.mode
    }

    #[must_use]
    pub const fn whitespace(&self) -> WhitespaceChoice {
        self.whitespace
    }

    #[must_use]
    pub const fn is_following(&self) -> bool {
        self.follow
    }

    #[must_use]
    pub const fn is_eof(&self) -> bool {
        self.eof
    }

    #[must_use]
    pub const fn tab_width(&self) -> usize {
        self.tab_width
    }

    #[must_use]
    pub const fn terminal_size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    #[must_use]
    pub const fn search(&self) -> &SearchSession<LineId> {
        &self.search
    }

    #[must_use]
    pub fn body_height(&self) -> usize {
        usize::from(self.height).saturating_sub(2)
    }

    /// Eventを適用し、I/Oを伴うeffectを返す。
    pub fn update(&mut self, event: Event) -> Vec<Effect> {
        let mut effects = match event {
            Event::Key(key) => self.handle_key(key),
            Event::Paste(text) => self.handle_paste(&text),
            Event::Resize { width, height } => {
                self.resize(width, height);
                vec![Effect::Redraw]
            }
            Event::DocumentUpdated { document, eof } => self.replace_document(document, eof),
            Event::SearchCompleted {
                generation,
                result,
                preview,
            } => self.apply_search_result(generation, result, preview),
            Event::WorkerFailed { message } => {
                self.status = message;
                Vec::new()
            }
        };
        if effects.is_empty() {
            effects.push(Effect::Redraw);
        }
        effects
    }

    fn handle_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if matches!(key.kind, KeyEventKind::Release) {
            return Vec::new();
        }
        if is_ctrl(&key, 'c') {
            return vec![Effect::Quit(130)];
        }
        if is_ctrl(&key, 'z') {
            return vec![Effect::Suspend];
        }
        if is_ctrl(&key, 'l') {
            return vec![Effect::Repaint];
        }

        match &self.mode {
            Mode::Search(_) => self.handle_search_key(key),
            Mode::Help { .. } => self.handle_help_key(key),
            Mode::FileList(_) => self.handle_file_list_key(key),
            Mode::Body => self.handle_body_key(key),
        }
    }

    fn handle_body_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if let KeyCode::Char(digit) = key.code
            && key.modifiers.is_empty()
            && digit.is_ascii_digit()
        {
            self.count_prefix.push(digit);
            self.status = format!("count: {}", self.count_prefix);
            return Vec::new();
        }
        if key.code == KeyCode::Esc {
            self.count_prefix.clear();
            self.pending_line = None;
            self.search.begin_navigation_intent();
            self.status = "cancelled".to_owned();
            return Vec::new();
        }

        let count = match self.take_count() {
            Ok(count) => count,
            Err(message) => {
                self.status = message;
                return Vec::new();
            }
        };
        let counted = count.is_some();
        let amount = count.map_or(1, NonZeroUsize::get);

        if is_down_line(&key) {
            self.move_rows(true, amount);
        } else if is_up_line(&key) {
            self.move_rows(false, amount);
        } else if is_down_page(&key) {
            self.move_rows(true, self.page_size().saturating_mul(amount));
        } else if is_up_page(&key) {
            self.move_rows(false, self.page_size().saturating_mul(amount));
        } else if is_down_half_page(&key) {
            self.move_rows(true, self.half_page_size().saturating_mul(amount));
        } else if is_up_half_page(&key) {
            self.move_rows(false, self.half_page_size().saturating_mul(amount));
        } else {
            return self.handle_body_command(key, count, counted);
        }
        Vec::new()
    }

    fn handle_body_command(
        &mut self,
        key: KeyEvent,
        count: Option<NonZeroUsize>,
        counted: bool,
    ) -> Vec<Effect> {
        // Ctrl/Alt を伴う未知の文字を plain command として解釈しない。既知の Ctrl navigation は
        // この関数へ来る前に処理済みで、uppercase/symbol に付く Shift だけを許可する。
        if matches!(key.code, KeyCode::Char(_))
            && !matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
        {
            self.status = if counted {
                "count discarded".to_owned()
            } else {
                "unknown key".to_owned()
            };
            return Vec::new();
        }
        match key.code {
            KeyCode::Char('q') if key.modifiers.is_empty() => vec![Effect::Quit(0)],
            KeyCode::Char('g') | KeyCode::Home => {
                if let Some(count) = count {
                    self.go_to_input_line(count.get());
                } else {
                    self.go_to_row(0);
                }
                Vec::new()
            }
            KeyCode::Char('G') | KeyCode::End => {
                if let Some(count) = count {
                    self.go_to_input_line(count.get());
                } else {
                    // Bare G は現在位置に関係なく、それ以前の pending navigation を置き換える。
                    self.pending_line = None;
                    self.search.begin_navigation_intent();
                    self.viewport_top = self.max_viewport_top();
                    self.follow = !self.eof;
                    self.status = if self.follow {
                        "following incoming data…".to_owned()
                    } else {
                        "end of input".to_owned()
                    };
                }
                Vec::new()
            }
            KeyCode::Char('[') => {
                self.jump_structure(Structure::Hunk, false, count_value(count));
                Vec::new()
            }
            KeyCode::Char(']') => {
                self.jump_structure(Structure::Hunk, true, count_value(count));
                Vec::new()
            }
            KeyCode::Char('{') => {
                self.jump_structure(Structure::File, false, count_value(count));
                Vec::new()
            }
            KeyCode::Char('}') => {
                self.jump_structure(Structure::File, true, count_value(count));
                Vec::new()
            }
            KeyCode::Char('(') => {
                self.jump_structure(Structure::Commit, false, count_value(count));
                Vec::new()
            }
            KeyCode::Char(')') => {
                self.jump_structure(Structure::Commit, true, count_value(count));
                Vec::new()
            }
            KeyCode::Char('/') => self.begin_search(SearchDirection::Forward, count),
            KeyCode::Char('?') => self.begin_search(SearchDirection::Backward, count),
            KeyCode::Char('n') => self.repeat_search(RepeatCommand::Next, count),
            KeyCode::Char('N') => self.repeat_search(RepeatCommand::Opposite, count),
            KeyCode::Tab => {
                if counted {
                    self.status = "count ignored for file list".to_owned();
                }
                self.open_file_list();
                Vec::new()
            }
            KeyCode::Char('h' | 'H') => {
                if counted {
                    self.status = "count ignored for help".to_owned();
                }
                self.mode = Mode::Help { top: 0 };
                Vec::new()
            }
            KeyCode::Char('W') => {
                self.whitespace = match self.whitespace {
                    WhitespaceChoice::Ignore => WhitespaceChoice::Show,
                    WhitespaceChoice::Show => WhitespaceChoice::Ignore,
                };
                let whitespace = format!("whitespace: {:?}", self.whitespace).to_ascii_lowercase();
                self.status = if counted {
                    format!("count ignored; {whitespace}")
                } else {
                    whitespace
                };
                Vec::new()
            }
            _ => {
                self.status = if counted {
                    "count discarded".to_owned()
                } else {
                    "unknown key".to_owned()
                };
                Vec::new()
            }
        }
    }

    fn handle_help_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.code == KeyCode::Esc
            || matches!(key.code, KeyCode::Char('q' | 'h' | 'H')) && is_plain_text_key(&key)
        {
            self.mode = Mode::Body;
            return Vec::new();
        }
        let body_height = self.body_height();
        if let Mode::Help { top } = &mut self.mode {
            let max_top = HELP_LINE_COUNT.saturating_sub(body_height);
            if is_down_line(&key) {
                *top = top.saturating_add(1).min(max_top);
            } else if is_up_line(&key) {
                *top = top.saturating_sub(1);
            } else if is_down_page(&key) {
                *top = top
                    .saturating_add(body_height.saturating_sub(1))
                    .min(max_top);
            } else if is_up_page(&key) {
                *top = top.saturating_sub(body_height.saturating_sub(1));
            } else if is_down_half_page(&key) {
                *top = top
                    .saturating_add(body_height.div_ceil(2).max(1))
                    .min(max_top);
            } else if is_up_half_page(&key) {
                *top = top.saturating_sub(body_height.div_ceil(2).max(1));
            } else if key.code == KeyCode::Home
                || key.code == KeyCode::Char('g') && is_plain_text_key(&key)
            {
                *top = 0;
            } else if key.code == KeyCode::End
                || key.code == KeyCode::Char('G') && is_plain_text_key(&key)
            {
                *top = max_top;
            }
        }
        Vec::new()
    }

    fn handle_file_list_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.code == KeyCode::Tab {
            self.mode = Mode::Body;
            return Vec::new();
        }
        let filter_active = matches!(&self.mode, Mode::FileList(state) if state.filter.is_some());
        if filter_active {
            return self.handle_filter_key(key);
        }

        if key.code == KeyCode::Esc || key.code == KeyCode::Char('q') && is_plain_text_key(&key) {
            self.mode = Mode::Body;
            return Vec::new();
        }
        if key.code == KeyCode::Char('/') && key.modifiers.is_empty() {
            if let Mode::FileList(state) = &mut self.mode {
                state.filter = Some(TextInput::default());
            }
            return Vec::new();
        }
        if key.code == KeyCode::Enter {
            self.jump_selected_file();
            return Vec::new();
        }

        let page = self.body_height().saturating_sub(2).max(1);
        if let Mode::FileList(state) = &mut self.mode {
            let len = state.filtered_file_indices.len();
            if len == 0 {
                return Vec::new();
            }
            if is_down_line(&key) {
                state.selected_file_index = (state.selected_file_index + 1).min(len - 1);
            } else if is_up_line(&key) {
                state.selected_file_index = state.selected_file_index.saturating_sub(1);
            } else if is_down_page(&key) {
                state.selected_file_index = (state.selected_file_index + page).min(len - 1);
            } else if is_up_page(&key) {
                state.selected_file_index = state.selected_file_index.saturating_sub(page);
            } else if key.code == KeyCode::Home
                || key.code == KeyCode::Char('g') && is_plain_text_key(&key)
            {
                state.selected_file_index = 0;
            } else if key.code == KeyCode::End
                || key.code == KeyCode::Char('G') && is_plain_text_key(&key)
            {
                state.selected_file_index = len - 1;
            }
        }
        Vec::new()
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.code == KeyCode::Esc {
            let nonempty = matches!(&self.mode, Mode::FileList(state) if state.filter.as_ref().is_some_and(|filter| !filter.text().is_empty()));
            if nonempty {
                let selected_file = self.selected_file_id();
                if let Mode::FileList(state) = &mut self.mode {
                    state.filter = None;
                }
                self.refresh_file_list(selected_file);
            } else {
                self.mode = Mode::Body;
            }
            return Vec::new();
        }
        if key.code == KeyCode::Enter {
            self.jump_selected_file();
            return Vec::new();
        }
        if matches!(
            key.code,
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
        ) {
            return self.move_filter_selection(key.code);
        }
        if let Mode::FileList(state) = &mut self.mode
            && let Some(filter) = &mut state.filter
        {
            edit_text_input(filter, &key);
        }
        self.recompute_file_filter();
        Vec::new()
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.code == KeyCode::Esc {
            let origin = self
                .search
                .cancel_incremental()
                .and_then(|anchor| self.row_for_search_anchor(anchor));
            let fallback = match &self.mode {
                Mode::Search(state) => state.origin_top,
                _ => self.viewport_top,
            };
            self.viewport_top = origin.unwrap_or(fallback);
            self.mode = Mode::Body;
            return Vec::new();
        }
        if key.code == KeyCode::Enter {
            return self.commit_search();
        }
        if key.code == KeyCode::Up {
            self.browse_search_history(false);
        } else if key.code == KeyCode::Down {
            self.browse_search_history(true);
        } else if let Mode::Search(state) = &mut self.mode {
            let previous = state.input.text().to_owned();
            edit_text_input(&mut state.input, &key);
            if state.input.text() != previous {
                state.history_cursor = None;
                state.history_draft = None;
            }
        }
        self.preview_search()
    }

    fn handle_paste(&mut self, pasted: &str) -> Vec<Effect> {
        let escaped = escape_path_bytes(pasted.as_bytes());
        match &mut self.mode {
            Mode::Search(state) => {
                state.input.insert(&escaped);
                state.history_cursor = None;
                state.history_draft = None;
                self.preview_search()
            }
            Mode::FileList(state) => {
                if let Some(filter) = &mut state.filter {
                    filter.insert(&escaped);
                    self.recompute_file_filter();
                } else {
                    self.status = "paste ignored outside text input".to_owned();
                }
                Vec::new()
            }
            _ => {
                self.status = "paste ignored outside text input".to_owned();
                Vec::new()
            }
        }
    }

    fn begin_search(
        &mut self,
        direction: SearchDirection,
        count: Option<NonZeroUsize>,
    ) -> Vec<Effect> {
        let origin = self.current_search_anchor();
        self.search.begin_incremental(direction, origin);
        self.mode = Mode::Search(SearchInputState {
            direction,
            input: TextInput::default(),
            origin_top: self.viewport_top,
            count: count.unwrap_or(NonZeroUsize::MIN),
            history_cursor: None,
            history_draft: None,
        });
        // SearchSession の generation 更新が committed forward search を破棄する。Ng は別 state
        // なので、検索入力を開いた時点でこちらも明示的に取り消す。
        self.pending_line = None;
        Vec::new()
    }

    fn preview_search(&mut self) -> Vec<Effect> {
        let (query, direction, count) = match &self.mode {
            Mode::Search(state) => (state.input.text().to_owned(), state.direction, state.count),
            _ => return Vec::new(),
        };
        let Some(update) = self.search.update_incremental(&query) else {
            return Vec::new();
        };
        match &update.status {
            IncrementalQueryStatus::Empty => {
                let origin = self
                    .search
                    .incremental()
                    .map(|incremental| incremental.original_anchor())
                    .and_then(|anchor| self.row_for_search_anchor(anchor));
                if let Some(origin) = origin {
                    self.viewport_top = origin.min(self.max_viewport_top());
                }
                self.status.clear();
                return Vec::new();
            }
            IncrementalQueryStatus::Invalid(error) => {
                self.status = format!("invalid regular expression: {error}");
                return Vec::new();
            }
            IncrementalQueryStatus::Valid => self.status.clear(),
        }
        let anchor = self.search.incremental().map_or_else(
            || self.current_search_anchor(),
            |query| query.original_anchor(),
        );
        let Some(compiled) = self
            .search
            .incremental()
            .and_then(|incremental| incremental.compiled_for_current_query())
            .cloned()
        else {
            return Vec::new();
        };
        vec![Effect::Search(SearchTask {
            generation: update.generation,
            compiled,
            request: ScanRequest::new(direction, anchor, count),
            preview: true,
        })]
    }

    fn commit_search(&mut self) -> Vec<Effect> {
        let count = match &self.mode {
            Mode::Search(state) => state.count,
            _ => NonZeroUsize::MIN,
        };
        match self.search.commit_incremental() {
            Ok(committed) => {
                self.mode = Mode::Body;
                if let Some(found) = committed.preview.and_then(|preview| preview.found) {
                    self.land_on_match(&found);
                    return Vec::new();
                }
                vec![Effect::Search(SearchTask {
                    generation: committed.generation,
                    compiled: committed.compiled,
                    request: ScanRequest::new(committed.direction, committed.origin, count),
                    preview: false,
                })]
            }
            Err(error) => {
                self.status = error.to_string();
                Vec::new()
            }
        }
    }

    fn repeat_search(
        &mut self,
        command: RepeatCommand,
        count: Option<NonZeroUsize>,
    ) -> Vec<Effect> {
        let Some(direction) = self.search.direction_for_repeat(command) else {
            self.status = "no previous search".to_owned();
            return Vec::new();
        };
        let Some(active) = self.search.active().cloned() else {
            return Vec::new();
        };
        let lines = self.search_lines();
        let anchor = self.search.current_match().map_or_else(
            || self.current_search_anchor(),
            |current| repeat_anchor(current, direction, &lines),
        );
        self.pending_line = None;
        let generation = self.search.begin_navigation_intent();
        vec![Effect::Search(SearchTask {
            generation,
            compiled: active.compiled().clone(),
            request: ScanRequest::new(direction, anchor, count.unwrap_or(NonZeroUsize::MIN)),
            preview: false,
        })]
    }

    fn apply_search_result(
        &mut self,
        generation: SearchGeneration,
        result: ScanResult<LineId>,
        preview: bool,
    ) -> Vec<Effect> {
        let found = result.found.clone();
        let applied = if preview {
            self.search.apply_incremental_preview(generation, result)
        } else {
            self.search.apply_scan_result(generation, result, self.eof)
        };
        if applied {
            if let Some(found) = found {
                self.land_on_match(&found);
            } else if !preview && self.search.pending_forward().is_none() {
                self.status = "pattern not found".to_owned();
            } else if !preview {
                self.status = "searching incoming data…".to_owned();
            }
        }
        Vec::new()
    }

    fn browse_search_history(&mut self, newer: bool) {
        let entries = self.search.history().entries();
        let Mode::Search(state) = &mut self.mode else {
            return;
        };

        if newer {
            let Some(cursor) = state.history_cursor else {
                return;
            };
            if cursor + 1 < entries.len() {
                let next = cursor + 1;
                state.history_cursor = Some(next);
                state.input.set(entries[next].clone());
            } else {
                state.history_cursor = None;
                state
                    .input
                    .set(state.history_draft.take().unwrap_or_default());
            }
            return;
        }

        if entries.is_empty() {
            return;
        }
        let index = state
            .history_cursor
            .map_or(entries.len() - 1, |cursor| cursor.saturating_sub(1));
        if state.history_cursor.is_none() {
            state.history_draft = Some(state.input.text().to_owned());
        }
        state.history_cursor = Some(index);
        state.input.set(entries[index].clone());
    }

    /// Main loopから呼び、owned search taskを現在snapshotへ同期scanする。
    #[must_use]
    pub fn execute_search_task(&self, task: &SearchTask) -> ScanResult<LineId> {
        let lines = self.search_lines();
        scan_lines(&task.compiled, &lines, task.request)
    }

    fn replace_document(&mut self, document: Arc<Document>, eof: bool) -> Vec<Effect> {
        let anchor = self.current_layout_anchor().or(self.deferred_layout_anchor);
        let selected_file = self.selected_file_id();
        self.document = document;
        self.eof = eof;
        self.reflow(anchor);
        self.refresh_file_list(selected_file);
        if self.follow {
            self.viewport_top = self.max_viewport_top();
        }
        if let Some(line) = self.pending_line
            && line <= self.document.lines().len()
        {
            self.go_to_input_line(line);
            self.pending_line = None;
        } else if let Some(line) = self.pending_line
            && eof
        {
            self.pending_line = None;
            self.status = format!("input line {line} is past EOF");
        }
        if self.follow && eof {
            self.follow = false;
            self.status = "end of input".to_owned();
        }
        let pending_task = self.search.pending_forward().and_then(|pending| {
            self.search.active().map(|active| SearchTask {
                generation: pending.generation,
                compiled: active.compiled().clone(),
                request: pending.request(),
                preview: false,
            })
        });
        if let Some(task) = pending_task {
            return vec![Effect::Search(task)];
        }
        if matches!(&self.mode, Mode::Search(_)) {
            // Document snapshot も worker result の鮮度を決める。query が同じでも generation を進め、
            // append 前 snapshot を走査中の preview が後着して viewport を戻すのを防ぐ。
            return self.preview_search();
        }
        Vec::new()
    }

    fn resize(&mut self, width: u16, height: u16) {
        let anchor = self.current_layout_anchor().or(self.deferred_layout_anchor);
        self.width = width;
        self.height = height;
        self.reflow(anchor);
        if self.follow {
            self.viewport_top = self.max_viewport_top();
        }
    }

    fn reflow(&mut self, anchor: Option<(LineId, usize)>) {
        match layout_document(&self.document, usize::from(self.width), self.tab_width) {
            Ok(layout) => {
                self.layout = Arc::new(layout);
                self.layout_error = None;
                self.deferred_layout_anchor = None;
                self.viewport_top = anchor
                    .and_then(|(line, offset)| self.layout.row_for_anchor(line, offset))
                    .unwrap_or(self.viewport_top)
                    .min(self.max_viewport_top());
            }
            Err(error) => {
                self.layout = Arc::new(DocumentLayout::default());
                self.layout_error = Some(error);
                self.deferred_layout_anchor = anchor;
                self.viewport_top = 0;
            }
        }
    }

    fn current_layout_anchor(&self) -> Option<(LineId, usize)> {
        self.layout
            .rows()
            .get(self.viewport_top)
            .map(|row| (row.line_id, row.content_range.start))
    }

    fn current_search_anchor(&self) -> SearchAnchor {
        self.layout
            .rows()
            .get(self.viewport_top)
            .map_or(SearchAnchor::BeforeDocument, |row| {
                SearchAnchor::at(row.line_id.get() as usize, row.content_range.start)
            })
    }

    fn row_for_search_anchor(&self, anchor: SearchAnchor) -> Option<usize> {
        match anchor {
            SearchAnchor::BeforeDocument => Some(0),
            SearchAnchor::AfterDocument => Some(self.max_viewport_top()),
            SearchAnchor::At {
                line_index,
                byte_offset,
            } => self
                .document
                .lines()
                .get(line_index)
                .and_then(|line| self.layout.row_for_anchor(line.id, byte_offset)),
        }
    }

    fn search_lines(&self) -> Vec<SearchLine<'_, LineId>> {
        self.document
            .lines()
            .iter()
            .map(|line| SearchLine::new(line.id, line.text.as_str()))
            .collect()
    }

    fn land_on_match(&mut self, found: &crate::search::SearchMatch<LineId>) {
        if let Some(row) = self
            .layout
            .row_for_anchor(found.line_id, found.byte_range.start)
        {
            // Result は current generation の完了なので、ここで generation を進めると同じ query の
            // commit/repeat と世代が表示位置に依存する。検索成功として他の navigation だけを解除する。
            self.viewport_top = row.min(self.max_viewport_top());
            self.follow = false;
            self.pending_line = None;
        }
    }

    fn move_rows(&mut self, forward: bool, amount: usize) {
        let previous = self.viewport_top;
        if forward {
            self.viewport_top = self
                .viewport_top
                .saturating_add(amount)
                .min(self.max_viewport_top());
        } else {
            self.viewport_top = self.viewport_top.saturating_sub(amount);
        }
        if self.viewport_top != previous {
            self.follow = false;
            self.pending_line = None;
            self.search.begin_navigation_intent();
            self.status.clear();
        } else {
            self.status = if forward {
                if self.eof {
                    "end of input".to_owned()
                } else {
                    "end of loaded input (loading)".to_owned()
                }
            } else {
                "beginning of input".to_owned()
            };
        }
    }

    fn go_to_row(&mut self, row: usize) {
        let previous = self.viewport_top;
        self.viewport_top = row.min(self.max_viewport_top());
        if self.viewport_top != previous {
            self.follow = false;
            self.pending_line = None;
            self.search.begin_navigation_intent();
            self.status.clear();
        }
    }

    fn go_to_input_line(&mut self, one_based: usize) {
        if one_based == 0 {
            self.status = "count must be greater than zero".to_owned();
            return;
        }
        if let Some(line) = self.document.lines().get(one_based - 1) {
            if let Some(row) = self.layout.row_for_line(line.id) {
                self.go_to_row(row);
            }
        } else if !self.eof {
            self.follow = false;
            self.search.begin_navigation_intent();
            self.pending_line = Some(one_based);
            self.status = format!("waiting for input line {one_based}…");
        } else {
            self.pending_line = None;
            self.search.begin_navigation_intent();
            self.status = format!("input line {one_based} is past EOF");
        }
    }

    fn jump_structure(&mut self, structure: Structure, forward: bool, count: usize) {
        let current = self.viewport_top;
        let rows: Vec<usize> = match structure {
            Structure::Hunk => self
                .document
                .hunks()
                .iter()
                .filter_map(|hunk| self.layout.row_for_hunk(hunk.id))
                .collect(),
            Structure::File => self
                .document
                .files()
                .iter()
                .filter_map(|file| self.layout.row_for_file(file.id))
                .collect(),
            Structure::Commit => self
                .document
                .commits()
                .iter()
                .filter_map(|commit| self.layout.row_for_commit(commit.id))
                .collect(),
        };
        let target = if forward {
            rows.into_iter().filter(|row| *row > current).nth(count - 1)
        } else {
            rows.into_iter()
                .rev()
                .filter(|row| *row < current)
                .nth(count - 1)
        };
        if let Some(target) = target {
            self.go_to_row(target);
        } else {
            self.status = if self.eof {
                "no more loaded target".to_owned()
            } else {
                "no next loaded target (loading)".to_owned()
            };
        }
    }

    fn open_file_list(&mut self) {
        let active_file = self
            .layout
            .rows()
            .get(self.viewport_top)
            .and_then(|row| row.file_id);
        let selected = active_file
            .and_then(|id| self.document.files().iter().position(|file| file.id == id))
            .unwrap_or(0);
        self.mode = Mode::FileList(FileListState {
            selected_file_index: selected,
            filtered_file_indices: (0..self.document.files().len()).collect(),
            filter: None,
        });
    }

    fn recompute_file_filter(&mut self) {
        let selected_file = self.selected_file_id();
        self.refresh_file_list(selected_file);
    }

    fn selected_file_id(&self) -> Option<FileId> {
        let Mode::FileList(state) = &self.mode else {
            return None;
        };
        state
            .filtered_file_indices
            .get(state.selected_file_index)
            .and_then(|index| self.document.files().get(*index))
            .map(|file| file.id)
    }

    fn refresh_file_list(&mut self, selected_file: Option<FileId>) {
        let Mode::FileList(state) = &self.mode else {
            return;
        };
        let query = state
            .filter
            .as_ref()
            .map(|filter| filter.text().to_lowercase());
        let matches: Vec<usize> = self
            .document
            .files()
            .iter()
            .enumerate()
            .filter(|(_, file)| {
                query
                    .as_ref()
                    .is_none_or(|query| is_subsequence(query, &file.display_path().to_lowercase()))
            })
            .map(|(index, _)| index)
            .collect();
        if let Mode::FileList(state) = &mut self.mode {
            state.filtered_file_indices = matches;
            state.selected_file_index = selected_file
                .and_then(|selected| {
                    state.filtered_file_indices.iter().position(|index| {
                        self.document.files().get(*index).map(|file| file.id) == Some(selected)
                    })
                })
                .unwrap_or_else(|| {
                    state
                        .selected_file_index
                        .min(state.filtered_file_indices.len().saturating_sub(1))
                });
        }
    }

    fn move_filter_selection(&mut self, key: KeyCode) -> Vec<Effect> {
        let page = self.body_height().saturating_sub(2).max(1);
        if let Mode::FileList(state) = &mut self.mode {
            let len = state.filtered_file_indices.len();
            if len == 0 {
                return Vec::new();
            }
            state.selected_file_index = match key {
                KeyCode::Up => state.selected_file_index.saturating_sub(1),
                KeyCode::Down => (state.selected_file_index + 1).min(len - 1),
                KeyCode::PageUp => state.selected_file_index.saturating_sub(page),
                KeyCode::PageDown => (state.selected_file_index + page).min(len - 1),
                _ => state.selected_file_index,
            };
        }
        Vec::new()
    }

    fn jump_selected_file(&mut self) {
        let file_id: Option<FileId> = match &self.mode {
            Mode::FileList(state) => state
                .filtered_file_indices
                .get(state.selected_file_index)
                .and_then(|index| self.document.files().get(*index))
                .map(|file| file.id),
            _ => None,
        };
        if let Some(row) = file_id.and_then(|id| self.layout.row_for_file(id)) {
            self.mode = Mode::Body;
            self.go_to_row(row);
        }
    }

    fn take_count(&mut self) -> Result<Option<NonZeroUsize>, String> {
        if self.count_prefix.is_empty() {
            return Ok(None);
        }
        let raw = std::mem::take(&mut self.count_prefix);
        let count = raw
            .parse::<usize>()
            .map_err(|_| "count overflow".to_owned())?;
        NonZeroUsize::new(count)
            .map(Some)
            .ok_or_else(|| "count must be greater than zero".to_owned())
    }

    fn max_viewport_top(&self) -> usize {
        self.layout.len().saturating_sub(self.body_height().max(1))
    }

    fn page_size(&self) -> usize {
        self.body_height().saturating_sub(1).max(1)
    }

    fn half_page_size(&self) -> usize {
        self.body_height().div_ceil(2).max(1)
    }
}

#[derive(Clone, Copy)]
enum Structure {
    Hunk,
    File,
    Commit,
}

fn edit_text_input(input: &mut TextInput, key: &KeyEvent) {
    if is_ctrl(key, 'a') || key.code == KeyCode::Home {
        input.cursor = 0;
    } else if is_ctrl(key, 'e') || key.code == KeyCode::End {
        input.cursor = input.text.len();
    } else if is_ctrl(key, 'b') || key.code == KeyCode::Left {
        input.move_left();
    } else if is_ctrl(key, 'f') || key.code == KeyCode::Right {
        input.move_right();
    } else if is_ctrl(key, 'u') {
        input.clear();
    } else if is_ctrl(key, 'w') {
        input.delete_previous_word();
    } else if key.code == KeyCode::Backspace {
        input.delete_previous();
    } else if key.code == KeyCode::Delete {
        input.delete_next();
    } else if let KeyCode::Char(character) = key.code
        && !key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
    {
        input.insert(&character.to_string());
    }
}

fn previous_grapheme_boundary(text: &str, cursor: usize) -> Option<usize> {
    text.get(..cursor)?
        .grapheme_indices(true)
        .next_back()
        .map(|(index, _)| index)
}

fn next_grapheme_boundary(text: &str, cursor: usize) -> Option<usize> {
    let suffix = text.get(cursor..)?;
    let mut graphemes = suffix.grapheme_indices(true);
    graphemes.next()?;
    graphemes
        .next()
        .map(|(index, _)| cursor + index)
        .or(Some(text.len()))
}

fn is_subsequence(query: &str, candidate: &str) -> bool {
    let mut characters = candidate.chars();
    query
        .chars()
        .all(|needle| characters.by_ref().any(|candidate| candidate == needle))
}

fn is_ctrl(key: &KeyEvent, character: char) -> bool {
    key.code == KeyCode::Char(character) && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn is_plain_text_key(key: &KeyEvent) -> bool {
    matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
}

fn is_down_line(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Down | KeyCode::Enter)
        || matches!(key.code, KeyCode::Char('j' | 'e')) && key.modifiers.is_empty()
        || is_ctrl(key, 'e')
        || is_ctrl(key, 'n')
}

fn is_up_line(key: &KeyEvent) -> bool {
    key.code == KeyCode::Up
        || matches!(key.code, KeyCode::Char('k' | 'y')) && key.modifiers.is_empty()
        || is_ctrl(key, 'y')
        || is_ctrl(key, 'p')
}

fn is_down_page(key: &KeyEvent) -> bool {
    key.code == KeyCode::PageDown
        || matches!(key.code, KeyCode::Char(' ' | 'f')) && key.modifiers.is_empty()
        || is_ctrl(key, 'f')
        || is_ctrl(key, 'v')
}

fn is_up_page(key: &KeyEvent) -> bool {
    key.code == KeyCode::PageUp
        || key.code == KeyCode::Char('b') && key.modifiers.is_empty()
        || is_ctrl(key, 'b')
}

fn is_down_half_page(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('d') && (key.modifiers.is_empty() || is_ctrl(key, 'd'))
}

fn is_up_half_page(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('u') && (key.modifiers.is_empty() || is_ctrl(key, 'u'))
}

fn count_value(count: Option<NonZeroUsize>) -> usize {
    count.map_or(1, NonZeroUsize::get)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_bytes;
    use crate::search::compile_smart_case;

    fn key(character: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
    }

    fn special(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn search_task(effects: &[Effect]) -> SearchTask {
        effects
            .iter()
            .find_map(|effect| match effect {
                Effect::Search(task) => Some(task.clone()),
                _ => None,
            })
            .expect("search effect")
    }

    fn state() -> AppState {
        let document = parse_bytes(
            b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,3 @@\n one\n-two\n+three\n four\n",
        );
        AppState::new(Arc::new(document), 20, 7, 4, WhitespaceChoice::Ignore, true)
    }

    #[test]
    fn e_and_y_match_j_and_k() {
        let mut app = state();
        app.update(key('e'));
        assert_eq!(app.viewport_top(), 1);
        app.update(key('y'));
        assert_eq!(app.viewport_top(), 0);
    }

    #[test]
    fn count_repeats_visual_rows() {
        let mut app = state();
        app.update(key('3'));
        app.update(key('j'));
        assert_eq!(app.viewport_top(), 3.min(app.max_viewport_top()));
    }

    #[test]
    fn q_is_text_while_filter_is_active() {
        let mut app = state();
        app.update(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        app.update(key('/'));
        app.update(key('q'));
        let Mode::FileList(list) = app.mode() else {
            panic!("expected file list");
        };
        assert_eq!(list.filter.as_ref().map(TextInput::text), Some("q"));
    }

    #[test]
    fn bare_end_enables_follow_until_successful_movement() {
        let mut app = state();
        app.eof = false;
        app.update(special(KeyCode::End));
        assert!(app.is_following());
        app.update(key('j'));
        assert!(app.is_following());
        app.update(key('k'));
        assert!(!app.is_following());
    }

    #[test]
    fn huge_count_saturates_at_document_boundary() {
        let mut app = state();
        for digit in usize::MAX.to_string().chars() {
            app.update(key(digit));
        }
        app.update(key('j'));
        assert_eq!(app.viewport_top(), app.max_viewport_top());
    }

    #[test]
    fn incremental_search_keeps_origin_and_generation_after_preview_lands() {
        let mut app = state();
        let origin = app.current_search_anchor();
        app.update(key('/'));
        let first = search_task(&app.update(Event::Paste("three".to_owned())));
        assert_eq!(first.request.anchor, origin);
        let result = app.execute_search_task(&first);
        app.update(Event::SearchCompleted {
            generation: first.generation,
            result,
            preview: true,
        });
        assert!(app.viewport_top() > 0);
        assert_eq!(app.search().generation(), first.generation);

        let refined = search_task(&app.update(Event::Paste("$".to_owned())));
        assert_eq!(refined.request.anchor, origin);
    }

    #[test]
    fn search_count_survives_commit_before_preview_returns() {
        let mut app = state();
        app.update(key('2'));
        app.update(key('/'));
        app.update(Event::Paste("a.rs".to_owned()));
        let committed = search_task(&app.update(special(KeyCode::Enter)));
        assert_eq!(committed.request.count.get(), 2);
        assert!(!committed.preview);
    }

    #[test]
    fn clearing_incremental_query_restores_search_origin() {
        let mut app = state();
        let origin_top = app.viewport_top();
        app.update(key('/'));
        let task = search_task(&app.update(Event::Paste("three".to_owned())));
        let result = app.execute_search_task(&task);
        app.update(Event::SearchCompleted {
            generation: task.generation,
            result,
            preview: true,
        });
        assert_ne!(app.viewport_top(), origin_top);

        app.update(Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.viewport_top(), origin_top);
    }

    #[test]
    fn history_navigation_is_not_reset_by_preview_generation() {
        let mut app = state();
        app.search.activate(
            compile_smart_case("alpha").expect("valid regex"),
            SearchDirection::Forward,
        );
        app.search.activate(
            compile_smart_case("beta").expect("valid regex"),
            SearchDirection::Forward,
        );
        app.update(key('/'));
        app.update(Event::Paste("draft".to_owned()));

        app.update(special(KeyCode::Up));
        assert_search_input(&app, "beta");
        app.update(special(KeyCode::Up));
        assert_search_input(&app, "alpha");
        app.update(special(KeyCode::Down));
        assert_search_input(&app, "beta");
        app.update(special(KeyCode::Down));
        assert_search_input(&app, "draft");
    }

    fn assert_search_input(app: &AppState, expected: &str) {
        let Mode::Search(search) = app.mode() else {
            panic!("expected search input");
        };
        assert_eq!(search.input.text(), expected);
    }

    #[test]
    fn resize_recovers_logical_anchor_after_width_is_temporarily_too_small() {
        let mut app = state();
        app.go_to_row(2);
        let anchor = app.current_layout_anchor();

        app.update(Event::Resize {
            width: 1,
            height: 7,
        });
        assert!(app.layout_error().is_some());
        app.update(Event::Resize {
            width: 20,
            height: 7,
        });

        assert_eq!(app.layout_error(), None);
        assert_eq!(app.current_layout_anchor(), anchor);
    }

    #[test]
    fn resize_keeps_following_at_reflowed_tail() {
        let mut app = state();
        app.eof = false;
        app.update(special(KeyCode::End));
        app.update(Event::Resize {
            width: 18,
            height: 6,
        });
        assert!(app.is_following());
        assert_eq!(app.viewport_top(), app.max_viewport_top());
    }

    #[test]
    fn clearing_filter_and_stream_update_preserve_selected_file_identity() {
        let initial = b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-a\n+A\ndiff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -1 +1 @@\n-b\n+B\n";
        let updated = b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-a\n+A\ndiff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -1 +1 @@\n-b\n+B\ndiff --git a/c.rs b/c.rs\n--- a/c.rs\n+++ b/c.rs\n@@ -1 +1 @@\n-c\n+C\n";
        let mut app = AppState::new(
            Arc::new(parse_bytes(initial)),
            40,
            10,
            4,
            WhitespaceChoice::Ignore,
            false,
        );
        app.update(special(KeyCode::Tab));
        app.update(key('j'));
        app.update(key('/'));
        app.update(Event::Paste("b".to_owned()));
        app.update(special(KeyCode::Esc));

        let Mode::FileList(list) = app.mode() else {
            panic!("expected file list");
        };
        assert_eq!(list.filtered_file_indices, vec![0, 1]);
        assert_eq!(list.selected_file_index, 1);

        app.update(Event::DocumentUpdated {
            document: Arc::new(parse_bytes(updated)),
            eof: true,
        });
        let Mode::FileList(list) = app.mode() else {
            panic!("expected file list");
        };
        assert_eq!(list.filtered_file_indices, vec![0, 1, 2]);
        assert_eq!(list.selected_file_index, 1);
    }

    #[test]
    fn text_input_edits_graphemes_without_splitting() {
        let mut input = TextInput::default();
        input.insert("a👩‍💻b");
        input.move_left();
        input.delete_previous();
        assert_eq!(input.text(), "ab");
    }

    #[test]
    fn fuzzy_filter_preserves_document_order() {
        assert!(is_subsequence("sr", "src/main.rs"));
        assert!(!is_subsequence("rsx", "src/main.rs"));
    }

    #[test]
    fn worker_failure_changes_only_status() {
        let mut app = state();
        app.go_to_row(1);
        let viewport = app.viewport_top();
        let generation = app.search().generation();

        app.update(Event::WorkerFailed {
            message: "search worker failed".to_owned(),
        });

        assert_eq!(app.status(), "search worker failed");
        assert_eq!(app.viewport_top(), viewport);
        assert_eq!(app.search().generation(), generation);
        assert_eq!(app.terminal_size(), (20, 7));
        assert_eq!(app.tab_width(), 4);
        assert!(app.is_eof());
    }

    #[test]
    fn eof_resolves_pending_line_and_follow_intents() {
        let mut app = state();
        app.eof = false;
        app.update(key('9'));
        app.update(key('g'));
        assert_eq!(app.pending_line, Some(9));

        let document = Arc::clone(&app.document);
        app.update(Event::DocumentUpdated {
            document,
            eof: true,
        });
        assert_eq!(app.pending_line, None);
        assert_eq!(app.status(), "input line 9 is past EOF");

        app.eof = false;
        app.update(special(KeyCode::End));
        assert!(app.is_following());
        let document = Arc::clone(&app.document);
        app.update(Event::DocumentUpdated {
            document,
            eof: true,
        });
        assert!(!app.is_following());
        assert_eq!(app.status(), "end of input");
    }

    #[test]
    fn unknown_body_key_sets_status_without_moving() {
        let mut app = state();
        app.update(key('z'));
        assert_eq!(app.status(), "unknown key");
        assert_eq!(app.viewport_top(), 0);
    }
}
