//! Interactive pager の一方向 state machine。
//!
//! Terminal event、streaming document snapshot、background search resultを `Event` として受け、
//! state変更と外部副作用 `Effect` を分離する。Viewportはdocument全体のvisual-row indexを作らず、
//! top rowをstable [`VisualAnchor`]で保持して画面分だけlayoutする。Rendererは事前構築済みrowsと公開
//! getterだけを読み、raw store、layout engine、worker channelへ触れない。Search resultはquery
//! generationとdocument generationの両方がcurrentの場合だけ適用する。

use std::num::NonZeroUsize;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;

use crate::ansi::escape_path_bytes;
use crate::bounded_text::{BoundedDocumentTextReader, BoundedTextError, BoundedTextRead};
use crate::cache::CacheUsage;
use crate::cli::WhitespaceChoice;
use crate::direct::{LayoutError, VisualRow};
use crate::document::{DocumentError, DocumentLineReader, DocumentLineRef, DocumentSnapshot};
use crate::large_line::LargeLineError;
use crate::model::{FileId, LineId};
use crate::search::{
    CompiledSearch, IncrementalQueryStatus, RepeatCommand, ScanOutcome, ScanRequest, ScanResult,
    SearchAnchor, SearchDirection, SearchGeneration, SearchLine, SearchLineRead, SearchSession,
    SearchSource, scan_source,
};
use crate::text::MAX_SEARCH_LINE_BYTES;
use crate::viewport::{
    ViewportLayout, ViewportLayoutConfig, ViewportLayoutError, ViewportLayoutStats, ViewportRow,
    ViewportRows, VisualAnchor,
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
        document: Arc<DocumentSnapshot>,
        eof: bool,
    },
    SearchCompleted {
        generation: SearchGeneration,
        document_generation: DocumentGeneration,
        result: ScanResult<LineId>,
        preview: bool,
    },
    /// Background task の panic/error を terminal loop が文字列へ変換して渡す。
    WorkerFailed {
        generation: SearchGeneration,
        document_generation: DocumentGeneration,
        preview: bool,
        message: String,
    },
    BackgroundStatus {
        message: Option<String>,
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

/// `AppState` がinstallしたdocument snapshotの世代。
///
/// Search queryの世代とは独立して進む。同じqueryを旧snapshotと新snapshotで走査した結果を区別し、
/// EOFだけが変わる更新でも旧snapshotのno-matchを新しいEOFへ適用しないためにtask/resultへ付与する。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocumentGeneration(u64);

impl DocumentGeneration {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    const fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

/// Background scanへ渡すowned request。
#[derive(Clone, Debug)]
pub struct SearchTask {
    /// Query edit/navigationごとに進み、異なる検索intentのresultを拒否するtoken。
    pub generation: SearchGeneration,
    /// このtaskへ渡すimmutable [`DocumentSnapshot`] をinstallしたstateの世代。
    pub document_generation: DocumentGeneration,
    /// Worker threadへ移動してscan中だけ共有するcompile済みregex。
    pub compiled: CompiledSearch,
    /// Direction、開始anchor、countを固定した1回分のscan範囲。
    pub request: ScanRequest,
    /// `true`はsearch input中のpreview、`false`はcommit/repeat/pending continuationを表す。
    pub preview: bool,
}

/// State構築、reflow、同期searchを継続できないfailure。
///
/// Terminal幅不足だけは[`AppState::layout_error`]へ保持してresizeを待つため、このerrorにはしない。
#[derive(Debug, Error)]
pub enum StateError {
    #[error(transparent)]
    Layout(#[from] LayoutError),
    #[error(transparent)]
    Document(#[from] DocumentError),
    #[error(transparent)]
    LargeLine(#[from] LargeLineError),
    #[error(transparent)]
    BoundedText(#[from] BoundedTextError),
    #[error("document snapshot does not contain logical line {line_id}")]
    MissingLine { line_id: LineId },
    #[error("logical line index {index} does not fit a stable u64 ID")]
    LineIndexOverflow { index: usize },
    #[error("viewport layout invariant failed: {message}")]
    ViewportInvariant { message: &'static str },
    #[error(
        "viewport cache cannot retain a {requested}-byte layout while {live} of {limit} bytes are live"
    )]
    ViewportBudgetExhausted {
        requested: usize,
        live: usize,
        limit: usize,
    },
    #[error(
        "search task belongs to document generation {task:?}, but current generation is {current:?}"
    )]
    StaleSearchDocument {
        task: DocumentGeneration,
        current: DocumentGeneration,
    },
    #[error("synchronous search was cancelled despite a fixed non-cancelling predicate")]
    UnexpectedSearchCancellation,
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
    pub origin_top: Option<VisualAnchor>,
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
    document: Arc<DocumentSnapshot>,
    document_generation: DocumentGeneration,
    viewport_layout: ViewportLayout,
    viewport_anchor: Option<VisualAnchor>,
    viewport_rows: ViewportRows,
    /// `Some`になるのはresizeで回復できる[`LayoutError::TooNarrow`]だけ。
    layout_error: Option<LayoutError>,
    width: u16,
    height: u16,
    tab_width: usize,
    mode: Mode,
    count_prefix: String,
    whitespace: WhitespaceChoice,
    follow: bool,
    eof: bool,
    pending_line: Option<usize>,
    search: SearchSession<LineId>,
    /// Workerへ最後に発行し、同じtask identityの完了または失敗をまだ受理していないrequest。
    /// Document更新時はquery generationがcurrentのcommitted requestだけを新snapshotへ再発行する。
    in_flight_search: Option<SearchTask>,
    status: String,
    background_status: Option<String>,
}

impl AppState {
    /// Initial snapshotの先頭viewportだけをlayoutしてbody modeを作る。
    ///
    /// 幅不足は空layoutと`layout_error`を持つ成功値となる。無効なtab幅、raw/table decode、snapshot内の
    /// line欠落はstateを構築せず[`StateError`]を返す。
    pub fn new(
        document: Arc<DocumentSnapshot>,
        width: u16,
        height: u16,
        tab_width: usize,
        whitespace: WhitespaceChoice,
        eof: bool,
    ) -> Result<Self, StateError> {
        let mut viewport_layout = ViewportLayout::new();
        let mut prepared = prepare_viewport(
            &mut viewport_layout,
            &document,
            None,
            width,
            height,
            tab_width,
            false,
            true,
        )?;
        viewport_layout
            .finish_preparation(&mut prepared.rows)
            .map_err(map_viewport_error)?;
        Ok(Self {
            document,
            document_generation: DocumentGeneration::default(),
            viewport_layout,
            viewport_anchor: prepared.anchor,
            viewport_rows: prepared.rows,
            layout_error: prepared.layout_error,
            width,
            height,
            tab_width,
            mode: Mode::Body,
            count_prefix: String::new(),
            whitespace,
            follow: false,
            eof,
            pending_line: None,
            search: SearchSession::default(),
            in_flight_search: None,
            status: String::new(),
            background_status: None,
        })
    }

    #[must_use]
    pub fn document(&self) -> &DocumentSnapshot {
        &self.document
    }

    /// Background workerへ現在snapshotを渡す。Raw/table/structureをcloneせずArcの参照countだけを増やす。
    #[must_use]
    pub fn document_snapshot(&self) -> Arc<DocumentSnapshot> {
        Arc::clone(&self.document)
    }

    #[must_use]
    pub const fn document_generation(&self) -> DocumentGeneration {
        self.document_generation
    }

    #[must_use]
    pub const fn layout_error(&self) -> Option<&LayoutError> {
        self.layout_error.as_ref()
    }

    /// Draw前にlayout済みのbody rows。件数は[`Self::body_height`]以下に制限される。
    #[must_use]
    pub fn visible_rows(&self) -> &[ViewportRow] {
        &self.viewport_rows.rows
    }

    /// Current top rowのdurable anchor。Empty documentでは`None`。
    #[must_use]
    pub const fn top_anchor(&self) -> Option<VisualAnchor> {
        self.viewport_anchor
    }

    /// Sticky headerとpriority decorationが参照するbody先頭row。
    #[must_use]
    pub fn current_row(&self) -> Option<&VisualRow> {
        self.viewport_rows.rows.first().map(ViewportRow::row)
    }

    /// Lazy layoutのdecode/cache counter。受入testとdiagnosticだけが参照する。
    #[must_use]
    pub fn viewport_layout_stats(&self) -> ViewportLayoutStats {
        self.viewport_layout.stats()
    }

    /// Wrap cacheのresident/live weightと上限。
    #[must_use]
    pub fn viewport_cache_usage(&self) -> CacheUsage {
        self.viewport_layout.cache_usage()
    }

    /// Current frameのstreaming row windowが占める保守的byte数。
    #[must_use]
    pub const fn viewport_streaming_bytes(&self) -> usize {
        self.viewport_rows.streaming_bytes()
    }

    /// Current frameがpinするeager layoutまたはstreamed windowの保守的byte数。
    #[must_use]
    pub const fn viewport_visible_bytes(&self) -> usize {
        self.viewport_rows.visible_bytes()
    }

    /// Eager cacheのexternal pinとstreaming frameを合わせた現在のviewport使用量。
    #[must_use]
    pub fn viewport_live_bytes(&self) -> usize {
        let live = self
            .viewport_layout
            .cache_usage()
            .live_bytes
            .saturating_add(self.viewport_rows.streaming_bytes());
        debug_assert!(live <= crate::derived_memory::VIEWPORT_MEMORY_BYTES);
        live
    }

    /// Visible rowがlogical offsetを描く先頭fragmentかを、store I/Oなしで返す。
    ///
    /// Frame先頭がsplit tabの途中ならtop anchorのordinalで直前fragmentを検出する。それ以外は
    /// 同じframe内の先行rowだけを調べるため、rendererがline全体のlayoutを要求しない。
    #[must_use]
    pub fn visible_row_is_primary_for_offset(&self, row_index: usize, offset: usize) -> bool {
        let Some(row) = self.viewport_rows.rows.get(row_index) else {
            return false;
        };
        if !row_contains_offset(row, offset) {
            return false;
        }
        if self.viewport_rows.rows[..row_index].iter().any(|previous| {
            previous.line_id == row.line_id && row_contains_offset(previous, offset)
        }) {
            return false;
        }
        !(row_index == 0
            && self.viewport_anchor.is_some_and(|anchor| {
                anchor.line_id == row.line_id
                    && anchor.byte_offset == offset
                    && anchor.fragment_ordinal > 0
            }))
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
    pub fn background_status(&self) -> Option<&str> {
        self.background_status.as_deref()
    }

    #[must_use]
    pub const fn search(&self) -> &SearchSession<LineId> {
        &self.search
    }

    #[must_use]
    pub fn body_height(&self) -> usize {
        usize::from(self.height).saturating_sub(2)
    }

    /// Eventを適用し、main loopが実行するeffectを返す。
    ///
    /// Resizeとdocument更新は新layoutを先に構築する。Decode/I/O failureではdocument、layout、terminal
    /// sizeを更新しないため、callerはerrorを報告して既存stateを破棄できる。
    pub fn update(&mut self, event: Event) -> Result<Vec<Effect>, StateError> {
        let mut effects = match event {
            Event::Key(key) => self.handle_key(key)?,
            Event::Paste(text) => self.handle_paste(&text)?,
            Event::Resize { width, height } => {
                self.resize(width, height)?;
                vec![Effect::Redraw]
            }
            Event::DocumentUpdated { document, eof } => self.replace_document(document, eof)?,
            Event::SearchCompleted {
                generation,
                document_generation,
                result,
                preview,
            } => self.apply_search_result(generation, document_generation, result, preview)?,
            Event::WorkerFailed {
                generation,
                document_generation,
                preview,
                message,
            } => self.apply_worker_failure(generation, document_generation, preview, message),
            Event::BackgroundStatus { message } => {
                self.background_status = message;
                Vec::new()
            }
        };
        if effects.is_empty() {
            effects.push(Effect::Redraw);
        }
        Ok(effects)
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<Vec<Effect>, StateError> {
        if matches!(key.kind, KeyEventKind::Release) {
            return Ok(Vec::new());
        }
        if is_ctrl(&key, 'c') {
            return Ok(vec![Effect::Quit(130)]);
        }
        if is_ctrl(&key, 'z') {
            return Ok(vec![Effect::Suspend]);
        }
        if is_ctrl(&key, 'l') {
            return Ok(vec![Effect::Repaint]);
        }

        match &self.mode {
            Mode::Search(_) => self.handle_search_key(key),
            Mode::Help { .. } => Ok(self.handle_help_key(key)),
            Mode::FileList(_) => self.handle_file_list_key(key),
            Mode::Body => self.handle_body_key(key),
        }
    }

    fn handle_body_key(&mut self, key: KeyEvent) -> Result<Vec<Effect>, StateError> {
        if let KeyCode::Char(digit) = key.code
            && key.modifiers.is_empty()
            && digit.is_ascii_digit()
        {
            self.count_prefix.push(digit);
            self.status = format!("count: {}", self.count_prefix);
            return Ok(Vec::new());
        }
        if key.code == KeyCode::Esc {
            self.count_prefix.clear();
            self.pending_line = None;
            self.search.begin_navigation_intent();
            self.status = "cancelled".to_owned();
            return Ok(Vec::new());
        }

        let count = match self.take_count() {
            Ok(count) => count,
            Err(message) => {
                self.status = message;
                return Ok(Vec::new());
            }
        };
        let counted = count.is_some();
        let amount = count.map_or(1, NonZeroUsize::get);

        if is_down_line(&key) {
            self.move_rows(true, amount)?;
        } else if is_up_line(&key) {
            self.move_rows(false, amount)?;
        } else if is_down_page(&key) {
            self.move_rows(true, self.page_size().saturating_mul(amount))?;
        } else if is_up_page(&key) {
            self.move_rows(false, self.page_size().saturating_mul(amount))?;
        } else if is_down_half_page(&key) {
            self.move_rows(true, self.half_page_size().saturating_mul(amount))?;
        } else if is_up_half_page(&key) {
            self.move_rows(false, self.half_page_size().saturating_mul(amount))?;
        } else {
            return self.handle_body_command(key, count, counted);
        }
        Ok(Vec::new())
    }

    fn handle_body_command(
        &mut self,
        key: KeyEvent,
        count: Option<NonZeroUsize>,
        counted: bool,
    ) -> Result<Vec<Effect>, StateError> {
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
            return Ok(Vec::new());
        }
        let effects = match key.code {
            KeyCode::Char('q') if key.modifiers.is_empty() => {
                if counted {
                    // 数値prefix付きquitをそのまま実行すると、誤入力をstatusで回収できない。Prefixを
                    // 消費した最初のqは本文modeに留め、prefixなしの次回qだけを終了commandにする。
                    self.status = "count discarded".to_owned();
                    Vec::new()
                } else {
                    vec![Effect::Quit(0)]
                }
            }
            KeyCode::Char('g') | KeyCode::Home => {
                if let Some(count) = count {
                    self.go_to_input_line(count.get())?;
                } else {
                    self.go_to_start()?;
                }
                Vec::new()
            }
            KeyCode::Char('G') | KeyCode::End => {
                if let Some(count) = count {
                    self.go_to_input_line(count.get())?;
                } else {
                    // Bare G は現在位置に関係なく、それ以前の pending navigation を置き換える。
                    self.pending_line = None;
                    self.search.begin_navigation_intent();
                    self.go_to_end()?;
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
                self.jump_structure(Structure::Hunk, false, count_value(count))?;
                Vec::new()
            }
            KeyCode::Char(']') => {
                self.jump_structure(Structure::Hunk, true, count_value(count))?;
                Vec::new()
            }
            KeyCode::Char('{') => {
                self.jump_structure(Structure::File, false, count_value(count))?;
                Vec::new()
            }
            KeyCode::Char('}') => {
                self.jump_structure(Structure::File, true, count_value(count))?;
                Vec::new()
            }
            KeyCode::Char('(') => {
                self.jump_structure(Structure::Commit, false, count_value(count))?;
                Vec::new()
            }
            KeyCode::Char(')') => {
                self.jump_structure(Structure::Commit, true, count_value(count))?;
                Vec::new()
            }
            KeyCode::Char('/') => self.begin_search(SearchDirection::Forward, count),
            KeyCode::Char('?') => self.begin_search(SearchDirection::Backward, count),
            KeyCode::Char('n') => self.repeat_search(RepeatCommand::Next, count)?,
            KeyCode::Char('N') => self.repeat_search(RepeatCommand::Opposite, count)?,
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
        };
        Ok(effects)
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

    fn handle_file_list_key(&mut self, key: KeyEvent) -> Result<Vec<Effect>, StateError> {
        if key.code == KeyCode::Tab {
            self.mode = Mode::Body;
            return Ok(Vec::new());
        }
        let filter_active = matches!(&self.mode, Mode::FileList(state) if state.filter.is_some());
        if filter_active {
            return self.handle_filter_key(key);
        }

        if key.code == KeyCode::Esc || key.code == KeyCode::Char('q') && is_plain_text_key(&key) {
            self.mode = Mode::Body;
            return Ok(Vec::new());
        }
        if key.code == KeyCode::Char('/') && key.modifiers.is_empty() {
            if let Mode::FileList(state) = &mut self.mode {
                state.filter = Some(TextInput::default());
            }
            return Ok(Vec::new());
        }
        if key.code == KeyCode::Enter {
            self.jump_selected_file()?;
            return Ok(Vec::new());
        }

        let page = self.body_height().saturating_sub(2).max(1);
        if let Mode::FileList(state) = &mut self.mode {
            let len = state.filtered_file_indices.len();
            if len == 0 {
                return Ok(Vec::new());
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
        Ok(Vec::new())
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> Result<Vec<Effect>, StateError> {
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
            return Ok(Vec::new());
        }
        if key.code == KeyCode::Enter {
            self.jump_selected_file()?;
            return Ok(Vec::new());
        }
        if matches!(
            key.code,
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
        ) {
            return Ok(self.move_filter_selection(key.code));
        }
        if let Mode::FileList(state) = &mut self.mode
            && let Some(filter) = &mut state.filter
        {
            edit_text_input(filter, &key);
        }
        self.recompute_file_filter();
        Ok(Vec::new())
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Result<Vec<Effect>, StateError> {
        if key.code == KeyCode::Esc {
            let origin = match &self.mode {
                Mode::Search(state) => state.origin_top,
                _ => self.viewport_anchor,
            };
            let _ = self.search.cancel_incremental();
            if let Some(anchor) = origin {
                self.install_anchor(anchor, true)?;
            }
            self.mode = Mode::Body;
            return Ok(Vec::new());
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

    fn handle_paste(&mut self, pasted: &str) -> Result<Vec<Effect>, StateError> {
        let escaped = escape_path_bytes(pasted.as_bytes());
        let effects = match &mut self.mode {
            Mode::Search(state) => {
                state.input.insert(&escaped);
                state.history_cursor = None;
                state.history_draft = None;
                return self.preview_search();
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
        };
        Ok(effects)
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
            origin_top: self.viewport_anchor,
            count: count.unwrap_or(NonZeroUsize::MIN),
            history_cursor: None,
            history_draft: None,
        });
        // SearchSession の generation 更新が committed forward search を破棄する。Ng は別 state
        // なので、検索入力を開いた時点でこちらも明示的に取り消す。
        self.pending_line = None;
        Vec::new()
    }

    fn preview_search(&mut self) -> Result<Vec<Effect>, StateError> {
        let (query, direction, count) = match &self.mode {
            Mode::Search(state) => (state.input.text().to_owned(), state.direction, state.count),
            _ => return Ok(Vec::new()),
        };
        let Some(update) = self.search.update_incremental(&query) else {
            return Ok(Vec::new());
        };
        match &update.status {
            IncrementalQueryStatus::Empty => {
                let origin = match &self.mode {
                    Mode::Search(state) => state.origin_top,
                    _ => self.viewport_anchor,
                };
                if let Some(anchor) = origin {
                    self.install_anchor(anchor, true)?;
                }
                self.status.clear();
                return Ok(Vec::new());
            }
            IncrementalQueryStatus::Invalid(error) => {
                self.status = format!("invalid regular expression: {error}");
                return Ok(Vec::new());
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
            return Ok(Vec::new());
        };
        Ok(self.issue_search(SearchTask {
            generation: update.generation,
            document_generation: self.document_generation,
            compiled,
            request: ScanRequest::new(direction, anchor, count),
            preview: true,
        }))
    }

    fn commit_search(&mut self) -> Result<Vec<Effect>, StateError> {
        let count = match &self.mode {
            Mode::Search(state) => state.count,
            _ => NonZeroUsize::MIN,
        };
        match self.search.commit_incremental() {
            Ok(committed) => {
                self.mode = Mode::Body;
                if let Some(found) = committed.preview.and_then(|preview| preview.found) {
                    self.land_on_match(&found)?;
                    return Ok(Vec::new());
                }
                Ok(self.issue_search(SearchTask {
                    generation: committed.generation,
                    document_generation: self.document_generation,
                    compiled: committed.compiled,
                    request: ScanRequest::new(committed.direction, committed.origin, count),
                    preview: false,
                }))
            }
            Err(error) => {
                self.status = error.to_string();
                Ok(Vec::new())
            }
        }
    }

    fn repeat_search(
        &mut self,
        command: RepeatCommand,
        count: Option<NonZeroUsize>,
    ) -> Result<Vec<Effect>, StateError> {
        let Some(direction) = self.search.direction_for_repeat(command) else {
            self.status = "no previous search".to_owned();
            return Ok(Vec::new());
        };
        let Some(active) = self.search.active().cloned() else {
            return Ok(Vec::new());
        };
        let current = self.search.current_match().cloned();
        let anchor = match current {
            Some(current) => self.repeat_search_anchor(&current, direction)?,
            None => self.current_search_anchor(),
        };
        self.pending_line = None;
        let generation = self.search.begin_navigation_intent();
        Ok(self.issue_search(SearchTask {
            generation,
            document_generation: self.document_generation,
            compiled: active.compiled().clone(),
            request: ScanRequest::new(direction, anchor, count.unwrap_or(NonZeroUsize::MIN)),
            preview: false,
        }))
    }

    fn apply_search_result(
        &mut self,
        generation: SearchGeneration,
        document_generation: DocumentGeneration,
        result: ScanResult<LineId>,
        preview: bool,
    ) -> Result<Vec<Effect>, StateError> {
        if document_generation != self.document_generation {
            return Ok(Vec::new());
        }
        self.clear_matching_search_task(generation, document_generation, preview);
        let found = result.found.clone();
        let applied = if preview {
            self.search.apply_incremental_preview(generation, result)
        } else {
            self.search.apply_scan_result(generation, result, self.eof)
        };
        if applied {
            if let Some(found) = found {
                self.land_on_match(&found)?;
            } else if !preview && self.search.pending_forward().is_none() {
                self.status = "pattern not found".to_owned();
            } else if !preview {
                self.status = "searching incoming data…".to_owned();
            }
        }
        Ok(Vec::new())
    }

    fn apply_worker_failure(
        &mut self,
        generation: SearchGeneration,
        document_generation: DocumentGeneration,
        preview: bool,
        message: String,
    ) -> Vec<Effect> {
        if generation != self.search.generation() || document_generation != self.document_generation
        {
            return Vec::new();
        }
        if !self.search_task_matches(generation, document_generation, preview) {
            return Vec::new();
        }
        self.clear_matching_search_task(generation, document_generation, preview);
        self.status = message;
        Vec::new()
    }

    fn issue_search(&mut self, task: SearchTask) -> Vec<Effect> {
        debug_assert_eq!(task.document_generation, self.document_generation);
        self.in_flight_search = Some(task.clone());
        vec![Effect::Search(task)]
    }

    fn clear_matching_search_task(
        &mut self,
        generation: SearchGeneration,
        document_generation: DocumentGeneration,
        preview: bool,
    ) {
        if self.search_task_matches(generation, document_generation, preview) {
            self.in_flight_search = None;
        }
    }

    fn search_task_matches(
        &self,
        generation: SearchGeneration,
        document_generation: DocumentGeneration,
        preview: bool,
    ) -> bool {
        self.in_flight_search.as_ref().is_some_and(|task| {
            task.generation == generation
                && task.document_generation == document_generation
                && task.preview == preview
        })
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

    /// Owned search taskを現在snapshotへ同期scanする。
    ///
    /// 1 readerのbufferをline間で再利用し、全lineのtext参照配列は作らない。Taskのdocument
    /// generationがcurrentでなければscanせず、raw/table decodeとsnapshotのline欠落は
    /// [`StateError`]へ変換する。
    pub fn execute_search_task(&self, task: &SearchTask) -> Result<ScanResult<LineId>, StateError> {
        if task.document_generation != self.document_generation {
            return Err(StateError::StaleSearchDocument {
                task: task.document_generation,
                current: self.document_generation,
            });
        }
        let mut source = DocumentSearchSource::new(&self.document);
        match scan_source(&task.compiled, &mut source, task.request, &|| false) {
            ScanOutcome::Completed(result) => Ok(result),
            ScanOutcome::Failed(error) => Err(error),
            // Predicateはfalse固定で、scan側が呼び出し回数を変えてもtrueにはならない。
            ScanOutcome::Cancelled => Err(StateError::UnexpectedSearchCancellation),
        }
    }

    fn replace_document(
        &mut self,
        document: Arc<DocumentSnapshot>,
        eof: bool,
    ) -> Result<Vec<Effect>, StateError> {
        let anchor = self.viewport_anchor;
        let resolved_pending = self.pending_line.and_then(|line| {
            line.checked_sub(1)
                .and_then(|index| document.line_id_at(index))
                .map(|line_id| (line, VisualAnchor::new(line_id, 0)))
        });
        let requested_anchor = resolved_pending.map_or(anchor, |(_, anchor)| Some(anchor));
        let selected_file = self.selected_file_id();
        // Decode/I/O failureでは旧snapshot、anchor、visible rowsを組のまま残す。Layout cacheの追加は
        // rendererから見えず、exact keyがdocument内容を含むため失敗後も旧frameを汚さない。
        let prepared = prepare_viewport(
            &mut self.viewport_layout,
            &document,
            requested_anchor,
            self.width,
            self.height,
            self.tab_width,
            self.follow && resolved_pending.is_none(),
            true,
        )?;
        let interrupted_search = self.in_flight_search.take();
        self.apply_prepared_viewport(prepared)?;
        self.document = document;
        self.document_generation = self.document_generation.next();
        self.eof = eof;
        self.refresh_file_list(selected_file);
        if resolved_pending.is_some() {
            self.pending_line = None;
            self.follow = false;
            self.search.begin_navigation_intent();
            self.status.clear();
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
                document_generation: self.document_generation,
                compiled: active.compiled().clone(),
                request: pending.request(),
                preview: false,
            })
        });
        if let Some(task) = pending_task {
            return Ok(self.issue_search(task));
        }
        if matches!(&self.mode, Mode::Search(_)) {
            // Document snapshot も worker result の鮮度を決める。query が同じでも generation を進め、
            // append 前 snapshot を走査中の preview が後着して viewport を戻すのを防ぐ。
            return self.preview_search();
        }
        if let Some(mut task) = interrupted_search
            .filter(|task| !task.preview && task.generation == self.search.generation())
        {
            // Query intentと開始anchorはそのままに、workerが読むsnapshotだけを交換する。旧taskの
            // resultはdocument generation不一致で拒否され、新taskがfinal EOFの判定を所有する。
            task.document_generation = self.document_generation;
            return Ok(self.issue_search(task));
        }
        Ok(Vec::new())
    }

    fn resize(&mut self, width: u16, height: u16) -> Result<(), StateError> {
        let prepared = prepare_viewport(
            &mut self.viewport_layout,
            &self.document,
            self.viewport_anchor,
            width,
            height,
            self.tab_width,
            self.follow,
            true,
        )?;
        self.apply_prepared_viewport(prepared)?;
        self.width = width;
        self.height = height;
        Ok(())
    }

    fn apply_prepared_viewport(
        &mut self,
        mut prepared: PreparedViewport,
    ) -> Result<(), StateError> {
        // Prepared visible layoutは8 MiB以下で、old frame drop後は12 MiB cacheへpromotionできる。
        // ここからのfailureはallocation/cache accounting invariantの破損なのでrecoverable resizeとは
        // 扱わず、空frameを残してmain loopを終了させる。Old frameへ戻すためにpayloadを二重保持しない。
        let old = std::mem::replace(
            &mut self.viewport_rows,
            ViewportRows {
                top: self.viewport_anchor,
                rows: Vec::new(),
                at_end: false,
                streaming_bytes: 0,
                eager_bytes: 0,
                byte_limited: false,
            },
        );
        drop(old);
        self.viewport_layout
            .finish_preparation(&mut prepared.rows)
            .map_err(map_viewport_error)?;
        self.viewport_anchor = prepared.anchor;
        self.viewport_rows = prepared.rows;
        self.layout_error = prepared.layout_error;
        Ok(())
    }

    fn current_search_anchor(&self) -> SearchAnchor {
        self.viewport_anchor
            .and_then(|anchor| {
                usize::try_from(anchor.line_id.get())
                    .ok()
                    .map(|index| SearchAnchor::at(index, anchor.byte_offset))
            })
            .unwrap_or(SearchAnchor::BeforeDocument)
    }

    /// Zero-width repeatだけ現在lineをdecodeする。Non-empty matchは保存済みbyte rangeだけで次anchorを
    /// 作れ、前行末へ戻る場合も追加で読むのはその1行だけに留める。
    fn repeat_search_anchor(
        &self,
        current: &crate::search::SearchMatch<LineId>,
        direction: SearchDirection,
    ) -> Result<SearchAnchor, StateError> {
        if !current.is_zero_width() {
            return Ok(match direction {
                SearchDirection::Forward => {
                    SearchAnchor::at(current.line_index, current.byte_range.end)
                }
                SearchDirection::Backward => {
                    SearchAnchor::at(current.line_index, current.byte_range.start)
                }
            });
        }

        let mut reader = DocumentLineReader::new();
        match direction {
            SearchDirection::Forward => {
                let line = read_document_line(&mut reader, &self.document, current.line_index)?;
                let offset = floor_scalar_boundary(line.text, current.byte_range.start);
                if let Some(next) = next_scalar_boundary(line.text, offset) {
                    Ok(SearchAnchor::at(current.line_index, next))
                } else if current.line_index.saturating_add(1) < self.document.line_count() {
                    Ok(SearchAnchor::at(current.line_index + 1, 0))
                } else {
                    Ok(SearchAnchor::AfterDocument)
                }
            }
            SearchDirection::Backward => {
                let previous = {
                    let line = read_document_line(&mut reader, &self.document, current.line_index)?;
                    let offset = floor_scalar_boundary(line.text, current.byte_range.start);
                    previous_scalar_boundary(line.text, offset)
                };
                if let Some(previous) = previous {
                    return Ok(SearchAnchor::at(current.line_index, previous));
                }
                if current.line_index == 0 {
                    return Ok(SearchAnchor::BeforeDocument);
                }
                let previous_line = current.line_index - 1;
                let line = read_document_line(&mut reader, &self.document, previous_line)?;
                Ok(SearchAnchor::at(previous_line, line.text.len()))
            }
        }
    }

    fn land_on_match(
        &mut self,
        found: &crate::search::SearchMatch<LineId>,
    ) -> Result<(), StateError> {
        self.viewport_layout.begin_preparation();
        let anchor_result = self.viewport_layout.anchor_for_byte(
            &self.document,
            found.line_id,
            found.byte_range.start,
            self.viewport_config(),
        );
        let anchor = match anchor_result {
            Ok(anchor) => anchor,
            Err(error) => {
                self.viewport_layout.cancel_preparation();
                return Err(map_viewport_error(error));
            }
        };
        // Result は current generation の完了なので、ここで generation を進めると同じ query の
        // commit/repeat と世代が表示位置に依存する。検索成功として他の navigation だけを解除する。
        let _ = self.install_anchor(anchor, true)?;
        self.follow = false;
        self.pending_line = None;
        Ok(())
    }

    fn move_rows(&mut self, forward: bool, amount: usize) -> Result<(), StateError> {
        let Some(current) = self.viewport_anchor else {
            self.status = if forward {
                if self.eof {
                    "end of input".to_owned()
                } else {
                    "end of loaded input (loading)".to_owned()
                }
            } else {
                "beginning of input".to_owned()
            };
            return Ok(());
        };
        if self.layout_error.is_some() {
            self.status = "terminal too small".to_owned();
            return Ok(());
        }
        let config = self.viewport_config();
        self.viewport_layout.begin_preparation();
        let candidate_result = if forward {
            self.viewport_layout
                .step_forward(&self.document, current, amount, config)
        } else {
            self.viewport_layout
                .step_backward(&self.document, current, amount, config)
        };
        let candidate = match candidate_result {
            Ok(candidate) => candidate,
            Err(error) => {
                self.viewport_layout.cancel_preparation();
                return Err(map_viewport_error(error));
            }
        };
        let moved = self.install_anchor(candidate, true)?;
        if moved {
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
        Ok(())
    }

    fn install_anchor(
        &mut self,
        anchor: VisualAnchor,
        clamp_bottom: bool,
    ) -> Result<bool, StateError> {
        let prepared = prepare_viewport(
            &mut self.viewport_layout,
            &self.document,
            Some(anchor),
            self.width,
            self.height,
            self.tab_width,
            false,
            clamp_bottom,
        )?;
        let changed = prepared.anchor != self.viewport_anchor;
        self.apply_prepared_viewport(prepared)?;
        Ok(changed)
    }

    fn go_to_start(&mut self) -> Result<(), StateError> {
        let prepared = prepare_viewport(
            &mut self.viewport_layout,
            &self.document,
            None,
            self.width,
            self.height,
            self.tab_width,
            false,
            false,
        )?;
        let changed = prepared.anchor != self.viewport_anchor;
        self.apply_prepared_viewport(prepared)?;
        if changed {
            self.follow = false;
            self.pending_line = None;
            self.search.begin_navigation_intent();
            self.status.clear();
        }
        Ok(())
    }

    fn go_to_end(&mut self) -> Result<(), StateError> {
        let prepared = prepare_viewport(
            &mut self.viewport_layout,
            &self.document,
            self.viewport_anchor,
            self.width,
            self.height,
            self.tab_width,
            true,
            true,
        )?;
        self.apply_prepared_viewport(prepared)?;
        Ok(())
    }

    fn go_to_input_line(&mut self, one_based: usize) -> Result<(), StateError> {
        if one_based == 0 {
            self.status = "count must be greater than zero".to_owned();
            return Ok(());
        }
        if let Some(line_id) = self.document.line_id_at(one_based - 1) {
            let _ = self.install_anchor(VisualAnchor::new(line_id, 0), true)?;
            self.follow = false;
            self.pending_line = None;
            self.search.begin_navigation_intent();
            self.status.clear();
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
        Ok(())
    }

    fn jump_structure(
        &mut self,
        structure: Structure,
        forward: bool,
        count: usize,
    ) -> Result<(), StateError> {
        let Some(current) = self.viewport_anchor else {
            return Ok(());
        };
        let target = match structure {
            Structure::Hunk => select_structure_target(
                self.document
                    .hunks()
                    .iter()
                    .map(|hunk| VisualAnchor::new(hunk.header_line, 0)),
                current,
                forward,
                count,
            ),
            Structure::File => select_structure_target(
                self.document
                    .files()
                    .iter()
                    .map(|file| VisualAnchor::new(file.span.start, 0)),
                current,
                forward,
                count,
            ),
            Structure::Commit => select_structure_target(
                self.document
                    .commits()
                    .iter()
                    .map(|commit| VisualAnchor::new(commit.span.start, 0)),
                current,
                forward,
                count,
            ),
        };
        if let Some(target) = target {
            let _ = self.install_anchor(target, true)?;
            self.follow = false;
            self.pending_line = None;
            self.search.begin_navigation_intent();
            self.status.clear();
        } else {
            self.status = if self.eof {
                "no more loaded target".to_owned()
            } else {
                "no next loaded target (loading)".to_owned()
            };
        }
        Ok(())
    }

    fn open_file_list(&mut self) {
        let active_file = self.current_row().and_then(|row| row.file_id);
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

    fn jump_selected_file(&mut self) -> Result<(), StateError> {
        let file_id: Option<FileId> = match &self.mode {
            Mode::FileList(state) => state
                .filtered_file_indices
                .get(state.selected_file_index)
                .and_then(|index| self.document.files().get(*index))
                .map(|file| file.id),
            _ => None,
        };
        if let Some(anchor) = file_id
            .and_then(|id| self.document.file(id))
            .map(|file| VisualAnchor::new(file.span.start, 0))
        {
            self.mode = Mode::Body;
            let _ = self.install_anchor(anchor, true)?;
            self.follow = false;
            self.pending_line = None;
            self.search.begin_navigation_intent();
            self.status.clear();
        }
        Ok(())
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

    fn page_size(&self) -> usize {
        self.body_height().saturating_sub(1).max(1)
    }

    fn half_page_size(&self) -> usize {
        self.body_height().div_ceil(2).max(1)
    }

    fn viewport_config(&self) -> ViewportLayoutConfig {
        ViewportLayoutConfig::new(usize::from(self.width), self.tab_width)
    }
}

struct PreparedViewport {
    anchor: Option<VisualAnchor>,
    rows: ViewportRows,
    /// `Some`はterminal幅だけが不足した状態。Decode/I/Oと他のlayout failureはこの値へ入れず返す。
    layout_error: Option<LayoutError>,
}

#[allow(clippy::too_many_arguments)]
fn prepare_viewport(
    viewport: &mut ViewportLayout,
    document: &DocumentSnapshot,
    requested_anchor: Option<VisualAnchor>,
    width: u16,
    height: u16,
    tab_width: usize,
    follow: bool,
    clamp_bottom: bool,
) -> Result<PreparedViewport, StateError> {
    if tab_width == 0 {
        return Err(StateError::Layout(LayoutError::InvalidTabWidth {
            actual: tab_width,
        }));
    }
    viewport.begin_preparation();
    let body_height = usize::from(height).saturating_sub(2);
    let config = ViewportLayoutConfig::new(usize::from(width), tab_width);
    let fallback_anchor = requested_anchor
        .filter(|anchor| document.contains_line(anchor.line_id))
        .or_else(|| {
            document
                .line_id_at(0)
                .map(|line_id| VisualAnchor::new(line_id, 0))
        });

    let mut build = || -> Result<ViewportRows, ViewportLayoutError> {
        let top = if follow {
            viewport.bottom_aligned_top(document, body_height, config)?
        } else {
            fallback_anchor
        };
        let mut rows = viewport.collect(document, top, body_height, config)?;
        if clamp_bottom
            && rows.at_end
            && rows.rows.len() < body_height
            && let Some(bottom) = viewport.bottom_aligned_top(document, body_height, config)?
        {
            rows = viewport.collect(document, Some(bottom), body_height, config)?;
        }
        Ok(rows)
    };

    match build() {
        Ok(rows) => Ok(PreparedViewport {
            anchor: rows.top,
            rows,
            layout_error: None,
        }),
        Err(ViewportLayoutError::Layout(error @ LayoutError::TooNarrow { .. })) => {
            Ok(PreparedViewport {
                anchor: fallback_anchor,
                rows: ViewportRows {
                    top: fallback_anchor,
                    rows: Vec::new(),
                    at_end: document.is_empty(),
                    streaming_bytes: 0,
                    eager_bytes: 0,
                    byte_limited: false,
                },
                layout_error: Some(error),
            })
        }
        Err(error) => {
            viewport.cancel_preparation();
            Err(map_viewport_error(error))
        }
    }
}

fn map_viewport_error(error: ViewportLayoutError) -> StateError {
    match error {
        ViewportLayoutError::Document(error) => StateError::Document(error),
        ViewportLayoutError::Layout(error) => StateError::Layout(error),
        ViewportLayoutError::LargeLine(error) => StateError::LargeLine(error),
        ViewportLayoutError::MissingLine { line_id }
        | ViewportLayoutError::MissingVisualRows { line_id } => StateError::MissingLine { line_id },
        ViewportLayoutError::UnavailableLineIndex { index } => {
            StateError::LineIndexOverflow { index }
        }
        ViewportLayoutError::CacheEntryUnavailable => StateError::ViewportInvariant {
            message: "inserted cache entry is unavailable",
        },
        ViewportLayoutError::CacheBudgetExhausted {
            requested,
            live,
            limit,
        } => StateError::ViewportBudgetExhausted {
            requested,
            live,
            limit,
        },
    }
}

fn select_structure_target<I>(
    targets: I,
    current: VisualAnchor,
    forward: bool,
    count: usize,
) -> Option<VisualAnchor>
where
    I: DoubleEndedIterator<Item = VisualAnchor>,
{
    if forward {
        targets
            .filter(|target| anchor_order(*target, current).is_gt())
            .nth(count.saturating_sub(1))
    } else {
        targets
            .rev()
            .filter(|target| anchor_order(*target, current).is_lt())
            .nth(count.saturating_sub(1))
    }
}

fn anchor_order(left: VisualAnchor, right: VisualAnchor) -> std::cmp::Ordering {
    (left.line_id, left.byte_offset, left.fragment_ordinal).cmp(&(
        right.line_id,
        right.byte_offset,
        right.fragment_ordinal,
    ))
}

fn row_contains_offset(row: &VisualRow, offset: usize) -> bool {
    row.content_range.contains(&offset)
        || (row.content_range.is_empty() && row.content_range.start == offset)
}

/// 同期search 1回で1 decoderを再利用するsnapshot accessor。
struct DocumentSearchSource<'document> {
    document: &'document DocumentSnapshot,
    bounded_reader: BoundedDocumentTextReader,
}

impl<'document> DocumentSearchSource<'document> {
    fn new(document: &'document DocumentSnapshot) -> Self {
        Self {
            document,
            bounded_reader: BoundedDocumentTextReader::new(),
        }
    }
}

impl SearchSource<LineId> for DocumentSearchSource<'_> {
    type Error = StateError;

    fn line_count(&self) -> usize {
        self.document.line_count()
    }

    fn read_line(
        &mut self,
        index: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<SearchLineRead<'_, LineId>, Self::Error> {
        let raw = u64::try_from(index).map_err(|_| StateError::LineIndexOverflow { index })?;
        let line_id = LineId::new(raw);
        if index >= self.document.line_count() {
            return Err(StateError::MissingLine { line_id });
        }
        match self
            .bounded_reader
            .read(self.document, line_id, MAX_SEARCH_LINE_BYTES, cancelled)?
        {
            BoundedTextRead::Available { metadata, text } => Ok(SearchLineRead::Searchable(
                SearchLine::new(metadata.id, text),
            )),
            BoundedTextRead::Oversized { metadata } => Ok(SearchLineRead::Oversized {
                line_id: metadata.id,
            }),
            BoundedTextRead::Cancelled => Ok(SearchLineRead::Cancelled),
        }
    }
}

fn read_document_line<'reader>(
    reader: &'reader mut DocumentLineReader,
    document: &DocumentSnapshot,
    index: usize,
) -> Result<DocumentLineRef<'reader>, StateError> {
    let line_id = match document.line_id_at(index) {
        Some(line_id) => line_id,
        None => {
            let line_id = u64::try_from(index)
                .map(LineId::new)
                .map_err(|_| StateError::LineIndexOverflow { index })?;
            return Err(StateError::MissingLine { line_id });
        }
    };
    reader
        .read(document, line_id)?
        .ok_or(StateError::MissingLine { line_id })
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

fn floor_scalar_boundary(text: &str, offset: usize) -> usize {
    let mut boundary = offset.min(text.len());
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

fn next_scalar_boundary(text: &str, offset: usize) -> Option<usize> {
    if offset >= text.len() {
        return None;
    }
    text.get(offset..)
        .and_then(|suffix| suffix.chars().next())
        .map(|character| offset + character.len_utf8())
}

fn previous_scalar_boundary(text: &str, offset: usize) -> Option<usize> {
    text.get(..offset)
        .and_then(|prefix| prefix.char_indices().next_back())
        .map(|(index, _)| index)
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
    use crate::ansi::AnsiStripper;
    use crate::document::{AppendDocumentLine, DocumentBuilder};
    use crate::document_store::LineStoreError;
    use crate::input::AppendStore;
    use crate::model::LineKind;
    use crate::parser::{GitParser, parse_bytes};
    use crate::search::compile_smart_case;
    use crate::storage::StructuralStorage;

    fn document_snapshot_with_display_lengths(
        input: &[u8],
        display_lengths: &[usize],
    ) -> DocumentSnapshot {
        let raw = AppendStore::new();
        raw.append(input).unwrap();
        raw.seal().unwrap();
        let storage = StructuralStorage::new().unwrap();
        let mut builder = DocumentBuilder::new(raw, &storage).unwrap();
        let mut tracker = builder.start_line_tracker().unwrap();
        let mut stripper = AnsiStripper::default();
        let mut spans = Vec::new();
        stripper
            .push_indexed(0, input, |offset, byte| {
                if let Some(span) = tracker.observe(offset, byte).unwrap() {
                    spans.push(span);
                }
            })
            .unwrap();
        stripper.finish_indexed(|offset, byte| {
            if let Some(span) = tracker.observe(offset, byte).unwrap() {
                spans.push(span);
            }
        });
        if let Some(span) = tracker
            .seal_eof(u64::try_from(input.len()).unwrap())
            .unwrap()
        {
            spans.push(span);
        }
        assert_eq!(spans.len(), display_lengths.len());
        for (span, &display_len) in spans.into_iter().zip(display_lengths) {
            builder
                .append_line(
                    span,
                    AppendDocumentLine {
                        display_len,
                        kind: LineKind::Plain,
                        commit_id: None,
                        file_id: None,
                        hunk_id: None,
                        old_line_number: None,
                        new_line_number: None,
                    },
                )
                .unwrap();
        }
        builder.mark_complete();
        builder.snapshot().unwrap()
    }

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
        )
        .unwrap();
        AppState::new(Arc::new(document), 20, 7, 4, WhitespaceChoice::Ignore, true).unwrap()
    }

    fn streaming_parser() -> (AppendStore, GitParser) {
        let raw = AppendStore::new();
        let storage = StructuralStorage::new().unwrap();
        let parser = GitParser::new(raw.clone(), &storage).unwrap();
        (raw, parser)
    }

    fn push(raw: &AppendStore, parser: &mut GitParser, bytes: &[u8]) {
        let start = raw.append(bytes).unwrap();
        let _ = parser.push(start, bytes).unwrap();
    }

    #[test]
    fn e_and_y_match_j_and_k() {
        let mut app = state();
        let origin = app.top_anchor();
        app.update(key('e')).unwrap();
        assert_ne!(app.top_anchor(), origin);
        app.update(key('y')).unwrap();
        assert_eq!(app.top_anchor(), origin);
    }

    #[test]
    fn count_repeats_visual_rows() {
        let mut app = state();
        let mut repeated = state();
        for _ in 0..3 {
            repeated.update(key('j')).unwrap();
        }
        app.update(key('3')).unwrap();
        app.update(key('j')).unwrap();
        assert_eq!(app.top_anchor(), repeated.top_anchor());
    }

    #[test]
    fn large_document_construct_append_and_resize_touch_only_viewport_lines() {
        let (raw, mut parser) = streaming_parser();
        let mut input = String::new();
        for index in 0..20_000 {
            input.push_str(&format!("line-{index:05}\n"));
        }
        push(&raw, &mut parser, input.as_bytes());
        let initial = Arc::new(parser.snapshot().unwrap());
        let mut app = AppState::new(initial, 80, 8, 4, WhitespaceChoice::Ignore, false).unwrap();
        let constructed = app.viewport_layout_stats();
        assert_eq!(app.visible_rows().len(), app.body_height());
        assert!(constructed.decoded_lines <= app.body_height() as u64);
        assert!(constructed.layout_misses <= app.body_height() as u64);

        push(&raw, &mut parser, b"tail\n");
        let before_append = app.viewport_layout_stats();
        app.update(Event::DocumentUpdated {
            document: Arc::new(parser.snapshot().unwrap()),
            eof: false,
        })
        .unwrap();
        let after_append = app.viewport_layout_stats();
        assert_eq!(after_append.decoded_lines, before_append.decoded_lines);
        assert!(after_append.cache_hits > before_append.cache_hits);

        let before_resize = app.viewport_layout_stats();
        app.update(Event::Resize {
            width: 72,
            height: 9,
        })
        .unwrap();
        let after_resize = app.viewport_layout_stats();
        assert!(
            after_resize.decoded_lines - before_resize.decoded_lines <= app.body_height() as u64
        );
        assert!(app.visible_rows().len() <= app.body_height());
    }

    #[test]
    fn resize_promotes_prepared_layouts_after_old_frame_pins_are_released() {
        let line = "x".repeat(60_000);
        let document =
            Arc::new(parse_bytes(format!("{line}\n{line}\n{line}\n").as_bytes()).unwrap());
        let mut app =
            AppState::new(document, 65_000, 6, 4, WhitespaceChoice::Ignore, true).unwrap();
        let before = app.viewport_layout_stats();

        app.update(Event::Resize {
            width: 64_000,
            height: 6,
        })
        .unwrap();

        let after = app.viewport_layout_stats();
        let cache = app.viewport_cache_usage();
        assert_eq!(app.visible_rows().len(), 2);
        assert!(after.layout_misses >= before.layout_misses + 2);
        assert!(after.cache_insert_rejections >= before.cache_insert_rejections + 2);
        assert!(app.viewport_visible_bytes() <= 8 * 1024 * 1024);
        assert!(cache.live_bytes <= 12 * 1024 * 1024);
        assert!(app.viewport_live_bytes() <= crate::derived_memory::VIEWPORT_MEMORY_BYTES);
    }

    #[test]
    fn distinct_streaming_configs_fit_current_and_prepared_frame_share() {
        let mut input = "x".repeat(200_000);
        input.push('\n');
        let document = Arc::new(parse_bytes(input.as_bytes()).unwrap());
        let mut app = AppState::new(document, 40, 120, 4, WhitespaceChoice::Ignore, true).unwrap();
        let old_frame_bytes = app.viewport_streaming_bytes();
        let before = app.viewport_layout_stats();

        app.update(Event::Resize {
            width: 41,
            height: 120,
        })
        .unwrap();

        let prepared_frame_bytes = app.viewport_streaming_bytes();
        let after = app.viewport_layout_stats();
        assert!(old_frame_bytes <= 4 * 1024 * 1024);
        assert!(prepared_frame_bytes <= 4 * 1024 * 1024);
        assert!(old_frame_bytes.saturating_add(prepared_frame_bytes) <= 8 * 1024 * 1024);
        assert!(after.streaming_navigators_created > before.streaming_navigators_created);
        assert_eq!(app.viewport_cache_usage().resident_bytes, 0);
    }

    #[test]
    fn eager_frame_to_streaming_frame_releases_resident_cache_on_commit() {
        let mut input = "head\n".to_owned();
        input.push_str(&"x".repeat(200_000));
        input.push('\n');
        let document = Arc::new(parse_bytes(input.as_bytes()).unwrap());
        let mut app = AppState::new(document, 40, 20, 4, WhitespaceChoice::Ignore, true).unwrap();
        let old_cache = app.viewport_cache_usage();
        assert_eq!(app.visible_rows().len(), 1);
        assert!(old_cache.resident_bytes > 0);
        assert_eq!(app.viewport_streaming_bytes(), 0);

        app.update(key('j')).unwrap();

        let streamed = app.viewport_streaming_bytes();
        assert!(streamed > 0);
        assert!(streamed <= 4 * 1024 * 1024);
        assert_eq!(app.viewport_cache_usage().resident_bytes, 0);
        assert!(
            old_cache
                .live_bytes
                .saturating_add(streamed)
                .saturating_add(4 * 1024 * 1024)
                <= crate::derived_memory::VIEWPORT_MEMORY_BYTES
        );
    }

    #[test]
    fn distant_structure_jump_does_not_layout_intervening_lines() {
        let mut input =
            String::from("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1,20000 +1,20000 @@\n");
        for _ in 0..20_000 {
            input.push_str(" same\n");
        }
        input.push_str("diff --git a/b b/b\n--- a/b\n+++ b/b\n@@ -1 +1 @@\n-old\n+new\n");
        let document = Arc::new(parse_bytes(input.as_bytes()).unwrap());
        let mut app = AppState::new(document, 80, 8, 4, WhitespaceChoice::Ignore, true).unwrap();
        let before = app.viewport_layout_stats();

        app.update(key('}')).unwrap();

        let after = app.viewport_layout_stats();
        assert!(after.decoded_lines - before.decoded_lines < 32);
        assert_eq!(
            app.current_row()
                .and_then(|row| row.file_id)
                .map(|id| id.get()),
            Some(1)
        );
    }

    #[test]
    fn state_scroll_round_trips_every_split_tab_fragment() {
        let document =
            parse_bytes(b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -0,0 +1,1 @@\n+\tZ\n").unwrap();
        let addition_index = document.line_count() - 1;
        let mut app =
            AppState::new(Arc::new(document), 11, 3, 4, WhitespaceChoice::Ignore, true).unwrap();
        app.go_to_input_line(addition_index + 1).unwrap();
        let origin = app.top_anchor().unwrap();
        let mut forward = vec![origin];
        for _ in 0..4 {
            app.update(key('j')).unwrap();
            forward.push(app.top_anchor().unwrap());
        }
        assert_eq!(
            forward
                .iter()
                .map(|anchor| (anchor.byte_offset, anchor.fragment_ordinal))
                .collect::<Vec<_>>(),
            vec![(0, 0), (1, 1), (1, 2), (1, 3), (2, 0)]
        );
        for expected in forward[..forward.len() - 1].iter().rev() {
            app.update(key('k')).unwrap();
            assert_eq!(app.top_anchor().as_ref(), Some(expected));
        }
    }

    #[test]
    fn q_is_text_while_filter_is_active() {
        let mut app = state();
        app.update(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)))
            .unwrap();
        app.update(key('/')).unwrap();
        app.update(key('q')).unwrap();
        let Mode::FileList(list) = app.mode() else {
            panic!("expected file list");
        };
        assert_eq!(list.filter.as_ref().map(TextInput::text), Some("q"));
    }

    #[test]
    fn counted_q_discards_the_prefix_before_a_later_bare_q_quits() {
        let mut app = state();

        assert!(matches!(
            app.update(key('2')).unwrap().as_slice(),
            [Effect::Redraw]
        ));
        let first = app.update(key('q')).unwrap();
        assert!(matches!(first.as_slice(), [Effect::Redraw]));
        assert_eq!(app.status(), "count discarded");
        assert!(matches!(app.mode(), Mode::Body));

        assert!(matches!(
            app.update(key('q')).unwrap().as_slice(),
            [Effect::Quit(0)]
        ));
    }

    #[test]
    fn bare_end_enables_follow_until_successful_movement() {
        let mut app = state();
        app.eof = false;
        app.update(special(KeyCode::End)).unwrap();
        assert!(app.is_following());
        app.update(key('j')).unwrap();
        assert!(app.is_following());
        app.update(key('k')).unwrap();
        assert!(!app.is_following());
    }

    #[test]
    fn huge_count_saturates_at_document_boundary() {
        let mut app = state();
        let mut end = state();
        end.go_to_end().unwrap();
        for digit in usize::MAX.to_string().chars() {
            app.update(key(digit)).unwrap();
        }
        app.update(key('j')).unwrap();
        assert_eq!(app.top_anchor(), end.top_anchor());
    }

    #[test]
    fn incremental_search_keeps_origin_and_generation_after_preview_lands() {
        let mut app = state();
        let origin = app.current_search_anchor();
        app.update(key('/')).unwrap();
        let first = search_task(&app.update(Event::Paste("three".to_owned())).unwrap());
        assert_eq!(first.request.anchor, origin);
        let result = app.execute_search_task(&first).unwrap();
        app.update(Event::SearchCompleted {
            generation: first.generation,
            document_generation: first.document_generation,
            result,
            preview: true,
        })
        .unwrap();
        assert_ne!(app.top_anchor(), Some(VisualAnchor::new(LineId::new(0), 0)));
        assert_eq!(app.search().generation(), first.generation);

        let refined = search_task(&app.update(Event::Paste("$".to_owned())).unwrap());
        assert_eq!(refined.request.anchor, origin);
    }

    #[test]
    fn search_count_survives_commit_before_preview_returns() {
        let mut app = state();
        app.update(key('2')).unwrap();
        app.update(key('/')).unwrap();
        app.update(Event::Paste("a.rs".to_owned())).unwrap();
        let committed = search_task(&app.update(special(KeyCode::Enter)).unwrap());
        assert_eq!(committed.request.count.get(), 2);
        assert!(!committed.preview);
    }

    #[test]
    fn preview_failure_cannot_replace_the_status_of_a_committed_search() {
        let mut app = state();
        app.update(key('/')).unwrap();
        let preview = search_task(&app.update(Event::Paste("three".to_owned())).unwrap());
        let committed = search_task(&app.update(special(KeyCode::Enter)).unwrap());
        assert_eq!(preview.generation, committed.generation);
        assert_eq!(preview.document_generation, committed.document_generation);
        assert!(preview.preview);
        assert!(!committed.preview);

        app.update(Event::WorkerFailed {
            generation: preview.generation,
            document_generation: preview.document_generation,
            preview: true,
            message: "stale preview failure".to_owned(),
        })
        .unwrap();

        assert_ne!(app.status(), "stale preview failure");
        assert!(
            app.in_flight_search
                .as_ref()
                .is_some_and(|task| !task.preview)
        );
    }

    #[test]
    fn committed_search_rejects_an_old_no_match_after_the_final_snapshot_arrives() {
        let mut app = AppState::new(
            Arc::new(parse_bytes(b"alpha\n").unwrap()),
            20,
            7,
            4,
            WhitespaceChoice::Ignore,
            false,
        )
        .unwrap();
        app.update(key('/')).unwrap();
        let _preview = search_task(&app.update(Event::Paste("needle".to_owned())).unwrap());
        let committed = search_task(&app.update(special(KeyCode::Enter)).unwrap());
        let stale_no_match = app.execute_search_task(&committed).unwrap();
        assert!(stale_no_match.found.is_none());

        let effects = app
            .update(Event::DocumentUpdated {
                document: Arc::new(parse_bytes(b"alpha\nneedle\n").unwrap()),
                eof: true,
            })
            .unwrap();
        let replacement = search_task(&effects);
        assert_eq!(replacement.generation, committed.generation);
        assert_ne!(
            replacement.document_generation,
            committed.document_generation
        );
        assert_eq!(replacement.request, committed.request);
        assert!(!replacement.preview);

        app.update(Event::SearchCompleted {
            generation: committed.generation,
            document_generation: committed.document_generation,
            result: stale_no_match,
            preview: false,
        })
        .unwrap();
        assert_ne!(app.status(), "pattern not found");

        let fresh_result = app.execute_search_task(&replacement).unwrap();
        assert_eq!(
            fresh_result.found.as_ref().map(|found| found.line_index),
            Some(1)
        );
        app.update(Event::SearchCompleted {
            generation: replacement.generation,
            document_generation: replacement.document_generation,
            result: fresh_result,
            preview: false,
        })
        .unwrap();
        assert_eq!(
            app.search().current_match().map(|found| found.line_index),
            Some(1)
        );
    }

    #[test]
    fn clearing_incremental_query_restores_search_origin() {
        let mut app = state();
        let origin_top = app.top_anchor();
        app.update(key('/')).unwrap();
        let task = search_task(&app.update(Event::Paste("three".to_owned())).unwrap());
        let result = app.execute_search_task(&task).unwrap();
        app.update(Event::SearchCompleted {
            generation: task.generation,
            document_generation: task.document_generation,
            result,
            preview: true,
        })
        .unwrap();
        assert_ne!(app.top_anchor(), origin_top);

        app.update(Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )))
        .unwrap();
        assert_eq!(app.top_anchor(), origin_top);
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
        app.update(key('/')).unwrap();
        app.update(Event::Paste("draft".to_owned())).unwrap();

        app.update(special(KeyCode::Up)).unwrap();
        assert_search_input(&app, "beta");
        app.update(special(KeyCode::Up)).unwrap();
        assert_search_input(&app, "alpha");
        app.update(special(KeyCode::Down)).unwrap();
        assert_search_input(&app, "beta");
        app.update(special(KeyCode::Down)).unwrap();
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
        app.move_rows(true, 2).unwrap();
        let anchor = app.top_anchor();

        app.update(Event::Resize {
            width: 0,
            height: 7,
        })
        .unwrap();
        assert!(matches!(
            app.layout_error(),
            Some(LayoutError::TooNarrow { .. })
        ));
        app.update(Event::Resize {
            width: 20,
            height: 7,
        })
        .unwrap();

        assert_eq!(app.layout_error(), None);
        assert_eq!(app.top_anchor(), anchor);
    }

    #[test]
    fn invalid_layout_parameters_remain_fatal_at_the_state_boundary() {
        let document = Arc::new(parse_bytes(b"plain\n").unwrap());
        assert!(matches!(
            AppState::new(document, 20, 7, 0, WhitespaceChoice::Ignore, true,),
            Err(StateError::Layout(LayoutError::InvalidTabWidth {
                actual: 0
            }))
        ));
    }

    #[test]
    fn document_decode_failures_remain_fatal_at_the_state_boundary() {
        let document = Arc::new(document_snapshot_with_display_lengths(b"plain\n", &[99]));
        assert!(matches!(
            AppState::new(document, 20, 7, 4, WhitespaceChoice::Ignore, true),
            Err(StateError::Document(DocumentError::LineStore(
                LineStoreError::DisplayLengthMismatch { line_id: 0, .. }
            )))
        ));
    }

    #[test]
    fn failed_document_update_preserves_the_previous_snapshot_and_viewport() {
        let mut app = state();
        let previous_document = Arc::clone(&app.document);
        let previous_anchor = app.viewport_anchor;
        let previous_rows = app.viewport_rows.clone();
        let invalid = Arc::new(document_snapshot_with_display_lengths(b"plain\n", &[99]));

        let error = app
            .update(Event::DocumentUpdated {
                document: invalid,
                eof: false,
            })
            .unwrap_err();

        assert!(matches!(
            error,
            StateError::Document(DocumentError::LineStore(
                LineStoreError::DisplayLengthMismatch { line_id: 0, .. }
            ))
        ));
        assert!(Arc::ptr_eq(&app.document, &previous_document));
        assert_eq!(app.viewport_anchor, previous_anchor);
        assert_eq!(app.viewport_rows, previous_rows);
        assert!(app.is_eof());
    }

    #[test]
    fn resize_keeps_following_at_reflowed_tail() {
        let mut app = state();
        app.eof = false;
        app.update(special(KeyCode::End)).unwrap();
        app.update(Event::Resize {
            width: 18,
            height: 6,
        })
        .unwrap();
        assert!(app.is_following());
        assert!(app.viewport_rows.at_end);
    }

    #[test]
    fn clearing_filter_and_stream_update_preserve_selected_file_identity() {
        let initial = b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-a\n+A\ndiff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -1 +1 @@\n-b\n+B\n";
        let updated = b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-a\n+A\ndiff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -1 +1 @@\n-b\n+B\ndiff --git a/c.rs b/c.rs\n--- a/c.rs\n+++ b/c.rs\n@@ -1 +1 @@\n-c\n+C\n";
        let mut app = AppState::new(
            Arc::new(parse_bytes(initial).unwrap()),
            40,
            10,
            4,
            WhitespaceChoice::Ignore,
            false,
        )
        .unwrap();
        app.update(special(KeyCode::Tab)).unwrap();
        app.update(key('j')).unwrap();
        app.update(key('/')).unwrap();
        app.update(Event::Paste("b".to_owned())).unwrap();
        app.update(special(KeyCode::Esc)).unwrap();

        let Mode::FileList(list) = app.mode() else {
            panic!("expected file list");
        };
        assert_eq!(list.filtered_file_indices, vec![0, 1]);
        assert_eq!(list.selected_file_index, 1);

        app.update(Event::DocumentUpdated {
            document: Arc::new(parse_bytes(updated).unwrap()),
            eof: true,
        })
        .unwrap();
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
        app.move_rows(true, 1).unwrap();
        let viewport = app.top_anchor();
        app.update(key('/')).unwrap();
        let task = search_task(&app.update(Event::Paste("three".to_owned())).unwrap());

        app.update(Event::WorkerFailed {
            generation: task.generation,
            document_generation: task.document_generation,
            preview: task.preview,
            message: "search worker failed".to_owned(),
        })
        .unwrap();

        assert_eq!(app.status(), "search worker failed");
        assert_eq!(app.top_anchor(), viewport);
        assert_eq!(app.search().generation(), task.generation);
        assert_eq!(app.terminal_size(), (20, 7));
        assert_eq!(app.tab_width(), 4);
        assert!(app.is_eof());
    }

    #[test]
    fn eof_resolves_pending_line_and_follow_intents() {
        let mut app = state();
        app.eof = false;
        app.update(key('9')).unwrap();
        app.update(key('g')).unwrap();
        assert_eq!(app.pending_line, Some(9));

        let document = Arc::clone(&app.document);
        app.update(Event::DocumentUpdated {
            document,
            eof: true,
        })
        .unwrap();
        assert_eq!(app.pending_line, None);
        assert_eq!(app.status(), "input line 9 is past EOF");

        app.eof = false;
        app.update(special(KeyCode::End)).unwrap();
        assert!(app.is_following());
        let document = Arc::clone(&app.document);
        app.update(Event::DocumentUpdated {
            document,
            eof: true,
        })
        .unwrap();
        assert!(!app.is_following());
        assert_eq!(app.status(), "end of input");
    }

    #[test]
    fn unknown_body_key_sets_status_without_moving() {
        let mut app = state();
        let origin = app.top_anchor();
        app.update(key('z')).unwrap();
        assert_eq!(app.status(), "unknown key");
        assert_eq!(app.top_anchor(), origin);
    }
}
