//! Interactive pager の一方向 state machine。
//!
//! Terminal event、streaming document snapshot、background search resultを `Event` として受け、
//! state変更と外部副作用 `Effect` を分離する。Viewportはdocument全体のvisual-row indexを作らず、
//! top rowをstable [`VisualAnchor`]で保持して画面分だけlayoutする。Rendererは事前構築済みrowsと公開
//! getterだけを読み、raw store、layout engine、worker channelへ触れない。Search resultはquery
//! generationとdocument generationの両方がcurrentの場合だけ適用する。

use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;

use crate::ansi::{
    EscapeError, LimitedPathEscape, escape_path_bytes, escape_path_bytes_with_limit,
};
use crate::bounded_text::{BoundedDocumentTextReader, BoundedTextError, BoundedTextRead};
use crate::cache::CacheUsage;
use crate::cli::WhitespaceChoice;
use crate::direct::{LayoutError, VisualRow};
use crate::document::{DocumentError, DocumentLineReader, DocumentLineRef, DocumentSnapshot};
use crate::large_line::LargeLineError;
use crate::model::{CommitId, FileId, LineId};
use crate::search::{
    CompiledSearch, IncrementalQueryStatus, RepeatCommand, ScanOutcome, ScanRequest, ScanResult,
    SearchAnchor, SearchDirection, SearchGeneration, SearchLine, SearchLineRead, SearchSession,
    SearchSource, scan_source,
};
use crate::structure_store::{
    StoredCommitMetadata, StructureMetadataReader, StructureSnapshot, StructureStoreError,
};
use crate::text::{MAX_SEARCH_LINE_BYTES, MAX_SYNTAX_LINE_BYTES};
use crate::viewport::{
    ViewportLayout, ViewportLayoutConfig, ViewportLayoutError, ViewportLayoutStats, ViewportRow,
    ViewportRows, VisualAnchor,
};

const HELP_LINE_COUNT: usize = 42;

/// Background filter resultが保持できるstable file ID数。
///
/// File件数そのものは`u64`で数え、workerとoverlayはselection周辺のこの件数だけを所有する。
/// 500万fileでも全ID配列をmain/worker間で複製しないためのhard capである。
pub const FILE_FILTER_WINDOW_CAPACITY: usize = 5_000;

/// File-filter inputがworkerへ渡せるescaped UTF-8 byte数。
///
/// 1 MiBの旧pathと新path、およびUTF-8の`" → "` 5 bytesを合わせた値に固定する。Stateは
/// TextInputを変更する前に検査し、runtimeも境界を越えた直接taskを拒否する。
pub const MAX_FILE_FILTER_QUERY_BYTES: usize = MAX_SYNTAX_LINE_BYTES * 2 + " → ".len();

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
    FileFilterCompleted {
        generation: FileFilterGeneration,
        document_generation: DocumentGeneration,
        total_matches: u64,
        selected_rank: Option<u64>,
        start_rank: u64,
        file_ids: Vec<FileId>,
    },
    /// Background task の panic/error を terminal loop が文字列へ変換して渡す。
    WorkerFailed {
        generation: SearchGeneration,
        document_generation: DocumentGeneration,
        preview: bool,
        message: String,
    },
    FileFilterFailed {
        generation: FileFilterGeneration,
        document_generation: DocumentGeneration,
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
    FileFilter(FileFilterTask),
    /// Current search worker requestをreplacementなしで取り消す。
    CancelSearch,
    /// Current file-filter worker requestをreplacementなしで取り消す。
    CancelFileFilter,
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

/// File filter input/window requestの世代。
///
/// Query edit、document更新、window外navigationで進める。Worker resultはこの値と
/// [`DocumentGeneration`]の両方がcurrentの場合だけ適用する。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileFilterGeneration(u64);

impl FileFilterGeneration {
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

/// Background path filterへ渡すowned request。
#[derive(Clone, Debug)]
pub struct FileFilterTask {
    /// Queryまたはtarget windowを識別し、古いresultを拒否するtoken。
    pub generation: FileFilterGeneration,
    /// Workerが読むimmutable document snapshotのstate世代。
    pub document_generation: DocumentGeneration,
    /// Decode後pathへcase-insensitive subsequenceとして適用するquery。
    pub query: String,
    /// Query変更時に可能なら維持するstable file identity。
    pub preferred_file: Option<FileId>,
    /// Preferred fileがmatchしない場合、またはwindow外へ移動する場合のmatch rank。
    pub requested_rank: Option<u64>,
}

/// State構築、reflow、同期searchを継続できないfailure。
///
/// Terminal幅不足だけは[`AppState::layout_error`]へ保持してresizeを待つため、このerrorにはしない。
#[derive(Debug, Error)]
pub enum StateError {
    #[error(transparent)]
    Escape(#[from] EscapeError),
    #[error(transparent)]
    Layout(#[from] LayoutError),
    #[error(transparent)]
    Document(#[from] DocumentError),
    #[error(transparent)]
    Structure(#[from] StructureStoreError),
    #[error(transparent)]
    LargeLine(#[from] LargeLineError),
    #[error(transparent)]
    BoundedText(#[from] BoundedTextError),
    #[error("document snapshot does not contain logical line {line_id}")]
    MissingLine { line_id: LineId },
    #[error("logical line index {index} does not fit a stable u64 ID")]
    LineIndexOverflow { index: usize },
    #[error("structure navigation count {count} does not fit a stable u64 ordinal")]
    StructureCountOverflow { count: usize },
    #[error("could not allocate {requested} bytes for a file-list ID window")]
    FileWindowAllocation { requested: usize },
    #[error("file-filter worker returned an invalid window: {message}")]
    InvalidFileFilterWindow { message: &'static str },
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
    #[error("document update belongs to a different parser lineage")]
    UnrelatedDocumentUpdate,
    #[error("document update regressed logical lines from {previous} to {current}")]
    RegressiveDocumentLines { previous: usize, current: usize },
    #[error("document update regressed {kind} records from {previous} to {current}")]
    RegressiveDocumentStructure {
        kind: &'static str,
        previous: u64,
        current: u64,
    },
    #[error("document update changed a complete snapshot back to incomplete")]
    ReopenedCompleteDocument,
    #[error("EOF flag {eof} does not match document completion state {document_complete}")]
    DocumentCompletionMismatch { eof: bool, document_complete: bool },
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
    /// Filterなしのoverlayで選択しているvirtual item。
    ///
    /// Fileを持つcommitは各fileだけ、finalized済みでfileが0件のcommitはplaceholder 1件だけを
    /// navigation対象にする。Openかつfileがまだ0件のcommitは入力待ちなのでitemを作らない。
    pub selected_item: Option<FileListItem>,
    /// Document更新とfilter window交換をまたいで維持するfile occurrence identity。
    pub selected_file: Option<FileId>,
    /// Query result全体ではなくselection周辺だけを保持するbounded window。
    pub window: Option<FileFilterWindow>,
    pub filter: Option<TextInput>,
    /// Query/window requestを識別するcurrent generation。
    pub generation: FileFilterGeneration,
    /// Current generationのworker resultを待っている間だけ`true`。
    pub loading: bool,
}

/// Unfiltered file-listをGit順に移動するstable item identity。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileListItem {
    File(FileId),
    EmptyCommit(CommitId),
}

impl FileListItem {
    #[must_use]
    pub const fn file_id(self) -> Option<FileId> {
        match self {
            Self::File(file_id) => Some(file_id),
            Self::EmptyCommit(_) => None,
        }
    }
}

/// File-list overlayが保持するmatch rankの連続window。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileFilterWindow {
    /// Queryに一致するfile occurrence総数。Filterなしではsnapshotのvisible file数。
    pub total_matches: u64,
    /// 全match中のselection rank。Matchなしでは`None`。
    pub selected_rank: Option<u64>,
    /// `file_ids[0]`が表す全match中のrank。
    pub start_rank: u64,
    /// Git出力順のstable ID。長さは[`FILE_FILTER_WINDOW_CAPACITY`]以下。
    pub file_ids: Vec<FileId>,
}

impl FileFilterWindow {
    /// Current selectionに対応するstable ID。Window外またはmatchなしでは`None`。
    #[must_use]
    pub fn selected_file(&self) -> Option<FileId> {
        let rank = self.selected_rank?;
        let offset = rank.checked_sub(self.start_rank)?;
        usize::try_from(offset)
            .ok()
            .and_then(|index| self.file_ids.get(index))
            .copied()
    }

    /// 全match中のrankに対応するwindow内ID。Window外では`None`。
    #[must_use]
    pub fn file_at_rank(&self, rank: u64) -> Option<FileId> {
        let offset = rank.checked_sub(self.start_rank)?;
        usize::try_from(offset)
            .ok()
            .and_then(|index| self.file_ids.get(index))
            .copied()
    }
}

struct PreparedUnfilteredFileList {
    window: FileFilterWindow,
    selected_item: Option<FileListItem>,
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
        let mut graphemes = before.grapheme_indices(true).rev().peekable();
        let mut start = self.cursor;
        while let Some((index, _)) = graphemes
            .next_if(|(_, grapheme)| grapheme.chars().next().is_some_and(char::is_whitespace))
        {
            start = index;
        }
        while let Some((index, _)) = graphemes.next_if(|(_, grapheme)| {
            grapheme
                .chars()
                .next()
                .is_none_or(|character| !character.is_whitespace())
        }) {
            start = index;
        }
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
    /// Structure navigationとfile jumpでfixed record bufferを再利用する。
    structure_reader: StructureMetadataReader,
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
    /// Overlayを閉じて開き直しても旧worker resultと衝突しないsession-wide counter。
    file_filter_generation: FileFilterGeneration,
    /// Workerへ最後に発行し、同じtask identityの完了または失敗をまだ受理していないrequest。
    /// Document更新時はquery generationがcurrentのcommitted requestだけを新snapshotへ再発行する。
    in_flight_search: Option<SearchTask>,
    /// Streaming publishをまたいで維持するfile-filter query/window intent。
    ///
    /// Window外navigationの`requested_rank`を保持し、新snapshotが先に届いてもselected fileへ
    /// 巻き戻さず同じrankを再発行する。Result/failure、query invalidation、overlay closeで解放する。
    in_flight_file_filter: Option<FileFilterTask>,
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
        validate_document_completion(&document, eof)?;
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
            structure_reader: StructureMetadataReader::new(),
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
            file_filter_generation: FileFilterGeneration::default(),
            in_flight_search: None,
            in_flight_file_filter: None,
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

    /// Pre-draw frameがsticky/list textをterminal幅へ収めるためのcurrent width。
    #[must_use]
    pub const fn terminal_width(&self) -> u16 {
        self.width
    }

    /// Pre-draw frameがoverlay row数をboundedにするためのcurrent height。
    #[must_use]
    pub const fn terminal_height(&self) -> u16 {
        self.height
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

    /// Current document/search generationのworker requestが未完了かを返す。
    ///
    /// Query編集やnavigation intentでgenerationが進んだ後も旧workerは終了通知を返し得るため、単に
    /// `in_flight_search.is_some()`とはしない。UIはこの値だけを`searching`表示へ使い、incoming dataを
    /// 待つ[`crate::search::PendingForward`]とは別の状態として扱う。
    #[must_use]
    pub fn is_search_in_flight(&self) -> bool {
        self.in_flight_search.as_ref().is_some_and(|task| {
            task.generation == self.search.generation()
                && task.document_generation == self.document_generation
        })
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
        let search_was_running = self.is_search_in_flight();
        let file_filter_was_running = self.is_file_filter_in_flight();
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
            Event::FileFilterCompleted {
                generation,
                document_generation,
                total_matches,
                selected_rank,
                start_rank,
                file_ids,
            } => self.apply_file_filter_result(
                generation,
                document_generation,
                total_matches,
                selected_rank,
                start_rank,
                file_ids,
            )?,
            Event::WorkerFailed {
                generation,
                document_generation,
                preview,
                message,
            } => self.apply_worker_failure(generation, document_generation, preview, message),
            Event::FileFilterFailed {
                generation,
                document_generation,
                message,
            } => self.apply_file_filter_failure(generation, document_generation, message),
            Event::BackgroundStatus { message } => {
                self.background_status = message;
                Vec::new()
            }
        };
        if search_was_running
            && !self.is_search_in_flight()
            && !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Search(_)))
        {
            self.in_flight_search = None;
            effects.push(Effect::CancelSearch);
        }
        if file_filter_was_running
            && !self.is_file_filter_in_flight()
            && !effects
                .iter()
                .any(|effect| matches!(effect, Effect::FileFilter(_)))
        {
            effects.push(Effect::CancelFileFilter);
        }
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
                self.open_file_list()?;
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
            self.status.clear();
            self.mode = Mode::Body;
            return Vec::new();
        }
        // Helpはsticky headerを置かず、status以外の全rowを使う。Body用heightを流用すると
        // 最終pageに空行が残り、page移動のoverlapも2行になる。
        let overlay_height = usize::from(self.height).saturating_sub(1);
        let page = overlay_height.saturating_sub(1).max(1);
        let before = match self.mode {
            Mode::Help { top } => top,
            _ => return Vec::new(),
        };
        let mut recognized = true;
        if let Mode::Help { top } = &mut self.mode {
            let max_top = HELP_LINE_COUNT.saturating_sub(overlay_height);
            if is_down_line(&key) {
                *top = top.saturating_add(1).min(max_top);
            } else if is_up_line(&key) {
                *top = top.saturating_sub(1);
            } else if is_down_page(&key) {
                *top = top.saturating_add(page).min(max_top);
            } else if is_up_page(&key) {
                *top = top.saturating_sub(page);
            } else if is_down_half_page(&key) {
                *top = top
                    .saturating_add(overlay_height.div_ceil(2).max(1))
                    .min(max_top);
            } else if is_up_half_page(&key) {
                *top = top.saturating_sub(overlay_height.div_ceil(2).max(1));
            } else if key.code == KeyCode::Home
                || key.code == KeyCode::Char('g') && is_plain_text_key(&key)
            {
                *top = 0;
            } else if key.code == KeyCode::End
                || key.code == KeyCode::Char('G') && is_plain_text_key(&key)
            {
                *top = max_top;
            } else {
                recognized = false;
            }
        }
        let after = match self.mode {
            Mode::Help { top } => top,
            _ => before,
        };
        if !recognized {
            self.status = "unknown key".to_owned();
        } else if after != before {
            self.status.clear();
        } else if is_up_line(&key)
            || is_up_page(&key)
            || is_up_half_page(&key)
            || key.code == KeyCode::Home
            || key.code == KeyCode::Char('g') && is_plain_text_key(&key)
        {
            self.status = "beginning of help".to_owned();
        } else {
            self.status = "end of help".to_owned();
        }
        Vec::new()
    }

    fn handle_file_list_key(&mut self, key: KeyEvent) -> Result<Vec<Effect>, StateError> {
        if key.code == KeyCode::Tab {
            self.invalidate_file_filter();
            self.mode = Mode::Body;
            return Ok(Vec::new());
        }
        let filter_active = matches!(&self.mode, Mode::FileList(state) if state.filter.is_some());
        if filter_active {
            return self.handle_filter_key(key);
        }

        if key.code == KeyCode::Esc || key.code == KeyCode::Char('q') && is_plain_text_key(&key) {
            self.invalidate_file_filter();
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

        self.move_file_selection(key)
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> Result<Vec<Effect>, StateError> {
        if key.code == KeyCode::Esc {
            let selected_file = self.selected_file_id();
            let selected_item = selected_file
                .map(FileListItem::File)
                .or_else(|| self.selected_file_list_item());
            if let Mode::FileList(state) = &mut self.mode {
                state.filter = None;
            }
            // Filter inputだけを閉じる。Queryの有無にかかわらずoverlayは残し、次のEscが
            // file list自体を閉じる。Generation更新で入力中に投げたresultも拒否する。
            self.invalidate_file_filter();
            self.install_unfiltered_file_window(selected_item, None)?;
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
            return self.move_file_selection(key);
        }
        let inserted_bytes = match key.code {
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                Some(character.len_utf8())
            }
            _ => None,
        };
        if let Some(actual) = inserted_bytes.and_then(|inserted| {
            let current = match &self.mode {
                Mode::FileList(state) => state.filter.as_ref()?.text().len(),
                _ => return None,
            };
            Some(current.saturating_add(inserted))
        }) && actual > MAX_FILE_FILTER_QUERY_BYTES
        {
            self.status = file_filter_query_limit_status(actual);
            return Ok(Vec::new());
        }
        let previous = match &self.mode {
            Mode::FileList(state) => state.filter.as_ref().map(|filter| filter.text().to_owned()),
            _ => None,
        };
        if let Mode::FileList(state) = &mut self.mode
            && let Some(filter) = &mut state.filter
            && !edit_text_input(filter, &key)
        {
            self.status = "unknown key".to_owned();
            return Ok(Vec::new());
        }
        let changed = match (&self.mode, previous) {
            (Mode::FileList(state), Some(previous)) => state
                .filter
                .as_ref()
                .is_some_and(|filter| filter.text() != previous),
            _ => false,
        };
        if changed {
            self.recompute_file_filter()
        } else {
            Ok(Vec::new())
        }
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
        let previous_query = match &self.mode {
            Mode::Search(state) => state.input.text().to_owned(),
            _ => return Ok(Vec::new()),
        };
        let mut direct_edit = false;
        if key.code == KeyCode::Up {
            self.browse_search_history(false);
        } else if key.code == KeyCode::Down {
            self.browse_search_history(true);
        } else if let Mode::Search(state) = &mut self.mode {
            if !edit_text_input(&mut state.input, &key) {
                self.status = "unknown key".to_owned();
                return Ok(Vec::new());
            }
            direct_edit = true;
        }
        let changed = matches!(
            &self.mode,
            Mode::Search(state) if state.input.text() != previous_query
        );
        if !changed {
            self.status.clear();
            return Ok(Vec::new());
        }
        if direct_edit && let Mode::Search(state) = &mut self.mode {
            state.history_cursor = None;
            state.history_draft = None;
        }
        self.preview_search()
    }

    fn handle_paste(&mut self, pasted: &str) -> Result<Vec<Effect>, StateError> {
        let mut filter_changed = false;
        let effects = match &mut self.mode {
            Mode::Search(state) => {
                let escaped = escape_path_bytes(pasted.as_bytes());
                state.input.insert(&escaped);
                state.history_cursor = None;
                state.history_draft = None;
                return self.preview_search();
            }
            Mode::FileList(state) => {
                if let Some(filter) = &mut state.filter {
                    let current_len = filter.text().len();
                    let remaining = MAX_FILE_FILTER_QUERY_BYTES.saturating_sub(current_len);
                    match escape_path_bytes_with_limit(pasted.as_bytes(), remaining)? {
                        LimitedPathEscape::Escaped(escaped) => {
                            filter.insert(&escaped);
                            filter_changed = true;
                        }
                        LimitedPathEscape::LimitExceeded { escaped_len } => {
                            let actual = current_len.saturating_add(escaped_len);
                            self.status = file_filter_query_limit_status(actual);
                        }
                    }
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
        if filter_changed {
            self.recompute_file_filter()
        } else {
            Ok(effects)
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
            IncrementalQueryStatus::Invalid(_) => {
                // Regex errorはSearch promptがIncrementalQueryから直接描く。Generic statusにも複製すると
                // 同じerrorがsuffixへ二度入るため、直前commandのstatusだけを消す。
                self.status.clear();
                return Ok(Vec::new());
            }
            IncrementalQueryStatus::Valid => self.status.clear(),
        }
        Ok(self.issue_incremental_preview(update.generation, direction, count))
    }

    /// Document replacement後にincremental queryのworker generationだけを更新する。
    ///
    /// Empty queryのviewport復元はquery edit時に完了しており、replacement用viewportもそのanchorから
    /// prepare済みである。ここで再layoutしないことでdocument swap後のfallible処理を作らない。
    fn refresh_incremental_search_after_document_update(&mut self) -> Vec<Effect> {
        let (query, direction, count) = match &self.mode {
            Mode::Search(state) => (state.input.text().to_owned(), state.direction, state.count),
            _ => return Vec::new(),
        };
        let Some(update) = self.search.update_incremental(&query) else {
            return Vec::new();
        };
        match update.status {
            IncrementalQueryStatus::Empty | IncrementalQueryStatus::Invalid(_) => {
                self.status.clear();
                Vec::new()
            }
            IncrementalQueryStatus::Valid => {
                self.status.clear();
                self.issue_incremental_preview(update.generation, direction, count)
            }
        }
    }

    fn issue_incremental_preview(
        &mut self,
        generation: SearchGeneration,
        direction: SearchDirection,
        count: NonZeroUsize,
    ) -> Vec<Effect> {
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
        self.issue_search(SearchTask {
            generation,
            document_generation: self.document_generation,
            compiled,
            request: ScanRequest::new(direction, anchor, count),
            preview: true,
        })
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
        self.pending_line = None;
        let generation = self.search.begin_navigation_intent();
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
                self.status.clear();
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

    #[allow(clippy::too_many_arguments)]
    fn apply_file_filter_result(
        &mut self,
        generation: FileFilterGeneration,
        document_generation: DocumentGeneration,
        total_matches: u64,
        selected_rank: Option<u64>,
        start_rank: u64,
        file_ids: Vec<FileId>,
    ) -> Result<Vec<Effect>, StateError> {
        if document_generation != self.document_generation {
            return Ok(Vec::new());
        }
        let Mode::FileList(state) = &self.mode else {
            return Ok(Vec::new());
        };
        if state.generation != generation
            || state
                .filter
                .as_ref()
                .is_none_or(|filter| filter.text().is_empty())
        {
            return Ok(Vec::new());
        }
        if !self.file_filter_task_matches(generation, document_generation) {
            return Ok(Vec::new());
        }
        if file_ids.len() > FILE_FILTER_WINDOW_CAPACITY {
            return Err(StateError::InvalidFileFilterWindow {
                message: "result exceeds the stable-ID capacity",
            });
        }
        let len =
            u64::try_from(file_ids.len()).map_err(|_| StateError::InvalidFileFilterWindow {
                message: "result length does not fit u64",
            })?;
        let end = start_rank
            .checked_add(len)
            .ok_or(StateError::InvalidFileFilterWindow {
                message: "result rank range overflows u64",
            })?;
        if start_rank > total_matches || end > total_matches {
            return Err(StateError::InvalidFileFilterWindow {
                message: "result rank range exceeds total matches",
            });
        }
        match (total_matches, selected_rank) {
            (0, None) if start_rank == 0 && file_ids.is_empty() => {}
            (0, _) => {
                return Err(StateError::InvalidFileFilterWindow {
                    message: "empty result has a selection or retained IDs",
                });
            }
            (_, Some(rank)) if rank < total_matches && rank >= start_rank && rank < end => {}
            _ => {
                return Err(StateError::InvalidFileFilterWindow {
                    message: "selection is outside the returned rank window",
                });
            }
        }
        let mut previous = None;
        for id in &file_ids {
            if self.document.structure().file_id_at(id.get()) != Some(*id)
                || previous.is_some_and(|previous| previous >= id.get())
            {
                return Err(StateError::InvalidFileFilterWindow {
                    message: "stable IDs are missing or not in Git output order",
                });
            }
            previous = Some(id.get());
        }
        let window = FileFilterWindow {
            total_matches,
            selected_rank,
            start_rank,
            file_ids,
        };
        let selected_file = window.selected_file();
        if total_matches > 0 && selected_file.is_none() {
            return Err(StateError::InvalidFileFilterWindow {
                message: "selected rank has no corresponding stable ID",
            });
        }
        if let Mode::FileList(state) = &mut self.mode {
            state.selected_file = selected_file;
            if let Some(file_id) = selected_file {
                state.selected_item = Some(FileListItem::File(file_id));
            }
            state.window = Some(window);
            state.loading = false;
        }
        self.in_flight_file_filter = None;
        self.status.clear();
        Ok(Vec::new())
    }

    fn apply_file_filter_failure(
        &mut self,
        generation: FileFilterGeneration,
        document_generation: DocumentGeneration,
        message: String,
    ) -> Vec<Effect> {
        if document_generation != self.document_generation
            || !self.file_filter_task_matches(generation, document_generation)
        {
            return Vec::new();
        }
        let Mode::FileList(state) = &mut self.mode else {
            return Vec::new();
        };
        if state.generation != generation
            || state
                .filter
                .as_ref()
                .is_none_or(|filter| filter.text().is_empty())
        {
            return Vec::new();
        }
        state.loading = false;
        self.in_flight_file_filter = None;
        self.status = message;
        Vec::new()
    }

    fn file_filter_task_matches(
        &self,
        generation: FileFilterGeneration,
        document_generation: DocumentGeneration,
    ) -> bool {
        self.in_flight_file_filter.as_ref().is_some_and(|task| {
            task.generation == generation && task.document_generation == document_generation
        })
    }

    fn is_file_filter_in_flight(&self) -> bool {
        self.in_flight_file_filter.as_ref().is_some_and(|task| {
            task.generation == self.file_filter_generation
                && task.document_generation == self.document_generation
        })
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
        validate_document_successor(&self.document, &document)?;
        validate_document_completion(&document, eof)?;
        let anchor = self.viewport_anchor;
        let resolved_pending = self.pending_line.and_then(|line| {
            line.checked_sub(1)
                .and_then(|index| document.line_id_at(index))
                .map(|line_id| (line, VisualAnchor::new(line_id, 0)))
        });
        let requested_anchor = resolved_pending.map_or(anchor, |(_, anchor)| Some(anchor));
        let selected_file = self.selected_file_id();
        let selected_item = self.selected_file_list_item();
        let file_filter_query = match &self.mode {
            Mode::FileList(state) => state
                .filter
                .as_ref()
                .map(|filter| filter.text().to_owned())
                .filter(|query| !query.is_empty()),
            _ => None,
        };
        let file_filter_intent = file_filter_query.as_ref().and_then(|query| {
            self.in_flight_file_filter
                .as_ref()
                .filter(|task| task.query == *query)
                .map(|task| (task.preferred_file, task.requested_rank))
        });
        let prepared_unfiltered_file_list =
            if file_filter_query.is_none() && matches!(self.mode, Mode::FileList(_)) {
                Some(prepare_unfiltered_file_list(
                    &mut self.structure_reader,
                    document.structure(),
                    selected_item,
                    None,
                )?)
            } else {
                None
            };
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
        let resolved_pending_moved =
            resolved_pending.is_some() && prepared.anchor != self.viewport_anchor;
        self.apply_prepared_viewport(prepared)?;
        let interrupted_search = self.in_flight_search.take();
        self.document = document;
        self.document_generation = self.document_generation.next();
        self.eof = eof;
        let mut effects = if let Some(query) = file_filter_query {
            let (preferred_file, requested_rank) =
                file_filter_intent.unwrap_or((selected_file, None));
            self.issue_file_filter(query, preferred_file, requested_rank, true)
        } else {
            if let Some(prepared) = prepared_unfiltered_file_list {
                self.install_prepared_unfiltered_file_list(prepared);
            }
            Vec::new()
        };
        if resolved_pending.is_some() {
            self.pending_line = None;
            if resolved_pending_moved {
                self.follow = false;
            }
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
            effects.extend(self.issue_search(task));
        } else if matches!(&self.mode, Mode::Search(_)) {
            // Document snapshot も worker result の鮮度を決める。query が同じでも generation を進め、
            // append 前 snapshot を走査中の preview が後着して viewport を戻すのを防ぐ。
            effects.extend(self.refresh_incremental_search_after_document_update());
        } else if let Some(mut task) = interrupted_search
            .filter(|task| !task.preview && task.generation == self.search.generation())
        {
            // Query intentと開始anchorはそのままに、workerが読むsnapshotだけを交換する。旧taskの
            // resultはdocument generation不一致で拒否され、新taskがfinal EOFの判定を所有する。
            task.document_generation = self.document_generation;
            effects.extend(self.issue_search(task));
        }
        Ok(effects)
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
        // Search contractは一致rowを本文先頭へ置く。Bottom clampは通常scroll専用で、EOF付近の
        // matchへ適用すると前行を補うため一致位置が画面下へずれる。
        let _ = self.install_anchor(anchor, false)?;
        self.follow = false;
        self.pending_line = None;
        Ok(())
    }

    fn move_rows(&mut self, forward: bool, amount: usize) -> Result<(), StateError> {
        self.begin_position_navigation();
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
        self.begin_position_navigation();
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
            self.status.clear();
        } else {
            self.status = "beginning of input".to_owned();
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
        self.begin_position_navigation();
        if one_based == 0 {
            self.status = "count must be greater than zero".to_owned();
            return Ok(());
        }
        if let Some(line_id) = self.document.line_id_at(one_based - 1) {
            let moved = self.install_anchor(VisualAnchor::new(line_id, 0), true)?;
            if moved {
                self.follow = false;
            }
            self.status.clear();
        } else if !self.eof {
            self.follow = false;
            self.pending_line = Some(one_based);
            self.status = format!("waiting for input line {one_based}…");
        } else {
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
        self.begin_position_navigation();
        let Some(current) = self.viewport_anchor else {
            self.status = if !forward {
                "no previous loaded target".to_owned()
            } else if self.eof {
                "no next loaded target".to_owned()
            } else {
                "no next loaded target (loading)".to_owned()
            };
            return Ok(());
        };
        let count = u64::try_from(count)
            .ok()
            .and_then(NonZeroU64::new)
            .ok_or(StateError::StructureCountOverflow { count })?;
        let snapshot = self.document.structure();
        let target = match structure {
            Structure::Hunk => {
                let id = if forward {
                    self.structure_reader
                        .nth_hunk_after(snapshot, current.line_id.get(), count)?
                } else {
                    self.structure_reader
                        .nth_hunk_before(snapshot, current.line_id.get(), count)?
                };
                id.map(|id| {
                    self.structure_reader
                        .read_hunk(snapshot, id)
                        .and_then(|hunk| {
                            hunk.ok_or(StructureStoreError::OutsideSnapshot {
                                kind: "hunk",
                                id: id.get(),
                                high_water: snapshot.hunk_count(),
                            })
                        })
                        .map(|hunk| VisualAnchor::new(hunk.header_line, 0))
                })
                .transpose()?
            }
            Structure::File => {
                let id = if forward {
                    self.structure_reader
                        .nth_file_after(snapshot, current.line_id.get(), count)?
                } else {
                    self.structure_reader
                        .nth_file_before(snapshot, current.line_id.get(), count)?
                };
                id.map(|id| {
                    self.structure_reader
                        .read_file(snapshot, id)
                        .and_then(|file| {
                            file.ok_or(StructureStoreError::OutsideSnapshot {
                                kind: "file",
                                id: id.get(),
                                high_water: snapshot.file_count(),
                            })
                        })
                        .map(|file| VisualAnchor::new(file.span.start, 0))
                })
                .transpose()?
            }
            Structure::Commit => {
                let id = if forward {
                    self.structure_reader.nth_commit_after(
                        snapshot,
                        current.line_id.get(),
                        count,
                    )?
                } else {
                    self.structure_reader.nth_commit_before(
                        snapshot,
                        current.line_id.get(),
                        count,
                    )?
                };
                id.map(|id| {
                    self.structure_reader
                        .read_commit(snapshot, id)
                        .and_then(|commit| {
                            commit.ok_or(StructureStoreError::OutsideSnapshot {
                                kind: "commit",
                                id: id.get(),
                                high_water: snapshot.commit_count(),
                            })
                        })
                        .map(|commit| VisualAnchor::new(commit.span.start, 0))
                })
                .transpose()?
            }
        };
        if let Some(target) = target {
            let moved = self.install_anchor(target, true)?;
            if moved {
                self.follow = false;
            }
            self.status.clear();
        } else {
            self.status = if !forward {
                "no previous loaded target".to_owned()
            } else if self.eof {
                "no next loaded target".to_owned()
            } else {
                "no next loaded target (loading)".to_owned()
            };
        }
        Ok(())
    }

    /// Ngまたはforward searchの待機を、新しい位置navigation commandで置き換える。
    ///
    /// Follow解除は実際に移動したcallerだけが行う。Pending intentはBOF/EOFでclampしたcommandでも
    /// 破棄し、後から到着したdataが古いtargetへviewportを移動させない。
    fn begin_position_navigation(&mut self) {
        self.pending_line = None;
        self.search.begin_navigation_intent();
    }

    fn open_file_list(&mut self) -> Result<(), StateError> {
        let active_file = self.current_row().and_then(|row| row.file_id);
        let active_commit = self.current_row().and_then(|row| row.commit_id);
        let active_item = if let Some(file_id) = active_file {
            Some(FileListItem::File(file_id))
        } else if let Some(commit_id) = active_commit {
            self.structure_reader
                .read_commit(self.document.structure(), commit_id)?
                .filter(|commit| commit.finalized && commit.files.count() == 0)
                .map(|_| FileListItem::EmptyCommit(commit_id))
        } else {
            None
        };
        self.invalidate_file_filter();
        self.mode = Mode::FileList(FileListState {
            selected_item: active_item,
            selected_file: active_file,
            window: None,
            filter: None,
            generation: self.file_filter_generation,
            loading: false,
        });
        self.install_unfiltered_file_window(active_item, None)
    }

    fn recompute_file_filter(&mut self) -> Result<Vec<Effect>, StateError> {
        let selected_file = self.selected_file_id();
        let selected_item = self.selected_file_list_item();
        let query = match &self.mode {
            Mode::FileList(state) => state
                .filter
                .as_ref()
                .map(|filter| filter.text().to_owned())
                .unwrap_or_default(),
            _ => return Ok(Vec::new()),
        };
        if query.is_empty() {
            self.invalidate_file_filter();
            self.install_unfiltered_file_window(selected_item, None)?;
            return Ok(Vec::new());
        }
        Ok(self.issue_file_filter(query, selected_file, None, true))
    }

    fn selected_file_id(&self) -> Option<FileId> {
        let Mode::FileList(state) = &self.mode else {
            return None;
        };
        state.selected_file
    }

    fn selected_file_list_item(&self) -> Option<FileListItem> {
        let Mode::FileList(state) = &self.mode else {
            return None;
        };
        state.selected_item
    }

    fn invalidate_file_filter(&mut self) -> FileFilterGeneration {
        self.file_filter_generation = self.file_filter_generation.next();
        self.in_flight_file_filter = None;
        if let Mode::FileList(state) = &mut self.mode {
            state.generation = self.file_filter_generation;
            state.loading = false;
        }
        self.file_filter_generation
    }

    fn install_unfiltered_file_window(
        &mut self,
        preferred_item: Option<FileListItem>,
        requested_rank: Option<u64>,
    ) -> Result<(), StateError> {
        let prepared = prepare_unfiltered_file_list(
            &mut self.structure_reader,
            self.document.structure(),
            preferred_item,
            requested_rank,
        )?;
        self.install_prepared_unfiltered_file_list(prepared);
        Ok(())
    }

    fn install_prepared_unfiltered_file_list(&mut self, prepared: PreparedUnfilteredFileList) {
        let PreparedUnfilteredFileList {
            window,
            selected_item,
        } = prepared;
        if let Mode::FileList(state) = &mut self.mode {
            state.selected_item = selected_item;
            state.selected_file = selected_item.and_then(FileListItem::file_id);
            state.window = Some(window);
            state.generation = self.file_filter_generation;
            state.loading = false;
        }
    }

    fn issue_file_filter(
        &mut self,
        query: String,
        preferred_file: Option<FileId>,
        requested_rank: Option<u64>,
        clear_window: bool,
    ) -> Vec<Effect> {
        let generation = self.invalidate_file_filter();
        if let Mode::FileList(state) = &mut self.mode {
            state.loading = true;
            if clear_window {
                state.window = None;
            }
        }
        let task = FileFilterTask {
            generation,
            document_generation: self.document_generation,
            query,
            preferred_file,
            requested_rank,
        };
        self.in_flight_file_filter = Some(task.clone());
        vec![Effect::FileFilter(task)]
    }

    fn move_file_selection(&mut self, key: KeyEvent) -> Result<Vec<Effect>, StateError> {
        if !is_file_list_navigation_key(&key) {
            self.status = "unknown key".to_owned();
            return Ok(Vec::new());
        }
        let page = self.body_height().saturating_sub(2).max(1);
        let (query, total, current) = match &self.mode {
            Mode::FileList(state) => {
                let query = state
                    .filter
                    .as_ref()
                    .map(|filter| filter.text().to_owned())
                    .filter(|query| !query.is_empty());
                let Some(window) = state.window.as_ref() else {
                    self.status = if state.loading {
                        "file list loading…".to_owned()
                    } else {
                        "no selectable files".to_owned()
                    };
                    return Ok(Vec::new());
                };
                (query, window.total_matches, window.selected_rank)
            }
            _ => return Ok(Vec::new()),
        };
        let Some(query) = query else {
            self.move_unfiltered_file_list_selection(key, page)?;
            return Ok(Vec::new());
        };
        let Some(current) = current else {
            self.status = "no selectable files".to_owned();
            return Ok(Vec::new());
        };
        let Some(target) = file_list_target_rank(key, current, total, page) else {
            return Ok(Vec::new());
        };
        if target == current {
            if self.is_file_filter_in_flight() {
                self.invalidate_file_filter();
            }
            self.status = if file_list_moves_forward(&key) {
                if self.eof {
                    "end of file list".to_owned()
                } else {
                    "end of loaded file list (loading)".to_owned()
                }
            } else {
                "beginning of file list".to_owned()
            };
            return Ok(Vec::new());
        }

        let target_file = match &self.mode {
            Mode::FileList(state) => state
                .window
                .as_ref()
                .and_then(|window| window.file_at_rank(target)),
            _ => None,
        };
        if let Some(file_id) = target_file {
            if self.is_file_filter_in_flight() {
                self.invalidate_file_filter();
            }
            if let Mode::FileList(state) = &mut self.mode
                && let Some(window) = &mut state.window
            {
                window.selected_rank = Some(target);
                state.selected_file = Some(file_id);
                state.selected_item = Some(FileListItem::File(file_id));
            }
            self.status.clear();
            return Ok(Vec::new());
        }
        self.status.clear();
        Ok(self.issue_file_filter(query, None, Some(target), false))
    }

    fn move_unfiltered_file_list_selection(
        &mut self,
        key: KeyEvent,
        page: usize,
    ) -> Result<(), StateError> {
        let current = self.selected_file_list_item();
        let Some(current) = current else {
            self.status = if self.document.is_complete() {
                "no selectable files".to_owned()
            } else {
                "file list loading…".to_owned()
            };
            return Ok(());
        };
        let snapshot = self.document.structure();
        let target = if key.code == KeyCode::Home
            || key.code == KeyCode::Char('g') && is_plain_text_key(&key)
        {
            first_unfiltered_file_list_item(&mut self.structure_reader, snapshot)?
        } else if key.code == KeyCode::End
            || key.code == KeyCode::Char('G') && is_plain_text_key(&key)
        {
            last_unfiltered_file_list_item(&mut self.structure_reader, snapshot)?
        } else {
            let steps = if is_down_page(&key) || is_up_page(&key) {
                page
            } else {
                1
            };
            let mut candidate = current;
            for _ in 0..steps {
                let next = if file_list_moves_forward(&key) {
                    next_unfiltered_file_list_item(&mut self.structure_reader, snapshot, candidate)?
                } else {
                    previous_unfiltered_file_list_item(
                        &mut self.structure_reader,
                        snapshot,
                        candidate,
                    )?
                };
                let Some(next) = next else {
                    break;
                };
                candidate = next;
            }
            Some(candidate)
        };
        let Some(target) = target else {
            self.status = "no selectable files".to_owned();
            return Ok(());
        };
        if target == current {
            self.status = if file_list_moves_forward(&key) {
                if self.eof {
                    "end of file list".to_owned()
                } else {
                    "end of loaded file list (loading)".to_owned()
                }
            } else {
                "beginning of file list".to_owned()
            };
            return Ok(());
        }

        let needs_window = target.file_id().is_some_and(|file_id| match &self.mode {
            Mode::FileList(state) => state
                .window
                .as_ref()
                .is_none_or(|window| !window.file_ids.contains(&file_id)),
            _ => false,
        });
        if needs_window {
            self.install_unfiltered_file_window(Some(target), target.file_id().map(FileId::get))?;
        } else if let Mode::FileList(state) = &mut self.mode {
            state.selected_item = Some(target);
            state.selected_file = target.file_id();
            if let Some(file_id) = target.file_id()
                && let Some(window) = &mut state.window
            {
                window.selected_rank = Some(file_id.get());
            }
        }
        self.status.clear();
        Ok(())
    }

    fn jump_selected_file(&mut self) -> Result<(), StateError> {
        let filtered_without_window = match &self.mode {
            Mode::FileList(state)
                if state
                    .filter
                    .as_ref()
                    .is_some_and(|filter| !filter.text().is_empty())
                    && state.window.is_none() =>
            {
                Some(state.loading)
            }
            _ => None,
        };
        if let Some(loading) = filtered_without_window {
            if loading {
                self.status = "file list loading…".to_owned();
            }
            return Ok(());
        }
        let item = match &self.mode {
            Mode::FileList(state)
                if state
                    .filter
                    .as_ref()
                    .is_some_and(|filter| !filter.text().is_empty()) =>
            {
                state.selected_file.map(FileListItem::File)
            }
            Mode::FileList(state) => state.selected_item,
            _ => None,
        };
        let Some(item) = item else {
            self.status = if self.eof {
                "no selectable files".to_owned()
            } else {
                "file list loading…".to_owned()
            };
            return Ok(());
        };
        let anchor = match item {
            FileListItem::File(file_id) => {
                let file = self
                    .structure_reader
                    .read_file(self.document.structure(), file_id)?
                    .ok_or(StructureStoreError::OutsideSnapshot {
                        kind: "file",
                        id: file_id.get(),
                        high_water: self.document.structure().file_count(),
                    })?;
                VisualAnchor::new(file.span.start, 0)
            }
            FileListItem::EmptyCommit(commit_id) => {
                let commit = self
                    .structure_reader
                    .read_commit(self.document.structure(), commit_id)?
                    .filter(|commit| commit.finalized && commit.files.count() == 0)
                    .ok_or(StructureStoreError::OutsideSnapshot {
                        kind: "empty commit",
                        id: commit_id.get(),
                        high_water: self.document.structure().commit_count(),
                    })?;
                VisualAnchor::new(commit.span.start, 0)
            }
        };
        {
            self.invalidate_file_filter();
            self.mode = Mode::Body;
            let moved = self.install_anchor(anchor, false)?;
            if moved {
                self.follow = false;
            }
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

fn validate_document_successor(
    previous: &DocumentSnapshot,
    current: &DocumentSnapshot,
) -> Result<(), StateError> {
    if previous.identity() != current.identity() {
        return Err(StateError::UnrelatedDocumentUpdate);
    }
    if current.line_count() < previous.line_count() {
        return Err(StateError::RegressiveDocumentLines {
            previous: previous.line_count(),
            current: current.line_count(),
        });
    }
    for (kind, previous, current) in [
        (
            "commit",
            previous.structure().commit_count(),
            current.structure().commit_count(),
        ),
        (
            "file",
            previous.structure().file_count(),
            current.structure().file_count(),
        ),
        (
            "hunk",
            previous.structure().hunk_count(),
            current.structure().hunk_count(),
        ),
    ] {
        if current < previous {
            return Err(StateError::RegressiveDocumentStructure {
                kind,
                previous,
                current,
            });
        }
    }
    if previous.is_complete() && !current.is_complete() {
        return Err(StateError::ReopenedCompleteDocument);
    }
    Ok(())
}

fn validate_document_completion(document: &DocumentSnapshot, eof: bool) -> Result<(), StateError> {
    let document_complete = document.is_complete();
    if eof != document_complete {
        return Err(StateError::DocumentCompletionMismatch {
            eof,
            document_complete,
        });
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Structure {
    Hunk,
    File,
    Commit,
}

fn identity_file_window(
    snapshot: &StructureSnapshot,
    preferred_file: Option<FileId>,
    requested_rank: Option<u64>,
) -> Result<FileFilterWindow, StateError> {
    let total_matches = snapshot.file_count();
    let selected_rank = if total_matches == 0 {
        None
    } else {
        Some(
            preferred_file
                .map(FileId::get)
                .filter(|rank| *rank < total_matches)
                .or(requested_rank)
                .unwrap_or(0)
                .min(total_matches - 1),
        )
    };
    let capacity = u64::try_from(FILE_FILTER_WINDOW_CAPACITY).map_err(|_| {
        StateError::InvalidFileFilterWindow {
            message: "file-list window capacity does not fit u64",
        }
    })?;
    let window_len_u64 = total_matches.min(capacity);
    let window_len =
        usize::try_from(window_len_u64).map_err(|_| StateError::InvalidFileFilterWindow {
            message: "bounded identity window does not fit usize",
        })?;
    let half = window_len_u64 / 2;
    let max_start = total_matches.saturating_sub(window_len_u64);
    let start_rank = selected_rank.map_or(0, |rank| rank.saturating_sub(half).min(max_start));
    let mut file_ids = Vec::new();
    file_ids
        .try_reserve_exact(window_len)
        .map_err(|_| StateError::FileWindowAllocation {
            requested: window_len.saturating_mul(std::mem::size_of::<FileId>()),
        })?;
    let end =
        start_rank
            .checked_add(window_len_u64)
            .ok_or(StateError::InvalidFileFilterWindow {
                message: "identity window end overflowed u64",
            })?;
    for ordinal in start_rank..end {
        let id = snapshot
            .file_id_at(ordinal)
            .ok_or(StateError::InvalidFileFilterWindow {
                message: "identity window crossed the structure high-water mark",
            })?;
        file_ids.push(id);
    }
    Ok(FileFilterWindow {
        total_matches,
        selected_rank,
        start_rank,
        file_ids,
    })
}

fn prepare_unfiltered_file_list(
    reader: &mut StructureMetadataReader,
    snapshot: &StructureSnapshot,
    preferred_item: Option<FileListItem>,
    requested_rank: Option<u64>,
) -> Result<PreparedUnfilteredFileList, StateError> {
    let preferred_file = preferred_item.and_then(FileListItem::file_id);
    let window = identity_file_window(snapshot, preferred_file, requested_rank)?;
    let selected_item = resolve_unfiltered_file_list_item(reader, snapshot, preferred_item)?;
    Ok(PreparedUnfilteredFileList {
        window,
        selected_item,
    })
}

fn resolve_unfiltered_file_list_item(
    reader: &mut StructureMetadataReader,
    snapshot: &StructureSnapshot,
    preferred: Option<FileListItem>,
) -> Result<Option<FileListItem>, StateError> {
    if let Some(preferred) = preferred {
        let available = match preferred {
            FileListItem::File(file_id) => reader.read_file(snapshot, file_id)?.is_some(),
            FileListItem::EmptyCommit(commit_id) => reader
                .read_commit(snapshot, commit_id)?
                .is_some_and(|commit| commit.finalized && commit.files.count() == 0),
        };
        if available {
            return Ok(Some(preferred));
        }
    }
    first_unfiltered_file_list_item(reader, snapshot).map_err(Into::into)
}

pub(crate) fn first_unfiltered_file_list_item(
    reader: &mut StructureMetadataReader,
    snapshot: &StructureSnapshot,
) -> Result<Option<FileListItem>, StructureStoreError> {
    file_list_item_from_commit_range(reader, snapshot, 0, true)
}

pub(crate) fn last_unfiltered_file_list_item(
    reader: &mut StructureMetadataReader,
    snapshot: &StructureSnapshot,
) -> Result<Option<FileListItem>, StructureStoreError> {
    let Some(last) = snapshot.commit_count().checked_sub(1) else {
        return Ok(None);
    };
    file_list_item_from_commit_range(reader, snapshot, last, false)
}

pub(crate) fn next_unfiltered_file_list_item(
    reader: &mut StructureMetadataReader,
    snapshot: &StructureSnapshot,
    current: FileListItem,
) -> Result<Option<FileListItem>, StructureStoreError> {
    let commit_id = match current {
        FileListItem::File(file_id) => {
            let file = reader.read_file(snapshot, file_id)?.ok_or(
                StructureStoreError::OutsideSnapshot {
                    kind: "file",
                    id: file_id.get(),
                    high_water: snapshot.file_count(),
                },
            )?;
            let commit = read_file_list_commit(reader, snapshot, file.commit_id)?;
            if let Some(next) = file_id.get().checked_add(1).map(FileId::new)
                && commit.files.contains(next)
            {
                return Ok(Some(FileListItem::File(next)));
            }
            file.commit_id
        }
        FileListItem::EmptyCommit(commit_id) => commit_id,
    };
    let Some(next_commit) = commit_id.get().checked_add(1) else {
        return Ok(None);
    };
    file_list_item_from_commit_range(reader, snapshot, next_commit, true)
}

pub(crate) fn previous_unfiltered_file_list_item(
    reader: &mut StructureMetadataReader,
    snapshot: &StructureSnapshot,
    current: FileListItem,
) -> Result<Option<FileListItem>, StructureStoreError> {
    let commit_id = match current {
        FileListItem::File(file_id) => {
            let file = reader.read_file(snapshot, file_id)?.ok_or(
                StructureStoreError::OutsideSnapshot {
                    kind: "file",
                    id: file_id.get(),
                    high_water: snapshot.file_count(),
                },
            )?;
            let commit = read_file_list_commit(reader, snapshot, file.commit_id)?;
            if file_id.get() > commit.files.start() {
                return Ok(Some(FileListItem::File(FileId::new(file_id.get() - 1))));
            }
            file.commit_id
        }
        FileListItem::EmptyCommit(commit_id) => commit_id,
    };
    let Some(previous_commit) = commit_id.get().checked_sub(1) else {
        return Ok(None);
    };
    file_list_item_from_commit_range(reader, snapshot, previous_commit, false)
}

fn file_list_item_from_commit_range(
    reader: &mut StructureMetadataReader,
    snapshot: &StructureSnapshot,
    start: u64,
    forward: bool,
) -> Result<Option<FileListItem>, StructureStoreError> {
    let mut ordinal = Some(start);
    while let Some(current) = ordinal.filter(|current| *current < snapshot.commit_count()) {
        let commit = read_file_list_commit(reader, snapshot, CommitId::new(current))?;
        if let Some(file_id) = if forward {
            commit.files.first()
        } else {
            commit.files.last()
        } {
            return Ok(Some(FileListItem::File(file_id)));
        }
        if commit.finalized {
            return Ok(Some(FileListItem::EmptyCommit(commit.id)));
        }
        // Parserが同時に持てるopen commitは末尾の1件だけである。Zero-file open commitをskipする
        // loopはforward/backwardとも1回までで、件数に比例するprefix scanにはならない。
        ordinal = if forward {
            current.checked_add(1)
        } else {
            current.checked_sub(1)
        };
    }
    Ok(None)
}

fn read_file_list_commit(
    reader: &mut StructureMetadataReader,
    snapshot: &StructureSnapshot,
    commit_id: CommitId,
) -> Result<StoredCommitMetadata, StructureStoreError> {
    reader
        .read_commit(snapshot, commit_id)?
        .ok_or(StructureStoreError::OutsideSnapshot {
            kind: "commit",
            id: commit_id.get(),
            high_water: snapshot.commit_count(),
        })
}

fn file_list_target_rank(key: KeyEvent, current: u64, total: u64, page: usize) -> Option<u64> {
    if total == 0 {
        return None;
    }
    let page = u64::try_from(page).unwrap_or(u64::MAX);
    let target = if is_down_line(&key) {
        current.saturating_add(1)
    } else if is_up_line(&key) {
        current.saturating_sub(1)
    } else if is_down_page(&key) {
        current.saturating_add(page)
    } else if is_up_page(&key) {
        current.saturating_sub(page)
    } else if key.code == KeyCode::Home || key.code == KeyCode::Char('g') && is_plain_text_key(&key)
    {
        0
    } else if key.code == KeyCode::End || key.code == KeyCode::Char('G') && is_plain_text_key(&key)
    {
        total - 1
    } else {
        return None;
    };
    Some(target.min(total - 1))
}

fn is_file_list_navigation_key(key: &KeyEvent) -> bool {
    is_down_line(key)
        || is_up_line(key)
        || is_down_page(key)
        || is_up_page(key)
        || key.code == KeyCode::Home
        || key.code == KeyCode::End
        || matches!(key.code, KeyCode::Char('g' | 'G')) && is_plain_text_key(key)
}

fn file_list_moves_forward(key: &KeyEvent) -> bool {
    is_down_line(key)
        || is_down_page(key)
        || key.code == KeyCode::End
        || key.code == KeyCode::Char('G') && is_plain_text_key(key)
}

fn edit_text_input(input: &mut TextInput, key: &KeyEvent) -> bool {
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
    } else {
        return false;
    }
    true
}

fn file_filter_query_limit_status(actual: usize) -> String {
    format!(
        "file filter query is {actual} bytes, exceeding the {MAX_FILE_FILTER_QUERY_BYTES}-byte limit"
    )
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
    use crate::model::{CommitId, HunkId, LineKind};
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
        builder.mark_complete().unwrap();
        builder.snapshot().unwrap()
    }

    fn key(character: char) -> Event {
        Event::Key(plain_key(character))
    }

    fn special(code: KeyCode) -> Event {
        Event::Key(special_key(code))
    }

    fn plain_key(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)
    }

    fn shifted_key(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::SHIFT)
    }

    fn special_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_key(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
    }

    fn alt_key(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::ALT)
    }

    #[derive(Debug, PartialEq, Eq)]
    enum ModeOutcome {
        Body,
        Help {
            top: usize,
        },
        FileList {
            selected_file: Option<u64>,
            total_matches: Option<u64>,
            selected_rank: Option<u64>,
            start_rank: Option<u64>,
            file_ids: Vec<u64>,
            filter: Option<String>,
            loading: bool,
        },
        Search {
            direction: SearchDirection,
            input: String,
            count: usize,
        },
    }

    #[derive(Debug, PartialEq, Eq)]
    enum EffectOutcome {
        Redraw,
        Repaint,
        Suspend,
        Quit(u8),
        Search {
            direction: SearchDirection,
            count: usize,
            preview: bool,
            pattern: String,
        },
        FileFilter {
            generation: u64,
            document_generation: u64,
            query: String,
            preferred_file: Option<u64>,
            requested_rank: Option<u64>,
        },
        CancelSearch,
        CancelFileFilter,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct KeyOutcome {
        top: Option<VisualAnchor>,
        current_context: Option<RowContext>,
        mode: ModeOutcome,
        whitespace: WhitespaceChoice,
        following: bool,
        pending_line: Option<usize>,
        status: String,
        search_generation: u64,
        effects: Vec<EffectOutcome>,
    }

    type RowContext = (Option<CommitId>, Option<FileId>, Option<HunkId>);

    fn mode_outcome(app: &AppState) -> ModeOutcome {
        match app.mode() {
            Mode::Body => ModeOutcome::Body,
            Mode::Help { top } => ModeOutcome::Help { top: *top },
            Mode::FileList(list) => {
                let window = list.window.as_ref();
                ModeOutcome::FileList {
                    selected_file: list.selected_file.map(FileId::get),
                    total_matches: window.map(|window| window.total_matches),
                    selected_rank: window.and_then(|window| window.selected_rank),
                    start_rank: window.map(|window| window.start_rank),
                    file_ids: window
                        .map(|window| window.file_ids.iter().map(|id| id.get()).collect())
                        .unwrap_or_default(),
                    filter: list.filter.as_ref().map(|input| input.text().to_owned()),
                    loading: list.loading,
                }
            }
            Mode::Search(search) => ModeOutcome::Search {
                direction: search.direction,
                input: search.input.text().to_owned(),
                count: search.count.get(),
            },
        }
    }

    fn effect_outcomes(effects: Vec<Effect>) -> Vec<EffectOutcome> {
        effects
            .into_iter()
            .map(|effect| match effect {
                Effect::Redraw => EffectOutcome::Redraw,
                Effect::Repaint => EffectOutcome::Repaint,
                Effect::Suspend => EffectOutcome::Suspend,
                Effect::Quit(code) => EffectOutcome::Quit(code),
                Effect::Search(task) => EffectOutcome::Search {
                    direction: task.request.direction,
                    count: task.request.count.get(),
                    preview: task.preview,
                    pattern: task.compiled.pattern().to_owned(),
                },
                Effect::FileFilter(task) => EffectOutcome::FileFilter {
                    generation: task.generation.get(),
                    document_generation: task.document_generation.get(),
                    query: task.query,
                    preferred_file: task.preferred_file.map(FileId::get),
                    requested_rank: task.requested_rank,
                },
                Effect::CancelSearch => EffectOutcome::CancelSearch,
                Effect::CancelFileFilter => EffectOutcome::CancelFileFilter,
            })
            .collect()
    }

    fn apply_key(app: &mut AppState, key: KeyEvent) -> KeyOutcome {
        let effects = app.update(Event::Key(key)).unwrap();
        KeyOutcome {
            top: app.top_anchor(),
            current_context: app
                .current_row()
                .map(|row| (row.commit_id, row.file_id, row.hunk_id)),
            mode: mode_outcome(app),
            whitespace: app.whitespace(),
            following: app.is_following(),
            pending_line: app.pending_line,
            status: app.status().to_owned(),
            search_generation: app.search().generation().get(),
            effects: effect_outcomes(effects),
        }
    }

    fn assert_body_alias(
        label: &str,
        setup: fn(&mut AppState),
        canonical: KeyEvent,
        alias: KeyEvent,
    ) {
        let mut canonical_state = state();
        setup(&mut canonical_state);
        let expected = apply_key(&mut canonical_state, canonical);

        let mut alias_state = state();
        setup(&mut alias_state);
        let actual = apply_key(&mut alias_state, alias);
        assert_eq!(actual, expected, "documented alias: {label}");
    }

    fn assert_help_alias(
        label: &str,
        setup: fn(&mut AppState),
        canonical: KeyEvent,
        alias: KeyEvent,
    ) {
        let mut canonical_state = state();
        let _ = apply_key(&mut canonical_state, plain_key('h'));
        setup(&mut canonical_state);
        let expected = apply_key(&mut canonical_state, canonical);

        let mut alias_state = state();
        let _ = apply_key(&mut alias_state, plain_key('h'));
        setup(&mut alias_state);
        let actual = apply_key(&mut alias_state, alias);
        assert_eq!(actual, expected, "documented help alias: {label}");
    }

    fn assert_file_list_alias(
        label: &str,
        setup: fn(&mut AppState),
        canonical: KeyEvent,
        alias: KeyEvent,
    ) {
        let mut canonical_state = structure_state();
        let _ = apply_key(&mut canonical_state, special_key(KeyCode::Tab));
        setup(&mut canonical_state);
        let expected = apply_key(&mut canonical_state, canonical);

        let mut alias_state = structure_state();
        let _ = apply_key(&mut alias_state, special_key(KeyCode::Tab));
        setup(&mut alias_state);
        let actual = apply_key(&mut alias_state, alias);
        assert_eq!(actual, expected, "documented file-list alias: {label}");
    }

    fn leave_at_start(_: &mut AppState) {}

    fn move_to_end(app: &mut AppState) {
        app.go_to_end().unwrap();
    }

    fn help_at_end(app: &mut AppState) {
        app.mode = Mode::Help {
            top: HELP_LINE_COUNT
                .saturating_sub(usize::from(app.terminal_height()).saturating_sub(1)),
        };
    }

    fn select_last_file(app: &mut AppState) {
        let Mode::FileList(list) = &mut app.mode else {
            panic!("expected file list");
        };
        let window = list.window.as_mut().expect("file-list window");
        let selected_rank = window.total_matches.checked_sub(1).expect("file match");
        let selected_file = window
            .file_at_rank(selected_rank)
            .expect("last file retained by test window");
        window.selected_rank = Some(selected_rank);
        list.selected_file = Some(selected_file);
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

    fn file_filter_task(effects: &[Effect]) -> FileFilterTask {
        effects
            .iter()
            .find_map(|effect| match effect {
                Effect::FileFilter(task) => Some(task.clone()),
                _ => None,
            })
            .expect("file-filter effect")
    }

    fn complete_file_filter(
        app: &mut AppState,
        task: &FileFilterTask,
        total_matches: u64,
        selected_rank: Option<u64>,
        start_rank: u64,
        file_ids: Vec<FileId>,
    ) {
        app.update(Event::FileFilterCompleted {
            generation: task.generation,
            document_generation: task.document_generation,
            total_matches,
            selected_rank,
            start_rank,
            file_ids,
        })
        .unwrap();
    }

    fn state() -> AppState {
        let document = parse_bytes(
            b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,3 @@\n one\n-two\n+three\n four\n",
        )
        .unwrap();
        AppState::new(Arc::new(document), 20, 7, 4, WhitespaceChoice::Ignore, true).unwrap()
    }

    fn structure_state() -> AppState {
        let paths = [
            "src/alpha.rs",
            "src/bravo.rs",
            "src/charlie.rs",
            "tests/delta.rs",
            "docs/echo.md",
            "scripts/foxtrot.sh",
        ];
        let mut input = String::new();
        for commit_index in 0..3 {
            input.push_str(&format!("commit {:040x}\n", commit_index + 1));
            input.push_str(&format!("    subject {commit_index}\n"));
            for file_offset in 0..2 {
                let file_index = commit_index * 2 + file_offset;
                let path = paths[file_index];
                input.push_str(&format!(
                    "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n"
                ));
                if file_index == paths.len() - 1 {
                    input.push_str("@@ -1,8 +1,8 @@\n-old\n+new\n");
                    for context in 1..8 {
                        input.push_str(&format!(" context {context}\n"));
                    }
                } else {
                    input.push_str("@@ -1 +1 @@\n-old\n+new\n");
                }
            }
        }
        let document = parse_bytes(input.as_bytes()).unwrap();
        AppState::new(Arc::new(document), 60, 7, 4, WhitespaceChoice::Ignore, true).unwrap()
    }

    fn long_state() -> AppState {
        let input: String = (0..40).map(|line| format!("line {line:02}\n")).collect();
        let document = parse_bytes(input.as_bytes()).unwrap();
        AppState::new(Arc::new(document), 60, 8, 4, WhitespaceChoice::Ignore, true).unwrap()
    }

    fn many_file_state(file_count: usize) -> AppState {
        let mut input = String::new();
        for ordinal in 0..file_count {
            input.push_str(&format!(
                "diff --git a/file-{ordinal:05}.rs b/file-{ordinal:05}.rs\n"
            ));
        }
        let document = parse_bytes(input.as_bytes()).unwrap();
        AppState::new(Arc::new(document), 60, 8, 4, WhitespaceChoice::Ignore, true).unwrap()
    }

    fn move_to_last_input_line(app: &mut AppState) {
        let last = app
            .document
            .line_id_at(app.document.line_count() - 1)
            .unwrap();
        let _ = app
            .install_anchor(VisualAnchor::new(last, 0), false)
            .unwrap();
    }

    #[derive(Clone, Copy)]
    enum StructureTarget {
        Hunk(usize),
        File(usize),
        Commit(usize),
    }

    fn structure_target(app: &AppState, target: StructureTarget) -> (VisualAnchor, RowContext) {
        let snapshot = app.document.structure();
        let mut reader = StructureMetadataReader::new();
        match target {
            StructureTarget::Hunk(index) => {
                let ordinal = u64::try_from(index).unwrap();
                let hunk_id = snapshot.hunk_id_at(ordinal).expect("hunk ordinal");
                let hunk = reader
                    .read_hunk(snapshot, hunk_id)
                    .unwrap()
                    .expect("hunk metadata");
                let file = reader
                    .read_file(snapshot, hunk.file_id)
                    .unwrap()
                    .expect("file metadata");
                (
                    VisualAnchor::new(hunk.header_line, 0),
                    (Some(file.commit_id), Some(file.id), Some(hunk.id)),
                )
            }
            StructureTarget::File(index) => {
                let ordinal = u64::try_from(index).unwrap();
                let file_id = snapshot.file_id_at(ordinal).expect("file ordinal");
                let file = reader
                    .read_file(snapshot, file_id)
                    .unwrap()
                    .expect("file metadata");
                (
                    VisualAnchor::new(file.span.start, 0),
                    (Some(file.commit_id), Some(file.id), None),
                )
            }
            StructureTarget::Commit(index) => {
                let ordinal = u64::try_from(index).unwrap();
                let commit_id = snapshot.commit_id_at(ordinal).expect("commit ordinal");
                let commit = reader
                    .read_commit(snapshot, commit_id)
                    .unwrap()
                    .expect("commit metadata");
                (
                    VisualAnchor::new(commit.span.start, 0),
                    (Some(commit.id), None, None),
                )
            }
        }
    }

    fn assert_structure_key(
        label: &str,
        key: KeyEvent,
        setup: fn(&mut AppState),
        target: StructureTarget,
    ) {
        let mut app = structure_state();
        setup(&mut app);
        let before = app.top_anchor();
        let (expected_anchor, expected_context) = structure_target(&app, target);
        assert_ne!(before, Some(expected_anchor), "invalid setup: {label}");

        let _ = apply_key(&mut app, key);

        assert_ne!(
            app.top_anchor(),
            before,
            "documented key was a no-op: {label}"
        );
        assert_eq!(app.top_anchor(), Some(expected_anchor), "target: {label}");
        let current = app.current_row().expect("structure target row");
        assert_eq!(current.line_id, expected_anchor.line_id, "row: {label}");
        assert_eq!(
            (current.commit_id, current.file_id, current.hunk_id),
            expected_context,
            "context: {label}"
        );
    }

    #[derive(Clone, Copy, Debug)]
    enum InputMode {
        FileFilter,
        Search,
    }

    fn input_mode_state(mode: InputMode, initial: &str) -> AppState {
        let mut app = match mode {
            InputMode::FileFilter => structure_state(),
            InputMode::Search => state(),
        };
        match mode {
            InputMode::FileFilter => {
                let _ = apply_key(&mut app, special_key(KeyCode::Tab));
                let _ = apply_key(&mut app, plain_key('/'));
            }
            InputMode::Search => {
                let _ = apply_key(&mut app, plain_key('/'));
            }
        }
        if !initial.is_empty() {
            app.update(Event::Paste(initial.to_owned())).unwrap();
        }
        app
    }

    fn active_input(app: &AppState, mode: InputMode) -> &TextInput {
        match (mode, app.mode()) {
            (InputMode::FileFilter, Mode::FileList(list)) => {
                list.filter.as_ref().expect("active file filter")
            }
            (InputMode::Search, Mode::Search(search)) => &search.input,
            _ => panic!("unexpected input mode"),
        }
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
    fn documented_body_movement_alias_matrix_matches_canonical_keys() {
        // Aliasごとに独立したstateを作り、canonical commandが変更するanchor、mode、status、effectを
        // 一括比較する。BOF/EOFで同じno-opになるだけの比較を避け、上方向は末尾から開始する。
        let line_down = [
            ("e", plain_key('e')),
            ("Down", special_key(KeyCode::Down)),
            ("Enter", special_key(KeyCode::Enter)),
            ("Ctrl-E", ctrl_key('e')),
            ("Ctrl-N", ctrl_key('n')),
        ];
        for (label, alias) in line_down {
            assert_body_alias(label, leave_at_start, plain_key('j'), alias);
        }

        let line_up = [
            ("y", plain_key('y')),
            ("Up", special_key(KeyCode::Up)),
            ("Ctrl-Y", ctrl_key('y')),
            ("Ctrl-P", ctrl_key('p')),
        ];
        for (label, alias) in line_up {
            assert_body_alias(label, move_to_end, plain_key('k'), alias);
        }

        let page_down = [
            ("Space", plain_key(' ')),
            ("f", plain_key('f')),
            ("Ctrl-F", ctrl_key('f')),
            ("Ctrl-V", ctrl_key('v')),
        ];
        for (label, alias) in page_down {
            assert_body_alias(label, leave_at_start, special_key(KeyCode::PageDown), alias);
        }

        let page_up = [("b", plain_key('b')), ("Ctrl-B", ctrl_key('b'))];
        for (label, alias) in page_up {
            assert_body_alias(label, move_to_end, special_key(KeyCode::PageUp), alias);
        }

        assert_body_alias("Ctrl-D", leave_at_start, plain_key('d'), ctrl_key('d'));
        assert_body_alias("Ctrl-U", move_to_end, plain_key('u'), ctrl_key('u'));
    }

    #[test]
    fn documented_body_command_alias_matrix_matches_canonical_keys() {
        assert_body_alias(
            "Home",
            move_to_end,
            plain_key('g'),
            special_key(KeyCode::Home),
        );
        assert_body_alias(
            "End",
            leave_at_start,
            shifted_key('G'),
            special_key(KeyCode::End),
        );
        assert_body_alias("H", leave_at_start, plain_key('h'), shifted_key('H'));
    }

    #[test]
    fn documented_help_scrolling_matrix_matches_body_navigation() {
        for (label, alias) in [
            ("e", plain_key('e')),
            ("Down", special_key(KeyCode::Down)),
            ("Enter", special_key(KeyCode::Enter)),
            ("Ctrl-E", ctrl_key('e')),
            ("Ctrl-N", ctrl_key('n')),
        ] {
            assert_help_alias(label, leave_at_start, plain_key('j'), alias);
        }
        for (label, alias) in [
            ("y", plain_key('y')),
            ("Up", special_key(KeyCode::Up)),
            ("Ctrl-Y", ctrl_key('y')),
            ("Ctrl-P", ctrl_key('p')),
        ] {
            assert_help_alias(label, help_at_end, plain_key('k'), alias);
        }
        for (label, alias) in [
            ("Space", plain_key(' ')),
            ("f", plain_key('f')),
            ("Ctrl-F", ctrl_key('f')),
            ("Ctrl-V", ctrl_key('v')),
        ] {
            assert_help_alias(label, leave_at_start, special_key(KeyCode::PageDown), alias);
        }
        for (label, alias) in [("b", plain_key('b')), ("Ctrl-B", ctrl_key('b'))] {
            assert_help_alias(label, help_at_end, special_key(KeyCode::PageUp), alias);
        }
        assert_help_alias("Ctrl-D", leave_at_start, plain_key('d'), ctrl_key('d'));
        assert_help_alias("Ctrl-U", help_at_end, plain_key('u'), ctrl_key('u'));
        assert_help_alias(
            "Home",
            help_at_end,
            plain_key('g'),
            special_key(KeyCode::Home),
        );
        assert_help_alias(
            "End",
            leave_at_start,
            shifted_key('G'),
            special_key(KeyCode::End),
        );

        for key in [
            plain_key('['),
            plain_key(']'),
            shifted_key('{'),
            shifted_key('}'),
            shifted_key('('),
            shifted_key(')'),
        ] {
            let mut app = state();
            let _ = apply_key(&mut app, plain_key('h'));
            let _ = apply_key(&mut app, plain_key('j'));
            let before = mode_outcome(&app);
            let outcome = apply_key(&mut app, key);
            assert_eq!(outcome.mode, before, "structure key must not leave help");
        }
    }

    #[test]
    fn documented_structure_key_matrix_uses_declared_direction() {
        struct Case {
            label: &'static str,
            key: KeyEvent,
            setup: fn(&mut AppState),
            target: StructureTarget,
        }

        let cases = [
            Case {
                label: "] next hunk",
                key: plain_key(']'),
                setup: leave_at_start,
                target: StructureTarget::Hunk(0),
            },
            Case {
                label: "[ previous hunk",
                key: plain_key('['),
                setup: move_to_last_input_line,
                target: StructureTarget::Hunk(5),
            },
            Case {
                label: "} next file",
                key: shifted_key('}'),
                setup: leave_at_start,
                target: StructureTarget::File(0),
            },
            Case {
                label: "{ previous file",
                key: shifted_key('{'),
                setup: move_to_last_input_line,
                target: StructureTarget::File(5),
            },
            Case {
                label: ") next commit",
                key: shifted_key(')'),
                setup: leave_at_start,
                target: StructureTarget::Commit(1),
            },
            Case {
                label: "( previous commit",
                key: shifted_key('('),
                setup: move_to_last_input_line,
                target: StructureTarget::Commit(2),
            },
        ];

        for case in cases {
            assert_structure_key(case.label, case.key, case.setup, case.target);
        }
    }

    #[test]
    fn documented_file_list_navigation_matrix_matches_declared_keys() {
        for (label, alias) in [("e", plain_key('e')), ("Down", special_key(KeyCode::Down))] {
            assert_file_list_alias(label, leave_at_start, plain_key('j'), alias);
        }
        for (label, alias) in [("y", plain_key('y')), ("Up", special_key(KeyCode::Up))] {
            assert_file_list_alias(label, select_last_file, plain_key('k'), alias);
        }
        for (label, alias) in [
            ("Space", plain_key(' ')),
            ("f", plain_key('f')),
            ("Ctrl-F", ctrl_key('f')),
            ("Ctrl-V", ctrl_key('v')),
        ] {
            assert_file_list_alias(label, leave_at_start, special_key(KeyCode::PageDown), alias);
        }
        for (label, alias) in [("b", plain_key('b')), ("Ctrl-B", ctrl_key('b'))] {
            assert_file_list_alias(label, select_last_file, special_key(KeyCode::PageUp), alias);
        }

        let mut filtered = input_mode_state(InputMode::FileFilter, "");
        for (key, expected) in [
            (special_key(KeyCode::Down), 1),
            (special_key(KeyCode::PageDown), 4),
            (special_key(KeyCode::Up), 3),
            (special_key(KeyCode::PageUp), 0),
        ] {
            let outcome = apply_key(&mut filtered, key);
            assert!(matches!(
                outcome.mode,
                ModeOutcome::FileList {
                    selected_rank: Some(selected),
                    ..
                } if selected == expected
            ));
        }
    }

    #[test]
    fn documented_file_filter_input_enter_and_escape_matrix() {
        for (label, key, expected) in [
            ("q", plain_key('q'), "q"),
            ("j", plain_key('j'), "j"),
            ("k", plain_key('k'), "k"),
            ("e", plain_key('e'), "e"),
            ("y", plain_key('y'), "y"),
            ("W", shifted_key('W'), "W"),
            ("?", shifted_key('?'), "?"),
            ("/", plain_key('/'), "/"),
        ] {
            let mut app = input_mode_state(InputMode::FileFilter, "");
            let _ = apply_key(&mut app, key);
            assert_eq!(
                active_input(&app, InputMode::FileFilter).text(),
                expected,
                "filter printable key: {label}"
            );
        }

        let mut jump = input_mode_state(InputMode::FileFilter, "");
        let effects = jump.update(Event::Paste("foxtrot".to_owned())).unwrap();
        let task = file_filter_task(&effects);
        complete_file_filter(&mut jump, &task, 1, Some(0), 0, vec![FileId::new(5)]);
        let (target, context) = structure_target(&jump, StructureTarget::File(5));
        let outcome = apply_key(&mut jump, special_key(KeyCode::Enter));
        assert_eq!(outcome.mode, ModeOutcome::Body);
        assert_eq!(jump.top_anchor(), Some(target));
        let current = jump.current_row().expect("selected file target");
        assert_eq!(current.line_id, target.line_id);
        assert_eq!(
            (current.commit_id, current.file_id, current.hunk_id),
            context
        );

        let mut escape = input_mode_state(InputMode::FileFilter, "alpha");
        let first = apply_key(&mut escape, special_key(KeyCode::Esc));
        assert!(matches!(
            first.mode,
            ModeOutcome::FileList {
                total_matches: Some(6),
                selected_rank: Some(0),
                start_rank: Some(0),
                file_ids,
                filter: None,
                ..
            } if file_ids == (0..6).collect::<Vec<_>>()
        ));
        let second = apply_key(&mut escape, special_key(KeyCode::Esc));
        assert_eq!(second.mode, ModeOutcome::Body);

        for close in [
            special_key(KeyCode::Tab),
            special_key(KeyCode::Esc),
            plain_key('q'),
        ] {
            let mut app = structure_state();
            let _ = apply_key(&mut app, special_key(KeyCode::Tab));
            assert_eq!(apply_key(&mut app, close).mode, ModeOutcome::Body);
        }
    }

    #[test]
    fn documented_filter_and_search_editing_key_matrix() {
        struct Case {
            label: &'static str,
            initial: &'static str,
            prelude: Option<KeyEvent>,
            edit: KeyEvent,
            insert_after: bool,
            expected: &'static str,
            cursor: usize,
        }

        for mode in [InputMode::FileFilter, InputMode::Search] {
            let cases = [
                Case {
                    label: "Left",
                    initial: "abcd",
                    prelude: None,
                    edit: special_key(KeyCode::Left),
                    insert_after: true,
                    expected: "abcxd",
                    cursor: 4,
                },
                Case {
                    label: "Ctrl-B",
                    initial: "abcd",
                    prelude: None,
                    edit: ctrl_key('b'),
                    insert_after: true,
                    expected: "abcxd",
                    cursor: 4,
                },
                Case {
                    label: "Right",
                    initial: "abcd",
                    prelude: Some(special_key(KeyCode::Home)),
                    edit: special_key(KeyCode::Right),
                    insert_after: true,
                    expected: "axbcd",
                    cursor: 2,
                },
                Case {
                    label: "Ctrl-F",
                    initial: "abcd",
                    prelude: Some(special_key(KeyCode::Home)),
                    edit: ctrl_key('f'),
                    insert_after: true,
                    expected: "axbcd",
                    cursor: 2,
                },
                Case {
                    label: "Home",
                    initial: "abcd",
                    prelude: None,
                    edit: special_key(KeyCode::Home),
                    insert_after: true,
                    expected: "xabcd",
                    cursor: 1,
                },
                Case {
                    label: "Ctrl-A",
                    initial: "abcd",
                    prelude: None,
                    edit: ctrl_key('a'),
                    insert_after: true,
                    expected: "xabcd",
                    cursor: 1,
                },
                Case {
                    label: "End",
                    initial: "abcd",
                    prelude: Some(special_key(KeyCode::Home)),
                    edit: special_key(KeyCode::End),
                    insert_after: true,
                    expected: "abcdx",
                    cursor: 5,
                },
                Case {
                    label: "Ctrl-E",
                    initial: "abcd",
                    prelude: Some(special_key(KeyCode::Home)),
                    edit: ctrl_key('e'),
                    insert_after: true,
                    expected: "abcdx",
                    cursor: 5,
                },
                Case {
                    label: "Backspace",
                    initial: "abcd",
                    prelude: None,
                    edit: special_key(KeyCode::Backspace),
                    insert_after: false,
                    expected: "abc",
                    cursor: 3,
                },
                Case {
                    label: "Delete",
                    initial: "abcd",
                    prelude: Some(special_key(KeyCode::Home)),
                    edit: special_key(KeyCode::Delete),
                    insert_after: false,
                    expected: "bcd",
                    cursor: 0,
                },
                Case {
                    label: "Ctrl-W",
                    initial: "alpha beta",
                    prelude: None,
                    edit: ctrl_key('w'),
                    insert_after: false,
                    expected: "alpha ",
                    cursor: 6,
                },
                Case {
                    label: "Ctrl-U",
                    initial: "abcd",
                    prelude: None,
                    edit: ctrl_key('u'),
                    insert_after: false,
                    expected: "",
                    cursor: 0,
                },
            ];

            for case in cases {
                let mut app = input_mode_state(mode, case.initial);
                if let Some(prelude) = case.prelude {
                    let _ = apply_key(&mut app, prelude);
                }
                let _ = apply_key(&mut app, case.edit);
                if case.insert_after {
                    let _ = apply_key(&mut app, plain_key('x'));
                }
                let input = active_input(&app, mode);
                assert_eq!(
                    (input.text(), input.cursor()),
                    (case.expected, case.cursor),
                    "documented {mode:?} edit key: {}",
                    case.label
                );
            }
        }
    }

    #[test]
    fn unsupported_search_input_keys_do_not_reissue_preview() {
        for (label, key) in [("F1", special_key(KeyCode::F(1))), ("Alt-x", alt_key('x'))] {
            let mut app = input_mode_state(InputMode::Search, "three");
            let generation = app.search.generation();
            let in_flight = app.in_flight_search.as_ref().expect("search preview task");
            let task_generation = in_flight.generation;
            let task_document_generation = in_flight.document_generation;

            let outcome = apply_key(&mut app, key);

            assert_eq!(outcome.status, "unknown key", "unsupported key: {label}");
            assert_eq!(outcome.search_generation, generation.get(), "key: {label}");
            assert_eq!(
                outcome.effects,
                vec![EffectOutcome::Redraw],
                "unsupported key must not reissue a preview: {label}"
            );
            assert_eq!(
                active_input(&app, InputMode::Search).text(),
                "three",
                "key: {label}"
            );
            assert!(app.in_flight_search.as_ref().is_some_and(|task| {
                task.generation == task_generation
                    && task.document_generation == task_document_generation
                    && task.preview
            }));
        }
    }

    #[test]
    fn cursor_only_and_boundary_search_edits_do_not_restart_the_worker() {
        let mut app = input_mode_state(InputMode::Search, "three");
        let generation = app.search.generation();
        let task = app.in_flight_search.clone().expect("search preview task");

        for key in [
            special_key(KeyCode::Left),
            special_key(KeyCode::Home),
            special_key(KeyCode::Backspace),
            special_key(KeyCode::Up),
        ] {
            let outcome = apply_key(&mut app, key);
            assert_eq!(outcome.effects, vec![EffectOutcome::Redraw]);
            assert_eq!(app.search.generation(), generation);
            assert!(app.in_flight_search.as_ref().is_some_and(|current| {
                current.generation == task.generation
                    && current.document_generation == task.document_generation
            }));
        }
        assert_eq!(active_input(&app, InputMode::Search).text(), "three");
    }

    #[test]
    fn unsupported_filter_input_keys_do_not_reissue_filter() {
        for (label, key) in [("F1", special_key(KeyCode::F(1))), ("Alt-x", alt_key('x'))] {
            let mut app = input_mode_state(InputMode::FileFilter, "alpha");
            let generation = app.file_filter_generation;
            let in_flight = app
                .in_flight_file_filter
                .as_ref()
                .expect("file-filter task");
            let task_generation = in_flight.generation;
            let task_document_generation = in_flight.document_generation;

            let outcome = apply_key(&mut app, key);

            assert_eq!(outcome.status, "unknown key", "unsupported key: {label}");
            assert_eq!(
                outcome.effects,
                vec![EffectOutcome::Redraw],
                "unsupported key must not reissue a filter: {label}"
            );
            assert_eq!(
                active_input(&app, InputMode::FileFilter).text(),
                "alpha",
                "key: {label}"
            );
            assert_eq!(app.file_filter_generation, generation, "key: {label}");
            assert!(app.in_flight_file_filter.as_ref().is_some_and(|task| {
                task.generation == task_generation
                    && task.document_generation == task_document_generation
                    && task.query == "alpha"
            }));
        }
    }

    #[test]
    fn global_control_keys_bypass_search_and_filter_inputs() {
        for mode in [InputMode::FileFilter, InputMode::Search] {
            for (label, key, expected) in [
                ("Ctrl-L", ctrl_key('l'), EffectOutcome::Repaint),
                ("Ctrl-Z", ctrl_key('z'), EffectOutcome::Suspend),
                ("Ctrl-C", ctrl_key('c'), EffectOutcome::Quit(130)),
            ] {
                let mut app = input_mode_state(mode, "three");
                let search_generation = app.search.generation();
                let filter_generation = app.file_filter_generation;

                let outcome = apply_key(&mut app, key);

                assert_eq!(outcome.effects, vec![expected], "{mode:?}: {label}");
                assert_eq!(
                    active_input(&app, mode).text(),
                    "three",
                    "{mode:?}: {label}"
                );
                assert_eq!(
                    app.search.generation(),
                    search_generation,
                    "{mode:?}: {label}"
                );
                assert_eq!(
                    app.file_filter_generation, filter_generation,
                    "{mode:?}: {label}"
                );
            }
        }
    }

    #[test]
    fn invalidated_background_intents_emit_explicit_worker_cancellation() {
        let mut search = input_mode_state(InputMode::Search, "three");
        let effects = search.update(special(KeyCode::Esc)).unwrap();
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::CancelSearch))
        );
        assert!(search.in_flight_search.is_none());

        let mut invalid = input_mode_state(InputMode::Search, "three");
        let effects = invalid.update(Event::Paste("[".to_owned())).unwrap();
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::CancelSearch))
        );
        assert!(invalid.in_flight_search.is_none());

        let mut filter = input_mode_state(InputMode::FileFilter, "alpha");
        let effects = filter.update(special(KeyCode::Esc)).unwrap();
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::CancelFileFilter))
        );

        let mut close = input_mode_state(InputMode::FileFilter, "alpha");
        let effects = close.update(special(KeyCode::Tab)).unwrap();
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::CancelFileFilter))
        );
    }

    #[test]
    fn documented_search_and_repeat_direction_matrix() {
        for (label, key, expected) in [
            ("/", plain_key('/'), SearchDirection::Forward),
            ("?", shifted_key('?'), SearchDirection::Backward),
        ] {
            let mut app = state();
            let outcome = apply_key(&mut app, key);
            assert_eq!(
                outcome.mode,
                ModeOutcome::Search {
                    direction: expected,
                    input: String::new(),
                    count: 1,
                },
                "documented key: {label}"
            );
        }

        for (active, key, expected) in [
            (
                SearchDirection::Forward,
                plain_key('n'),
                SearchDirection::Forward,
            ),
            (
                SearchDirection::Forward,
                shifted_key('N'),
                SearchDirection::Backward,
            ),
            (
                SearchDirection::Backward,
                plain_key('n'),
                SearchDirection::Backward,
            ),
            (
                SearchDirection::Backward,
                shifted_key('N'),
                SearchDirection::Forward,
            ),
        ] {
            let mut app = state();
            app.search
                .activate(compile_smart_case("three").unwrap(), active);
            let outcome = apply_key(&mut app, key);
            assert!(matches!(
                outcome.effects.as_slice(),
                [EffectOutcome::Search {
                    direction,
                    count: 1,
                    preview: false,
                    pattern,
                }] if *direction == expected && pattern == "three"
            ));
        }
    }

    #[test]
    fn documented_search_enter_and_escape_restore_the_declared_state() {
        let mut committed = state();
        let _ = apply_key(&mut committed, plain_key('/'));
        committed.update(Event::Paste("three".to_owned())).unwrap();
        let outcome = apply_key(&mut committed, special_key(KeyCode::Enter));
        assert_eq!(outcome.mode, ModeOutcome::Body);
        assert!(matches!(
            outcome.effects.as_slice(),
            [EffectOutcome::Search {
                direction: SearchDirection::Forward,
                count: 1,
                preview: false,
                pattern,
            }] if pattern == "three"
        ));

        let mut cancelled = state();
        let origin = cancelled.top_anchor();
        let _ = apply_key(&mut cancelled, plain_key('/'));
        let preview = search_task(&cancelled.update(Event::Paste("three".to_owned())).unwrap());
        let result = cancelled.execute_search_task(&preview).unwrap();
        cancelled
            .update(Event::SearchCompleted {
                generation: preview.generation,
                document_generation: preview.document_generation,
                result,
                preview: true,
            })
            .unwrap();
        assert_ne!(cancelled.top_anchor(), origin);

        let outcome = apply_key(&mut cancelled, special_key(KeyCode::Esc));
        assert_eq!(outcome.mode, ModeOutcome::Body);
        assert_eq!(cancelled.top_anchor(), origin);
    }

    #[test]
    fn documented_overlay_toggle_and_global_key_matrix() {
        let mut file_list = state();
        let file_list = apply_key(&mut file_list, special_key(KeyCode::Tab));
        assert!(matches!(file_list.mode, ModeOutcome::FileList { .. }));

        let mut file_list_close = state();
        let _ = apply_key(&mut file_list_close, special_key(KeyCode::Tab));
        let file_list_close = apply_key(&mut file_list_close, special_key(KeyCode::Tab));
        assert_eq!(file_list_close.mode, ModeOutcome::Body);

        let mut help = state();
        let help = apply_key(&mut help, plain_key('h'));
        assert_eq!(help.mode, ModeOutcome::Help { top: 0 });

        for (label, close) in [
            ("Esc", special_key(KeyCode::Esc)),
            ("q", plain_key('q')),
            ("h", plain_key('h')),
            ("H", shifted_key('H')),
        ] {
            let mut help_close = state();
            let _ = apply_key(&mut help_close, plain_key('h'));
            let help_close = apply_key(&mut help_close, close);
            assert_eq!(
                help_close.mode,
                ModeOutcome::Body,
                "help close key: {label}"
            );
        }

        let mut whitespace = state();
        let whitespace = apply_key(&mut whitespace, shifted_key('W'));
        assert_eq!(whitespace.whitespace, WhitespaceChoice::Show);

        let global = [
            ("Ctrl-L", ctrl_key('l'), EffectOutcome::Repaint),
            ("Ctrl-Z", ctrl_key('z'), EffectOutcome::Suspend),
            ("q", plain_key('q'), EffectOutcome::Quit(0)),
            ("Ctrl-C", ctrl_key('c'), EffectOutcome::Quit(130)),
        ];
        for (label, key, expected) in global {
            let mut app = state();
            let outcome = apply_key(&mut app, key);
            assert_eq!(outcome.effects, vec![expected], "documented key: {label}");
        }

        let mut cancelled = state();
        let _ = apply_key(&mut cancelled, plain_key('4'));
        let cancelled = apply_key(&mut cancelled, special_key(KeyCode::Esc));
        assert_eq!(cancelled.mode, ModeOutcome::Body);
        assert_eq!(cancelled.status, "cancelled");
        assert_eq!(cancelled.effects, vec![EffectOutcome::Redraw]);
    }

    #[test]
    fn documented_numeric_prefix_matrix() {
        let mut counted_rows = long_state();
        let mut repeated_rows = long_state();
        let _ = apply_key(&mut counted_rows, plain_key('3'));
        let counted_rows = apply_key(&mut counted_rows, plain_key('j'));
        for _ in 0..3 {
            let _ = apply_key(&mut repeated_rows, plain_key('j'));
        }
        assert_eq!(counted_rows.top, repeated_rows.top_anchor());
        assert_eq!(counted_rows.top.map(|anchor| anchor.line_id.get()), Some(3));

        let mut counted_page = long_state();
        let mut repeated_page = long_state();
        let _ = apply_key(&mut counted_page, plain_key('2'));
        let counted_page = apply_key(&mut counted_page, special_key(KeyCode::PageDown));
        for _ in 0..2 {
            let _ = apply_key(&mut repeated_page, special_key(KeyCode::PageDown));
        }
        assert_eq!(counted_page.top, repeated_page.top_anchor());
        assert_eq!(
            counted_page.top.map(|anchor| anchor.line_id.get()),
            Some(10)
        );

        for (label, key, target) in [
            ("2]", plain_key(']'), StructureTarget::Hunk(1)),
            ("2}", shifted_key('}'), StructureTarget::File(1)),
            ("2)", shifted_key(')'), StructureTarget::Commit(2)),
        ] {
            let mut app = structure_state();
            let (expected_anchor, expected_context) = structure_target(&app, target);
            let _ = apply_key(&mut app, plain_key('2'));
            let _ = apply_key(&mut app, key);
            assert_eq!(app.top_anchor(), Some(expected_anchor), "target: {label}");
            let current = app.current_row().expect("counted structure target row");
            assert_eq!(current.line_id, expected_anchor.line_id, "row: {label}");
            assert_eq!(
                (current.commit_id, current.file_id, current.hunk_id),
                expected_context,
                "context: {label}"
            );
        }

        for command in [plain_key('g'), shifted_key('G')] {
            let mut app = long_state();
            let _ = apply_key(&mut app, plain_key('3'));
            let outcome = apply_key(&mut app, command);
            assert_eq!(outcome.top.map(|anchor| anchor.line_id.get()), Some(2));
        }

        for (key, direction) in [
            (plain_key('/'), SearchDirection::Forward),
            (shifted_key('?'), SearchDirection::Backward),
        ] {
            let mut app = state();
            let _ = apply_key(&mut app, plain_key('2'));
            let outcome = apply_key(&mut app, key);
            assert_eq!(
                outcome.mode,
                ModeOutcome::Search {
                    direction,
                    input: String::new(),
                    count: 2,
                }
            );
        }

        for (digits, key, direction, count) in [
            ('3', plain_key('n'), SearchDirection::Forward, 3),
            ('4', shifted_key('N'), SearchDirection::Backward, 4),
        ] {
            let mut app = state();
            app.search.activate(
                compile_smart_case("three").unwrap(),
                SearchDirection::Forward,
            );
            let _ = apply_key(&mut app, plain_key(digits));
            let outcome = apply_key(&mut app, key);
            assert!(matches!(
                outcome.effects.as_slice(),
                [EffectOutcome::Search {
                    direction: actual_direction,
                    count: actual_count,
                    preview: false,
                    pattern,
                }] if *actual_direction == direction && *actual_count == count && pattern == "three"
            ));
        }
    }

    #[test]
    fn documented_prefix_discard_commands_do_not_leak_the_count() {
        let mut one_row = state();
        let expected_top = apply_key(&mut one_row, plain_key('j')).top;

        let mut whitespace = state();
        let _ = apply_key(&mut whitespace, plain_key('4'));
        let counted = apply_key(&mut whitespace, shifted_key('W'));
        assert_eq!(counted.mode, ModeOutcome::Body);
        assert_eq!(counted.whitespace, WhitespaceChoice::Show);
        assert_eq!(counted.status, "count ignored; whitespace: show");
        assert_eq!(apply_key(&mut whitespace, plain_key('j')).top, expected_top);

        let mut file_list = state();
        let _ = apply_key(&mut file_list, plain_key('4'));
        let counted = apply_key(&mut file_list, special_key(KeyCode::Tab));
        assert!(matches!(counted.mode, ModeOutcome::FileList { .. }));
        assert_eq!(counted.status, "count ignored for file list");
        let _ = apply_key(&mut file_list, special_key(KeyCode::Tab));
        assert_eq!(apply_key(&mut file_list, plain_key('j')).top, expected_top);

        let mut help = state();
        let _ = apply_key(&mut help, plain_key('4'));
        let counted = apply_key(&mut help, plain_key('h'));
        assert_eq!(counted.mode, ModeOutcome::Help { top: 0 });
        assert_eq!(counted.status, "count ignored for help");
        let _ = apply_key(&mut help, plain_key('h'));
        assert_eq!(apply_key(&mut help, plain_key('j')).top, expected_top);

        let mut quit = state();
        let _ = apply_key(&mut quit, plain_key('4'));
        let counted = apply_key(&mut quit, plain_key('q'));
        assert_eq!(counted.mode, ModeOutcome::Body);
        assert_eq!(counted.status, "count discarded");
        assert_eq!(counted.effects, vec![EffectOutcome::Redraw]);
        assert_eq!(apply_key(&mut quit, plain_key('j')).top, expected_top);
        assert_eq!(
            apply_key(&mut quit, plain_key('q')).effects,
            vec![EffectOutcome::Quit(0)]
        );
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
    fn resize_promotes_prepared_eager_rows_with_old_pins_inside_memory_partitions() {
        let line = "\tx".repeat(6_200);
        let input = std::iter::repeat_n(format!("{line}\n"), 10).collect::<String>();
        let document = Arc::new(parse_bytes(input.as_bytes()).unwrap());
        let mut app =
            AppState::new(document, 30_000, 12, 4, WhitespaceChoice::Ignore, true).unwrap();
        let before = app.viewport_layout_stats();
        assert_eq!(app.visible_rows().len(), 10);
        assert!(app.viewport_visible_bytes() <= 8 * 1024 * 1024);

        app.update(Event::Resize {
            width: 29_000,
            height: 12,
        })
        .unwrap();

        let after = app.viewport_layout_stats();
        let cache = app.viewport_cache_usage();
        assert_eq!(app.visible_rows().len(), 10);
        assert_eq!(after.decoded_lines, before.decoded_lines + 10);
        assert_eq!(after.layout_misses, before.layout_misses + 10);
        assert_eq!(
            after.cache_insert_rejections,
            before.cache_insert_rejections + 2
        );
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
        let (raw, mut parser) = streaming_parser();
        push(&raw, &mut parser, b"alpha\n");
        let mut app = AppState::new(
            Arc::new(parser.snapshot().unwrap()),
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

        push(&raw, &mut parser, b"needle\n");
        raw.seal().unwrap();
        let effects = app
            .update(Event::DocumentUpdated {
                document: Arc::new(parser.into_document().unwrap()),
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
        assert!(app.status().is_empty());
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
    fn eof_flag_must_match_document_completion_state() {
        let complete = Arc::new(parse_bytes(b"plain\n").unwrap());
        assert!(matches!(
            AppState::new(complete, 20, 7, 4, WhitespaceChoice::Ignore, false,),
            Err(StateError::DocumentCompletionMismatch {
                eof: false,
                document_complete: true
            })
        ));

        let (raw, mut parser) = streaming_parser();
        push(&raw, &mut parser, b"plain\n");
        let incomplete = Arc::new(parser.snapshot().unwrap());
        assert!(matches!(
            AppState::new(
                Arc::clone(&incomplete),
                20,
                7,
                4,
                WhitespaceChoice::Ignore,
                true,
            ),
            Err(StateError::DocumentCompletionMismatch {
                eof: true,
                document_complete: false
            })
        ));
        let mut app = AppState::new(
            Arc::clone(&incomplete),
            20,
            7,
            4,
            WhitespaceChoice::Ignore,
            false,
        )
        .unwrap();
        assert!(matches!(
            app.update(Event::DocumentUpdated {
                document: incomplete,
                eof: true,
            }),
            Err(StateError::DocumentCompletionMismatch {
                eof: true,
                document_complete: false
            })
        ));
    }

    #[test]
    fn unrelated_document_update_preserves_the_previous_snapshot_and_viewport() {
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

        assert!(matches!(error, StateError::UnrelatedDocumentUpdate));
        assert!(Arc::ptr_eq(&app.document, &previous_document));
        assert_eq!(app.viewport_anchor, previous_anchor);
        assert_eq!(app.viewport_rows, previous_rows);
        assert!(app.is_eof());
    }

    #[test]
    fn older_snapshot_from_the_same_parser_is_rejected() {
        let (raw, mut parser) = streaming_parser();
        push(&raw, &mut parser, b"first\n");
        let older = Arc::new(parser.snapshot().unwrap());
        push(&raw, &mut parser, b"second\n");
        let newer = Arc::new(parser.snapshot().unwrap());
        let mut app = AppState::new(newer, 20, 7, 4, WhitespaceChoice::Ignore, false).unwrap();

        let error = app
            .update(Event::DocumentUpdated {
                document: older,
                eof: false,
            })
            .unwrap_err();

        assert!(matches!(
            error,
            StateError::RegressiveDocumentLines {
                previous: 2,
                current: 1
            }
        ));
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
        let (raw, mut parser) = streaming_parser();
        push(&raw, &mut parser, initial);
        let mut app = AppState::new(
            Arc::new(parser.snapshot().unwrap()),
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
        assert_eq!(list.selected_file, Some(FileId::new(1)));
        assert_eq!(
            list.window.as_ref(),
            Some(&FileFilterWindow {
                total_matches: 2,
                selected_rank: Some(1),
                start_rank: 0,
                file_ids: vec![FileId::new(0), FileId::new(1)],
            })
        );

        push(&raw, &mut parser, &updated[initial.len()..]);
        raw.seal().unwrap();
        app.update(Event::DocumentUpdated {
            document: Arc::new(parser.into_document().unwrap()),
            eof: true,
        })
        .unwrap();
        let Mode::FileList(list) = app.mode() else {
            panic!("expected file list");
        };
        assert_eq!(list.selected_file, Some(FileId::new(1)));
        assert_eq!(
            list.window.as_ref(),
            Some(&FileFilterWindow {
                total_matches: 3,
                selected_rank: Some(1),
                start_rank: 0,
                file_ids: vec![FileId::new(0), FileId::new(1), FileId::new(2)],
            })
        );
    }

    #[test]
    fn unfiltered_file_list_retains_at_most_five_thousand_ids() {
        let mut app = many_file_state(FILE_FILTER_WINDOW_CAPACITY + 1);

        let effects = app.update(special(KeyCode::Tab)).unwrap();

        assert!(matches!(effects.as_slice(), [Effect::Redraw]));
        let Mode::FileList(list) = app.mode() else {
            panic!("expected file list");
        };
        let window = list.window.as_ref().expect("unfiltered window");
        assert_eq!(window.total_matches, 5_001);
        assert_eq!(window.selected_rank, Some(0));
        assert_eq!(window.start_rank, 0);
        assert_eq!(window.file_ids.len(), FILE_FILTER_WINDOW_CAPACITY);
        assert_eq!(window.file_ids.first(), Some(&FileId::new(0)));
        assert_eq!(window.file_ids.last(), Some(&FileId::new(4_999)));
    }

    #[test]
    fn empty_commit_items_are_navigable_across_multiple_overlay_pages() {
        let mut input = String::new();
        for ordinal in 0..24 {
            input.push_str(&format!(
                "commit {ordinal:040x}\n\n    empty {ordinal:02}\n"
            ));
        }
        let document = parse_bytes(input.as_bytes()).unwrap();
        let mut app =
            AppState::new(Arc::new(document), 60, 7, 4, WhitespaceChoice::Ignore, true).unwrap();

        app.update(special(KeyCode::Tab)).unwrap();
        let Mode::FileList(list) = app.mode() else {
            panic!("expected file list");
        };
        assert_eq!(
            list.selected_item,
            Some(FileListItem::EmptyCommit(CommitId::new(0)))
        );

        app.update(special(KeyCode::PageDown)).unwrap();
        app.update(special(KeyCode::PageDown)).unwrap();
        let Mode::FileList(list) = app.mode() else {
            panic!("expected file list");
        };
        assert_eq!(
            list.selected_item,
            Some(FileListItem::EmptyCommit(CommitId::new(6)))
        );

        app.update(key('G')).unwrap();
        let Mode::FileList(list) = app.mode() else {
            panic!("expected file list");
        };
        assert_eq!(
            list.selected_item,
            Some(FileListItem::EmptyCommit(CommitId::new(23)))
        );
        let expected = structure_target(&app, StructureTarget::Commit(23)).0;
        app.update(special(KeyCode::Enter)).unwrap();
        assert!(matches!(app.mode(), Mode::Body));
        assert_eq!(app.top_anchor(), Some(expected));
    }

    #[test]
    fn unfiltered_cursor_interleaves_files_and_long_empty_commit_gaps() {
        let mut input = String::from(
            "commit 0000000000000000000000000000000000000000\n\
             diff --git a/first.rs b/first.rs\n--- a/first.rs\n+++ b/first.rs\n",
        );
        for ordinal in 1..18 {
            input.push_str(&format!(
                "commit {ordinal:040x}\n\n    empty {ordinal:02}\n"
            ));
        }
        input.push_str(
            "commit 0000000000000000000000000000000000000012\n\
             diff --git a/last.rs b/last.rs\n--- a/last.rs\n+++ b/last.rs\n",
        );
        let document = parse_bytes(input.as_bytes()).unwrap();
        let mut app =
            AppState::new(Arc::new(document), 60, 7, 4, WhitespaceChoice::Ignore, true).unwrap();

        app.update(special(KeyCode::Tab)).unwrap();
        assert_eq!(
            app.selected_file_list_item(),
            Some(FileListItem::File(FileId::new(0)))
        );
        app.update(key('j')).unwrap();
        assert_eq!(
            app.selected_file_list_item(),
            Some(FileListItem::EmptyCommit(CommitId::new(1)))
        );
        app.update(key('G')).unwrap();
        assert_eq!(
            app.selected_file_list_item(),
            Some(FileListItem::File(FileId::new(1)))
        );
        app.update(key('k')).unwrap();
        assert_eq!(
            app.selected_file_list_item(),
            Some(FileListItem::EmptyCommit(CommitId::new(17)))
        );
    }

    #[test]
    fn streaming_open_commit_becomes_a_file_or_finalized_empty_item() {
        let (raw, mut parser) = streaming_parser();
        push(
            &raw,
            &mut parser,
            b"commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
        );
        let mut app = AppState::new(
            Arc::new(parser.snapshot().unwrap()),
            60,
            7,
            4,
            WhitespaceChoice::Ignore,
            false,
        )
        .unwrap();
        app.update(special(KeyCode::Tab)).unwrap();
        assert_eq!(app.selected_file_list_item(), None);
        app.update(key('j')).unwrap();
        assert_eq!(app.status(), "file list loading…");
        app.update(special(KeyCode::Enter)).unwrap();
        assert!(matches!(app.mode(), Mode::FileList(_)));
        assert_eq!(app.status(), "file list loading…");

        push(
            &raw,
            &mut parser,
            b"diff --git a/now.rs b/now.rs\n--- a/now.rs\n+++ b/now.rs\n",
        );
        app.update(Event::DocumentUpdated {
            document: Arc::new(parser.snapshot().unwrap()),
            eof: false,
        })
        .unwrap();
        assert_eq!(
            app.selected_file_list_item(),
            Some(FileListItem::File(FileId::new(0)))
        );

        let (raw, mut parser) = streaming_parser();
        push(
            &raw,
            &mut parser,
            b"commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
        );
        let initial = parser.snapshot().unwrap();
        let mut empty =
            AppState::new(Arc::new(initial), 60, 7, 4, WhitespaceChoice::Ignore, false).unwrap();
        empty.update(special(KeyCode::Tab)).unwrap();
        raw.seal().unwrap();
        let complete = parser.into_document().unwrap();
        empty
            .update(Event::DocumentUpdated {
                document: Arc::new(complete),
                eof: true,
            })
            .unwrap();
        assert_eq!(
            empty.selected_file_list_item(),
            Some(FileListItem::EmptyCommit(CommitId::new(0)))
        );

        let (raw, mut parser) = streaming_parser();
        push(
            &raw,
            &mut parser,
            b"commit cccccccccccccccccccccccccccccccccccccccc\n\
              commit dddddddddddddddddddddddddddddddddddddddd\n",
        );
        let mut preserved = AppState::new(
            Arc::new(parser.snapshot().unwrap()),
            60,
            7,
            4,
            WhitespaceChoice::Ignore,
            false,
        )
        .unwrap();
        preserved.update(special(KeyCode::Tab)).unwrap();
        assert_eq!(
            preserved.selected_file_list_item(),
            Some(FileListItem::EmptyCommit(CommitId::new(0)))
        );
        push(
            &raw,
            &mut parser,
            b"diff --git a/later.rs b/later.rs\n--- a/later.rs\n+++ b/later.rs\n",
        );
        preserved
            .update(Event::DocumentUpdated {
                document: Arc::new(parser.snapshot().unwrap()),
                eof: false,
            })
            .unwrap();
        assert_eq!(
            preserved.selected_file_list_item(),
            Some(FileListItem::EmptyCommit(CommitId::new(0)))
        );
    }

    #[test]
    fn filtered_enter_waits_for_the_current_query_result() {
        let mut app = structure_state();
        app.update(special(KeyCode::Tab)).unwrap();
        app.update(key('/')).unwrap();
        let top = app.top_anchor();
        let effects = app
            .update(Event::Paste("not-yet-returned".to_owned()))
            .unwrap();
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::FileFilter(_)))
        );
        let task = file_filter_task(&effects);

        app.update(special(KeyCode::Enter)).unwrap();
        assert!(matches!(app.mode(), Mode::FileList(_)));
        assert_eq!(app.top_anchor(), top);
        assert_eq!(app.status(), "file list loading…");

        app.update(Event::FileFilterFailed {
            generation: task.generation,
            document_generation: task.document_generation,
            message: "filter failed".to_owned(),
        })
        .unwrap();
        app.update(special(KeyCode::Enter)).unwrap();
        assert!(matches!(app.mode(), Mode::FileList(_)));
        assert_eq!(app.top_anchor(), top);
        assert_eq!(app.status(), "filter failed");
    }

    #[test]
    fn file_filter_paste_rejects_expansion_past_the_query_limit_without_mutation() {
        assert_eq!(MAX_FILE_FILTER_QUERY_BYTES, 2_097_157);
        let mut app = structure_state();
        app.update(special(KeyCode::Tab)).unwrap();
        app.update(key('/')).unwrap();
        let initial = "x".repeat(MAX_FILE_FILTER_QUERY_BYTES - 3);
        let generation = app.file_filter_generation;
        let Mode::FileList(list) = &mut app.mode else {
            panic!("expected file list");
        };
        list.filter.as_mut().unwrap().set(initial.clone());

        // NULはpath inputで4 bytesの`\\x00`へ展開するため、raw 1 byteではなくescape後の
        // query長で拒否する。拒否時はworker generationも進めない。
        let effects = app.update(Event::Paste("\0".to_owned())).unwrap();

        assert!(matches!(effects.as_slice(), [Effect::Redraw]));
        let Mode::FileList(list) = app.mode() else {
            panic!("expected file list");
        };
        assert_eq!(list.filter.as_ref().unwrap().text(), initial);
        assert_eq!(list.generation, generation);
        assert_eq!(app.file_filter_generation, generation);
        assert!(app.in_flight_file_filter.is_none());
        assert_eq!(
            app.status(),
            file_filter_query_limit_status(MAX_FILE_FILTER_QUERY_BYTES + 1)
        );
    }

    #[test]
    fn file_filter_typed_character_rejection_retains_the_current_worker_intent() {
        let mut app = structure_state();
        app.update(special(KeyCode::Tab)).unwrap();
        app.update(key('/')).unwrap();
        let initial = "x".repeat(MAX_FILE_FILTER_QUERY_BYTES - 4);
        let Mode::FileList(list) = &mut app.mode else {
            panic!("expected file list");
        };
        list.filter.as_mut().unwrap().set(initial);

        let accepted_effects = app.update(Event::Paste("\0".to_owned())).unwrap();
        let accepted_task = file_filter_task(&accepted_effects);
        assert_eq!(accepted_task.query.len(), MAX_FILE_FILTER_QUERY_BYTES);
        assert!(accepted_task.query.ends_with("\\x00"));

        let rejected_effects = app.update(key('é')).unwrap();

        assert!(matches!(rejected_effects.as_slice(), [Effect::Redraw]));
        let Mode::FileList(list) = app.mode() else {
            panic!("expected file list");
        };
        assert_eq!(
            list.filter.as_ref().unwrap().text(),
            accepted_task.query.as_str()
        );
        let retained = app.in_flight_file_filter.as_ref().unwrap();
        assert_eq!(retained.generation, accepted_task.generation);
        assert_eq!(
            retained.document_generation,
            accepted_task.document_generation
        );
        assert_eq!(retained.query, accepted_task.query);
        assert_eq!(
            app.status(),
            file_filter_query_limit_status(MAX_FILE_FILTER_QUERY_BYTES + 'é'.len_utf8())
        );
    }

    #[test]
    fn file_filter_result_requires_current_generations_and_an_open_overlay() {
        let mut app = structure_state();
        app.update(special(KeyCode::Tab)).unwrap();
        app.update(key('/')).unwrap();
        let task = file_filter_task(&app.update(Event::Paste("alpha".to_owned())).unwrap());
        let waiting = mode_outcome(&app);

        app.update(Event::FileFilterCompleted {
            generation: FileFilterGeneration(task.generation.get().wrapping_add(1)),
            document_generation: task.document_generation,
            total_matches: 1,
            selected_rank: Some(0),
            start_rank: 0,
            file_ids: vec![FileId::new(0)],
        })
        .unwrap();
        assert_eq!(mode_outcome(&app), waiting);

        app.update(Event::FileFilterCompleted {
            generation: task.generation,
            document_generation: DocumentGeneration(task.document_generation.get().wrapping_add(1)),
            total_matches: 1,
            selected_rank: Some(0),
            start_rank: 0,
            file_ids: vec![FileId::new(0)],
        })
        .unwrap();
        assert_eq!(mode_outcome(&app), waiting);

        complete_file_filter(&mut app, &task, 1, Some(0), 0, vec![FileId::new(0)]);
        assert!(matches!(
            mode_outcome(&app),
            ModeOutcome::FileList {
                selected_file: Some(0),
                total_matches: Some(1),
                selected_rank: Some(0),
                loading: false,
                ..
            }
        ));

        let replacement = file_filter_task(&app.update(Event::Paste("x".to_owned())).unwrap());
        app.update(special(KeyCode::Tab)).unwrap();
        let anchor = app.top_anchor();
        complete_file_filter(&mut app, &replacement, 0, None, 0, Vec::new());
        assert!(matches!(app.mode(), Mode::Body));
        assert_eq!(app.top_anchor(), anchor);
    }

    #[test]
    fn filtered_navigation_requests_a_new_window_after_crossing_its_rank_range() {
        let mut app = structure_state();
        app.update(special(KeyCode::Tab)).unwrap();
        app.update(key('/')).unwrap();
        let initial = file_filter_task(&app.update(Event::Paste("r".to_owned())).unwrap());
        complete_file_filter(
            &mut app,
            &initial,
            6,
            Some(1),
            0,
            vec![FileId::new(0), FileId::new(1)],
        );

        let effects = app.update(special(KeyCode::Down)).unwrap();
        let next = file_filter_task(&effects);
        assert_eq!(next.query, "r");
        assert_eq!(next.preferred_file, None);
        assert_eq!(next.requested_rank, Some(2));
        assert_eq!(next.document_generation, initial.document_generation);
        assert_ne!(next.generation, initial.generation);
        let Mode::FileList(list) = app.mode() else {
            panic!("expected file list");
        };
        assert!(list.loading);
        assert_eq!(list.generation, next.generation);
        assert_eq!(list.selected_file, Some(FileId::new(1)));
        assert_eq!(
            list.window.as_ref().and_then(|window| window.selected_rank),
            Some(1)
        );

        let document = Arc::clone(&app.document);
        let effects = app
            .update(Event::DocumentUpdated {
                document,
                eof: true,
            })
            .unwrap();
        let reissued = file_filter_task(&effects);
        assert_eq!(reissued.query, "r");
        assert_eq!(reissued.preferred_file, None);
        assert_eq!(reissued.requested_rank, Some(2));
        assert_ne!(reissued.document_generation, next.document_generation);
        assert_ne!(reissued.generation, next.generation);
    }

    #[test]
    fn returning_to_retained_filter_window_cancels_the_outstanding_rank_request() {
        let mut app = structure_state();
        app.update(special(KeyCode::Tab)).unwrap();
        app.update(key('/')).unwrap();
        let initial = file_filter_task(&app.update(Event::Paste("r".to_owned())).unwrap());
        complete_file_filter(
            &mut app,
            &initial,
            6,
            Some(1),
            0,
            vec![FileId::new(0), FileId::new(1)],
        );
        let outstanding = file_filter_task(&app.update(special(KeyCode::Down)).unwrap());
        assert_eq!(outstanding.requested_rank, Some(2));

        let effects = app.update(special(KeyCode::Up)).unwrap();
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::CancelFileFilter))
        );
        let Mode::FileList(list) = app.mode() else {
            panic!("expected file list");
        };
        assert_eq!(list.selected_file, Some(FileId::new(0)));
        assert_eq!(
            list.window.as_ref().and_then(|window| window.selected_rank),
            Some(0)
        );

        app.update(Event::FileFilterCompleted {
            generation: outstanding.generation,
            document_generation: outstanding.document_generation,
            total_matches: 6,
            selected_rank: Some(2),
            start_rank: 2,
            file_ids: vec![FileId::new(2)],
        })
        .unwrap();
        let Mode::FileList(list) = app.mode() else {
            panic!("expected file list");
        };
        assert_eq!(list.selected_file, Some(FileId::new(0)));
        assert_eq!(
            list.window.as_ref().and_then(|window| window.selected_rank),
            Some(0)
        );
    }

    #[test]
    fn text_input_edits_graphemes_without_splitting() {
        let mut input = TextInput::default();
        input.insert("a👩‍💻b");
        input.move_left();
        input.delete_previous();
        assert_eq!(input.text(), "ab");

        input.set("keep foo \u{301}");
        input.delete_previous_word();
        assert_eq!(input.text(), "keep ");
        assert_eq!(input.cursor(), "keep ".len());
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
    fn clamped_navigation_replaces_pending_line_and_forward_search_intents() {
        let mut pending_line = state();
        pending_line.eof = false;
        pending_line.update(key('9')).unwrap();
        pending_line.update(key('g')).unwrap();
        assert_eq!(pending_line.pending_line, Some(9));
        let generation = pending_line.search().generation();

        pending_line.update(key('g')).unwrap();
        assert_eq!(pending_line.pending_line, None);
        assert!(pending_line.search().generation() > generation);
        assert_eq!(pending_line.status(), "beginning of input");

        let mut pending_search = state();
        pending_search.eof = false;
        pending_search.update(key('/')).unwrap();
        let task = search_task(
            &pending_search
                .update(Event::Paste("not-present".to_owned()))
                .unwrap(),
        );
        let result = pending_search.execute_search_task(&task).unwrap();
        pending_search
            .update(Event::SearchCompleted {
                generation: task.generation,
                document_generation: task.document_generation,
                result,
                preview: true,
            })
            .unwrap();
        let committed = search_task(&pending_search.update(special(KeyCode::Enter)).unwrap());
        let result = pending_search.execute_search_task(&committed).unwrap();
        pending_search
            .update(Event::SearchCompleted {
                generation: committed.generation,
                document_generation: committed.document_generation,
                result,
                preview: false,
            })
            .unwrap();
        assert!(pending_search.search().pending_forward().is_some());

        pending_search.update(key('k')).unwrap();
        assert!(pending_search.search().pending_forward().is_none());
        assert_eq!(pending_search.status(), "beginning of input");

        let mut missing_search = state();
        missing_search.eof = false;
        missing_search.update(key('9')).unwrap();
        missing_search.update(key('g')).unwrap();
        assert_eq!(missing_search.pending_line, Some(9));
        let generation = missing_search.search().generation();

        missing_search.update(key('n')).unwrap();
        assert_eq!(missing_search.pending_line, None);
        assert!(missing_search.search().generation() > generation);
        assert_eq!(missing_search.status(), "no previous search");
    }

    #[test]
    fn search_match_near_eof_is_kept_at_the_body_top() {
        let mut app = state();
        app.update(key('/')).unwrap();
        let task = search_task(&app.update(Event::Paste("four".to_owned())).unwrap());
        let result = app.execute_search_task(&task).unwrap();
        let found = result.found.as_ref().unwrap().line_id;

        app.update(Event::SearchCompleted {
            generation: task.generation,
            document_generation: task.document_generation,
            result,
            preview: true,
        })
        .unwrap();

        assert_eq!(app.top_anchor().map(|anchor| anchor.line_id), Some(found));
        assert_eq!(
            app.visible_rows().first().map(|row| row.line_id),
            Some(found)
        );
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
