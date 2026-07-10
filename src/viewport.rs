//! Viewport周辺だけをdecodeしてvisual rowへ変換するlazy layout engine。
//!
//! [`ViewportLayout`]はdocument全体のrow indexを作らない。Top位置はstableな[`LineId`]、logical byte
//! offset、同じbyteを描くfragmentのordinalからなる[`VisualAnchor`]で保持し、移動またはframe収集で
//! 触れたlogical lineだけを[`DocumentLineReader`]でdecodeする。折り返しは
//! [`crate::direct::layout_line`]へ集約し、source lineのgutterはlineのfixed metadataに正規化済みの
//! file単位最大行番号を使う。Hot pathはpath等のblobを読まない。
//!
//! 折り返し結果はbyte weight付きLRUへ置く。Engineは[`DocumentSnapshot::identity`]が変わるとcacheを
//! 空にし、keyにはeffective kind、sticky metadata、行番号、gutter、terminal幅、tab幅を含める。同じ
//! input streamのappend-only snapshot間では再利用でき、file/hunkのfallback化、gutter桁増加、reflow
//! 条件変更では一致しない。Hit判定はfixed metadataだけを読み、line textのdecodeはmiss時だけ行う。

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::mem;
use std::ops::Deref;
use std::sync::Arc;

use thiserror::Error;

use crate::cache::{CacheHandle, CacheUsage, WeightedLruCache};
use crate::direct::{
    DocumentLayoutError, GutterSpec, LargeLineNavigator, LargeRowAnchor, LayoutError, LineLayout,
    MAX_STREAMING_VISUAL_ROW_BYTES, MAX_STREAMING_WINDOW_ROWS, VisualRow,
    eager_layout_allocation_upper_bound, eager_layout_decoded_allocation_upper_bound, layout_line,
    line_requires_streaming_layout,
};
use crate::document::{
    DocumentError, DocumentIdentity, DocumentLineMetadata, DocumentLineMetadataReader,
    DocumentLineReader, DocumentSnapshot,
};
use crate::large_line::{LargeLineError, LargeLineStats};
use crate::model::{CommitId, FileId, HunkId, LineId, LineKind};

/// Viewportの24 MiB shareからeager line-layout cacheへ割り当てる上限。
pub const DEFAULT_VIEWPORT_CACHE_BYTES: usize = 12 * 1024 * 1024;
/// Stateful巨大行navigator、current frame、prepared frameへ残す上限。
pub const DEFAULT_VIEWPORT_STREAM_BYTES: usize = 8 * 1024 * 1024;
/// Transaction中にcurrent/preparedの2 frameが同居するため、streaming 1 frameはpoolの半分へ制限する。
const VIEWPORT_STREAM_FRAME_BYTES: usize = DEFAULT_VIEWPORT_STREAM_BYTES / 2;
/// Line decode buffer、frame metadata、row/index bookkeepingへ残す上限。
pub const DEFAULT_VIEWPORT_TRANSIENT_BYTES: usize = 4 * 1024 * 1024;
/// Eager [`DocumentLineReader`]のraw、ANSI除去後、escaped text bufferへ予約する上限。
const VIEWPORT_EAGER_READER_BYTES: usize = 3 * 1024 * 1024;
/// Current/prepared frameが同時に生存するtransaction全体のrow handle上限。
const VIEWPORT_ROW_HANDLE_BYTES: usize =
    DEFAULT_VIEWPORT_TRANSIENT_BYTES - VIEWPORT_EAGER_READER_BYTES;
/// 2 frameのどちらか片方がtransient shareを使い切らないよう、1 frameを半分に制限する。
const VIEWPORT_ROW_HANDLE_FRAME_BYTES: usize = VIEWPORT_ROW_HANDLE_BYTES / 2;
/// Cacheとprepared eager candidateが共有する24 MiB partitionの非transient部分。
const VIEWPORT_EAGER_OWNED_BYTES: usize =
    DEFAULT_VIEWPORT_CACHE_BYTES + DEFAULT_VIEWPORT_STREAM_BYTES;
const MAX_VIEWPORT_ROW_HANDLES_PER_FRAME: usize = {
    let by_bytes = VIEWPORT_ROW_HANDLE_FRAME_BYTES / mem::size_of::<ViewportRow>();
    if by_bytes < MAX_STREAMING_WINDOW_ROWS {
        by_bytes
    } else {
        MAX_STREAMING_WINDOW_ROWS
    }
};

const _: () = assert!(
    DEFAULT_VIEWPORT_CACHE_BYTES + DEFAULT_VIEWPORT_STREAM_BYTES + DEFAULT_VIEWPORT_TRANSIENT_BYTES
        == crate::derived_memory::VIEWPORT_MEMORY_BYTES
);
const _: () = assert!(
    VIEWPORT_EAGER_READER_BYTES + 2 * VIEWPORT_ROW_HANDLE_FRAME_BYTES
        == DEFAULT_VIEWPORT_TRANSIENT_BYTES
);

/// Reflow後も位置を復元できるtop-row anchor。
///
/// `byte_offset`はsanitized logical lineのbyte offsetで、source lineではdiff markerをoffset 0として含む。
/// `fragment_ordinal`は同じbyteを描くvisual rowだけを表示順に数え、通常のrowでは0になる。Tab展開が
/// 複数rowへ分かれた場合も各fragmentを一意に指し、resize後にfragment数が減った場合は末尾ordinalへ
/// clampする。Byte offsetは内容位置を優先するため、fragment数が再び増えてもclamp前のordinalは復元しない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisualAnchor {
    pub line_id: LineId,
    pub byte_offset: usize,
    pub fragment_ordinal: usize,
}

impl VisualAnchor {
    /// Stable lineとlogical byte offsetからanchorを作る。
    #[must_use]
    pub const fn new(line_id: LineId, byte_offset: usize) -> Self {
        Self {
            line_id,
            byte_offset,
            fragment_ordinal: 0,
        }
    }

    /// 同じlogical byteを描くfragmentのordinalを付ける。
    #[must_use]
    pub const fn with_fragment_ordinal(mut self, fragment_ordinal: usize) -> Self {
        self.fragment_ordinal = fragment_ordinal;
        self
    }
}

/// 1回のviewport操作に適用するreflow条件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewportLayoutConfig {
    /// Gutterを含むterminalのcell幅。
    pub width: usize,
    /// Tab stop間のcell数。0は[`LayoutError::InvalidTabWidth`]になる。
    pub tab_width: usize,
}

impl ViewportLayoutConfig {
    /// Terminal幅とtab幅を束ねる。値の検証はdocument操作時に行う。
    #[must_use]
    pub const fn new(width: usize, tab_width: usize) -> Self {
        Self { width, tab_width }
    }
}

/// 収集済みviewportが保持する1 visual rowへの共有参照。
///
/// 通常lineはwrap cacheの[`LineLayout`]をpinし、row indexだけをframeへ複製する。Cacheへ収まらない
/// 小さなcustom budgetのtest経路でも[`LineLayout`]内部の`Arc`を共有し、`VisualRow`のtextとmappingを
/// frameごとに複製しない。
#[derive(Clone, Debug)]
pub struct ViewportRow {
    source: SharedVisualRow,
}

impl ViewportRow {
    fn new(layout: &LoadedLineLayout, row_index: usize) -> Self {
        Self {
            source: SharedVisualRow::Layout {
                layout: layout.shared(),
                row_index,
            },
        }
    }

    fn streamed(row: Arc<VisualRow>) -> Self {
        Self {
            source: SharedVisualRow::Streamed(row),
        }
    }

    /// 共有layout内のvisual rowを返す。
    #[must_use]
    pub fn row(&self) -> &VisualRow {
        match &self.source {
            SharedVisualRow::Layout { layout, row_index } => &layout.rows()[*row_index],
            SharedVisualRow::Streamed(row) => row,
        }
    }

    fn promote_prepared(
        &mut self,
        promoted: &HashMap<LineLayoutKey, CacheHandle<LineLayout>>,
    ) -> Result<(), ViewportLayoutError> {
        let SharedVisualRow::Layout { layout, .. } = &mut self.source else {
            return Ok(());
        };
        let SharedLineLayout::Prepared { key, .. } = layout else {
            return Ok(());
        };
        let cached = promoted
            .get(key)
            .cloned()
            .ok_or(ViewportLayoutError::CacheEntryUnavailable)?;
        *layout = SharedLineLayout::Cached(cached);
        Ok(())
    }
}

#[derive(Clone, Debug)]
enum SharedVisualRow {
    Layout {
        layout: SharedLineLayout,
        row_index: usize,
    },
    Streamed(Arc<VisualRow>),
}

#[derive(Clone, Debug)]
enum SharedLineLayout {
    Cached(CacheHandle<LineLayout>),
    Prepared {
        key: LineLayoutKey,
        layout: Arc<LineLayout>,
    },
}

impl SharedLineLayout {
    fn rows(&self) -> &[VisualRow] {
        match self {
            Self::Cached(layout) => layout.rows(),
            Self::Prepared { layout, .. } => layout.rows(),
        }
    }
}

impl Deref for ViewportRow {
    type Target = VisualRow;

    fn deref(&self) -> &Self::Target {
        self.row()
    }
}

impl PartialEq for ViewportRow {
    fn eq(&self, other: &Self) -> bool {
        self.row() == other.row()
    }
}

impl Eq for ViewportRow {}

/// 収集済みviewport rowsと、収集に使った正規化済みtop位置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewportRows {
    /// Empty documentでは`None`。それ以外では`rows`の先頭を含むanchor。
    pub top: Option<VisualAnchor>,
    /// Sticky headerが参照するcommit/file/hunk IDを保持したvisual rows。
    pub rows: Vec<ViewportRow>,
    /// `rows`末尾がsnapshot末尾のvisual rowまで達したか。
    pub at_end: bool,
    /// Streamed row windowが保持するpayloadの保守的byte数。Navigatorのblocked rowも含む。
    pub(crate) streaming_bytes: usize,
    /// Visible eager layoutsを重複なしで数えたbyte数。
    pub(crate) eager_bytes: usize,
    /// Visible share上限で収集を止めた場合にtrue。
    pub(crate) byte_limited: bool,
}

impl ViewportRows {
    /// Current frameのstreaming share使用量を返す。
    #[must_use]
    pub const fn streaming_bytes(&self) -> usize {
        self.streaming_bytes
    }

    /// Eager/streaming row payloadを合わせたframe使用量を返す。
    #[must_use]
    pub const fn visible_bytes(&self) -> usize {
        self.eager_bytes.saturating_add(self.streaming_bytes)
    }

    /// Visible shareへ達してbody heightより前に収集を止めたかを返す。
    #[must_use]
    pub const fn byte_limited(&self) -> bool {
        self.byte_limited
    }

    /// Row textを共有するhandle vectorが確保したbyte数を返す。
    ///
    /// [`ViewportLayout::collect`]が作るframeでは、state transaction中のold/new合計が
    /// [`VIEWPORT_ROW_HANDLE_BYTES`]以下になる。
    #[cfg(test)]
    #[must_use]
    pub(crate) fn row_handle_bytes(&self) -> usize {
        self.rows
            .capacity()
            .saturating_mul(mem::size_of::<ViewportRow>())
    }
}

/// Lazy decodeとwrap cacheのdeterministic counter。
///
/// `metadata_reads`はcache lookupのfixed-record read数、`decoded_lines`はcontent-aware allocation
/// preflightのためtextを読んだline数、`layout_misses`はその後[`layout_line`]まで実行したline数を
/// 数える。Frame残量またはcache admissionで止めたlineでは前者だけが増える。CounterはI/O量の
/// 受入testに使い、cache clearやdocument lineage変更では戻さない。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViewportLayoutStats {
    pub metadata_reads: u64,
    pub decoded_lines: u64,
    pub cache_hits: u64,
    pub layout_misses: u64,
    pub cache_insert_rejections: u64,
    /// Streamed line navigatorを新しいline/configへ作り直した回数。
    pub streaming_navigators_created: u64,
    /// 完了済みnavigatorと現在のnavigatorがraw storeから読んだbyte数。
    pub streaming_raw_bytes_read: u64,
    /// Streamed decode中に同時保持したdecoder bufferの最大byte数。
    pub streaming_peak_buffered_bytes: usize,
}

/// Viewport layoutがlineまたはvisual rowを返せなかった理由。
#[derive(Debug, Error)]
pub enum ViewportLayoutError {
    #[error(transparent)]
    Document(#[from] DocumentError),
    #[error(transparent)]
    Layout(#[from] LayoutError),
    #[error(transparent)]
    LargeLine(#[from] LargeLineError),
    #[error("document snapshot does not contain logical line {line_id}")]
    MissingLine { line_id: LineId },
    #[error("document snapshot cannot represent logical line index {index} as a stable ID")]
    UnavailableLineIndex { index: usize },
    #[error("logical line {line_id} produced no visual rows")]
    MissingVisualRows { line_id: LineId },
    #[error("a layout inserted into the viewport cache could not be read back")]
    CacheEntryUnavailable,
    #[error(
        "viewport cache cannot retain a {requested}-byte layout while {live} of {limit} bytes are live"
    )]
    CacheBudgetExhausted {
        requested: usize,
        live: usize,
        limit: usize,
    },
}

impl From<DocumentLayoutError> for ViewportLayoutError {
    fn from(error: DocumentLayoutError) -> Self {
        match error {
            DocumentLayoutError::Layout(error) => Self::Layout(error),
            DocumentLayoutError::Document(error) => Self::Document(error),
            DocumentLayoutError::LargeLine(error) => Self::LargeLine(error),
            DocumentLayoutError::MissingLine { line_id }
            | DocumentLayoutError::MissingVisualRows { line_id }
            | DocumentLayoutError::StreamingRequired { line_id } => Self::MissingLine { line_id },
            DocumentLayoutError::UnavailableLineIndex { index } => {
                Self::UnavailableLineIndex { index }
            }
        }
    }
}

/// 1つのinput streamに対するlazy viewport layout state。
///
/// Reader bufferとwrap cacheをframe間で再利用する。Public methodはdocumentを借用するだけでsnapshotを
/// 保持しないため、parserは同じengineへ新しいappend-only snapshotを渡せる。Cache accountingは
/// [`Self::cache_usage`]から取得でき、resident/live byteはいずれもconstructorの上限を超えない。
#[derive(Debug)]
pub struct ViewportLayout {
    identity: Option<DocumentIdentity>,
    metadata_reader: DocumentLineMetadataReader,
    reader: DocumentLineReader,
    cache: WeightedLruCache<LineLayoutKey, LineLayout>,
    large_navigator: Option<LargeNavigatorSlot>,
    completed_streaming_stats: LargeLineStats,
    preparing: bool,
    prepared_layouts: HashMap<LineLayoutKey, PreparedLineLayout>,
    prepared_owned_bytes: usize,
    stats: ViewportLayoutStats,
}

impl Default for ViewportLayout {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewportLayout {
    /// 12 MiBのeager cache、8 MiBのprepared/streaming share、4 MiBのdecode領域を持つengineを作る。
    #[must_use]
    pub fn new() -> Self {
        Self::with_cache_budget(DEFAULT_VIEWPORT_CACHE_BYTES)
    }

    /// Wrap cacheのresident/live weight上限を明示してengineを作る。
    #[must_use]
    pub fn with_cache_budget(cache_budget_bytes: usize) -> Self {
        Self {
            identity: None,
            metadata_reader: DocumentLineMetadataReader::new(),
            reader: DocumentLineReader::new(),
            cache: WeightedLruCache::new(cache_budget_bytes),
            large_navigator: None,
            completed_streaming_stats: LargeLineStats::default(),
            preparing: false,
            prepared_layouts: HashMap::new(),
            prepared_owned_bytes: 0,
            stats: ViewportLayoutStats::default(),
        }
    }

    /// 現在のcache resident/live byteと上限を返す。
    #[must_use]
    pub fn cache_usage(&self) -> CacheUsage {
        self.cache.usage()
    }

    /// Residentなline-layout entry数を返す。
    #[must_use]
    pub fn cached_line_count(&self) -> usize {
        self.cache.len()
    }

    /// Decode、cache hit、layout、insert拒否の累積counterを返す。
    #[must_use]
    pub fn stats(&self) -> ViewportLayoutStats {
        let active = self
            .large_navigator
            .as_ref()
            .map_or_else(LargeLineStats::default, |slot| slot.navigator.stats());
        let mut stats = self.stats;
        stats.streaming_raw_bytes_read = self
            .completed_streaming_stats
            .raw_bytes_read
            .saturating_add(active.raw_bytes_read);
        stats.streaming_peak_buffered_bytes = self
            .completed_streaming_stats
            .peak_buffered_bytes
            .max(active.peak_buffered_bytes);
        stats
    }

    /// Resident wrap layoutを破棄する。既存[`ViewportRows`]がpinするentryはframe dropまでliveに残る。
    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    /// State transaction用のprepared-layout領域を空にして開始する。
    pub(crate) fn begin_preparation(&mut self) {
        self.prepared_layouts.clear();
        self.prepared_owned_bytes = 0;
        self.preparing = true;
    }

    /// Failed transactionが保持したprepared layoutを破棄する。
    pub(crate) fn cancel_preparation(&mut self) {
        self.prepared_layouts.clear();
        self.prepared_owned_bytes = 0;
        self.preparing = false;
    }

    /// Old frame pinの解放後、prepared layoutをcacheへ移しrow handleをcache pinへ差し替える。
    pub(crate) fn finish_preparation(
        &mut self,
        rows: &mut ViewportRows,
    ) -> Result<(), ViewportLayoutError> {
        let prepared = std::mem::take(&mut self.prepared_layouts);
        let mut promoted = HashMap::new();
        promoted.try_reserve(prepared.len()).map_err(|_| {
            ViewportLayoutError::CacheBudgetExhausted {
                requested: self.prepared_owned_bytes,
                live: self.cache.live_bytes(),
                limit: self.cache.limit_bytes(),
            }
        })?;
        for (key, prepared) in prepared {
            let lookup = key.clone();
            let handle = if let Some(handle) = self.cache.get(&key) {
                handle
            } else {
                match self
                    .cache
                    .insert(key, prepared.layout.as_ref().clone(), prepared.weight)
                {
                    Ok(_) => self
                        .cache
                        .get(&lookup)
                        .ok_or(ViewportLayoutError::CacheEntryUnavailable)?,
                    Err(error) => {
                        let requested = error.weight_bytes();
                        let live = error.live_bytes();
                        let limit = error.limit_bytes();
                        drop(error.into_entry());
                        self.preparing = false;
                        self.prepared_owned_bytes = 0;
                        return Err(ViewportLayoutError::CacheBudgetExhausted {
                            requested,
                            live,
                            limit,
                        });
                    }
                }
            };
            promoted.insert(lookup, handle);
        }
        for row in &mut rows.rows {
            row.promote_prepared(&promoted)?;
        }
        if rows.streaming_bytes() > 0 {
            self.cache.clear();
        } else {
            self.retire_large_navigator();
        }
        self.prepared_owned_bytes = 0;
        self.preparing = false;
        Ok(())
    }

    /// Snapshot先頭のvisual rowを返す。Empty documentでは`None`。
    pub fn first(
        &mut self,
        document: &DocumentSnapshot,
        config: ViewportLayoutConfig,
    ) -> Result<Option<VisualAnchor>, ViewportLayoutError> {
        validate_config(config)?;
        let Some(line_id) = document.line_id_at(0) else {
            return Ok(None);
        };
        self.first_on_line(document, line_id, config).map(Some)
    }

    /// Snapshot末尾のvisual rowを返す。巨大行では全文row vectorを作らず末尾まで逐次scanする。
    pub fn last(
        &mut self,
        document: &DocumentSnapshot,
        config: ViewportLayoutConfig,
    ) -> Result<Option<VisualAnchor>, ViewportLayoutError> {
        validate_config(config)?;
        let Some(index) = document.line_count().checked_sub(1) else {
            return Ok(None);
        };
        let line_id = line_id_at(document, index)?;
        self.last_on_line(document, line_id, config).map(Some)
    }

    /// Logical byteを含むvisual rowへanchorを正規化する。
    ///
    /// Offsetがline末尾を越える場合は最後のlogical byte boundaryへclampする。返すoffsetはclamp後の
    /// logical byteを保つため、width変更後も同じ内容付近をtopへ戻せる。
    pub fn anchor_for_byte(
        &mut self,
        document: &DocumentSnapshot,
        line_id: LineId,
        byte_offset: usize,
        config: ViewportLayoutConfig,
    ) -> Result<VisualAnchor, ViewportLayoutError> {
        self.normalize(document, VisualAnchor::new(line_id, byte_offset), config)
    }

    /// Existing anchorを現在のsnapshotとreflow条件へ写す。
    pub fn normalize(
        &mut self,
        document: &DocumentSnapshot,
        anchor: VisualAnchor,
        config: ViewportLayoutConfig,
    ) -> Result<VisualAnchor, ViewportLayoutError> {
        validate_config(config)?;
        ensure_line(document, anchor.line_id)?;
        self.normalize_on_line(document, anchor, config)
    }

    /// Anchorから最大`count` visual rowsだけ後方へ進む。Document末尾では最後のrowへclampする。
    pub fn step_forward(
        &mut self,
        document: &DocumentSnapshot,
        anchor: VisualAnchor,
        count: usize,
        config: ViewportLayoutConfig,
    ) -> Result<VisualAnchor, ViewportLayoutError> {
        validate_config(config)?;
        let mut line_index = ensure_line(document, anchor.line_id)?;
        let mut current = self.normalize_on_line(document, anchor, config)?;
        let mut remaining = count;
        while remaining > 0 {
            let step = self.step_within_line_forward(document, current, remaining, config)?;
            current = step.anchor;
            remaining -= step.rows_moved;
            if remaining == 0 || line_index + 1 >= document.line_count() {
                return Ok(current);
            }

            // Current line末尾と次line先頭の間を1 visual rowとして消費する。
            remaining -= 1;
            line_index += 1;
            current = self.first_on_line(document, line_id_at(document, line_index)?, config)?;
        }
        Ok(current)
    }

    /// Anchorから最大`count` visual rowsだけ前方へ戻る。Document先頭では最初のrowへclampする。
    pub fn step_backward(
        &mut self,
        document: &DocumentSnapshot,
        anchor: VisualAnchor,
        count: usize,
        config: ViewportLayoutConfig,
    ) -> Result<VisualAnchor, ViewportLayoutError> {
        validate_config(config)?;
        let mut line_index = ensure_line(document, anchor.line_id)?;
        let mut current = self.normalize_on_line(document, anchor, config)?;
        let mut remaining = count;
        while remaining > 0 {
            let step = self.step_within_line_backward(document, current, remaining, config)?;
            current = step.anchor;
            remaining -= step.rows_moved;
            if remaining == 0 || line_index == 0 {
                return Ok(current);
            }

            // Current line先頭と前line末尾の間を1 visual rowとして消費する。
            remaining -= 1;
            line_index -= 1;
            current = self.last_on_line(document, line_id_at(document, line_index)?, config)?;
        }
        Ok(current)
    }

    /// Snapshot末尾を`body_height` rowsへbottom-alignしたtop anchorを返す。
    pub fn bottom_aligned_top(
        &mut self,
        document: &DocumentSnapshot,
        body_height: usize,
        config: ViewportLayoutConfig,
    ) -> Result<Option<VisualAnchor>, ViewportLayoutError> {
        let Some(last) = self.last(document, config)? else {
            return Ok(None);
        };
        self.step_backward(document, last, body_height.saturating_sub(1), config)
            .map(Some)
    }

    /// Top anchorから最大`row_limit`件だけvisual rowを収集する。
    ///
    /// 通常lineはcache layoutをpinし、巨大行はstateful navigatorの`Arc<VisualRow>` windowを共有する。
    /// どちらもframeへtext/span-mapをdeep-copyしない。
    pub fn collect(
        &mut self,
        document: &DocumentSnapshot,
        top: Option<VisualAnchor>,
        row_limit: usize,
        config: ViewportLayoutConfig,
    ) -> Result<ViewportRows, ViewportLayoutError> {
        validate_config(config)?;
        if document.is_empty() {
            return Ok(ViewportRows {
                top: None,
                rows: Vec::new(),
                at_end: true,
                streaming_bytes: 0,
                eager_bytes: 0,
                byte_limited: false,
            });
        }

        let initial = top.unwrap_or(VisualAnchor::new(line_id_at(document, 0)?, 0));
        let normalized = self.normalize_on_line(document, initial, config)?;
        let mut current = normalized;
        let mut line_index = ensure_line(document, normalized.line_id)?;
        let collection_limit = row_limit.min(MAX_VIEWPORT_ROW_HANDLES_PER_FRAME);
        let mut output = Vec::new();
        output.try_reserve_exact(collection_limit).map_err(|_| {
            LayoutError::StreamingRowAllocation {
                requested: collection_limit.saturating_mul(mem::size_of::<ViewportRow>()),
            }
        })?;
        let mut at_end = false;
        let mut streaming_bytes = 0_usize;
        let mut eager_bytes = 0_usize;
        let mut byte_limited = false;

        while output.len() < collection_limit {
            let descriptor = self.describe_line(document, current.line_id, config)?;
            let available = collection_limit - output.len();
            if descriptor.streaming {
                // Eager cache pinとstreamed Arc windowを同じframeへ混ぜるとcurrent/preparedの同居量を
                // 1つの台帳で追えない。Streamed lineへ達した時点でeager frameを確定し、次frameの
                // navigationでstreaming-only frameへ切り替える。
                if !output.is_empty() || eager_bytes > 0 {
                    byte_limited = true;
                    break;
                }
                let remaining_stream_bytes =
                    VIEWPORT_STREAM_FRAME_BYTES.saturating_sub(streaming_bytes);
                if remaining_stream_bytes < MAX_STREAMING_VISUAL_ROW_BYTES {
                    byte_limited = true;
                    break;
                }
                let navigator = self.ensure_large_navigator(document, &descriptor, current)?;
                let window = navigator.collect_window_bounded(available, remaining_stream_bytes)?;
                streaming_bytes = streaming_bytes.saturating_add(window.retained_bytes);
                byte_limited |= window.byte_limited;
                output.extend(window.rows.into_iter().map(ViewportRow::streamed));
                if window.reached_line_end && line_index + 1 >= document.line_count() {
                    at_end = true;
                }
                break;
            } else {
                let remaining = DEFAULT_VIEWPORT_STREAM_BYTES
                    .saturating_sub(eager_bytes)
                    .saturating_sub(streaming_bytes);
                let Some(layout) =
                    self.load_eager_layout_for_frame(document, descriptor, remaining)?
                else {
                    byte_limited = true;
                    break;
                };
                eager_bytes = eager_bytes.saturating_add(layout.weight_bytes());
                let rows = ensure_non_empty(current.line_id, layout.rows())?;
                let (_, row_index) = normalize_anchor_for_rows(rows, current);
                let end = row_index.saturating_add(available).min(rows.len());
                output.extend((row_index..end).map(|index| ViewportRow::new(&layout, index)));
                if end < rows.len() {
                    break;
                }
            }

            if line_index + 1 >= document.line_count() {
                at_end = true;
                break;
            }
            if output.len() == collection_limit {
                break;
            }
            let next_index = line_index + 1;
            let next_line_id = line_id_at(document, next_index)?;
            if !output.is_empty()
                && self
                    .describe_line(document, next_line_id, config)?
                    .streaming
            {
                byte_limited = true;
                break;
            }
            line_index = next_index;
            // 次line先頭はbyte 0 / fragment 0で一意に表せる。ここでlayoutを先読みすると、
            // 次loopがframe残量を照合する前にold-frame pinとcandidateを同居させてしまう。
            current = VisualAnchor::new(next_line_id, 0);
        }

        if collection_limit < row_limit && output.len() == collection_limit && !at_end {
            byte_limited = true;
        }
        debug_assert!(
            output
                .capacity()
                .saturating_mul(mem::size_of::<ViewportRow>())
                <= VIEWPORT_ROW_HANDLE_FRAME_BYTES
        );

        Ok(ViewportRows {
            top: Some(normalized),
            rows: output,
            at_end,
            streaming_bytes,
            eager_bytes,
            byte_limited,
        })
    }

    fn first_on_line(
        &mut self,
        document: &DocumentSnapshot,
        line_id: LineId,
        config: ViewportLayoutConfig,
    ) -> Result<VisualAnchor, ViewportLayoutError> {
        self.normalize_on_line(document, VisualAnchor::new(line_id, 0), config)
    }

    fn last_on_line(
        &mut self,
        document: &DocumentSnapshot,
        line_id: LineId,
        config: ViewportLayoutConfig,
    ) -> Result<VisualAnchor, ViewportLayoutError> {
        let descriptor = self.describe_line(document, line_id, config)?;
        if descriptor.streaming {
            let requested = VisualAnchor::new(line_id, usize::MAX);
            let navigator = self.ensure_large_navigator(document, &descriptor, requested)?;
            return navigator
                .last()
                .map(|anchor| visual_anchor(line_id, anchor))
                .map_err(Into::into);
        }
        let layout = self.load_eager_layout(document, descriptor)?;
        let rows = ensure_non_empty(line_id, layout.rows())?;
        Ok(anchor_for_row_at(line_id, rows, rows.len() - 1))
    }

    fn normalize_on_line(
        &mut self,
        document: &DocumentSnapshot,
        anchor: VisualAnchor,
        config: ViewportLayoutConfig,
    ) -> Result<VisualAnchor, ViewportLayoutError> {
        let descriptor = self.describe_line(document, anchor.line_id, config)?;
        if descriptor.streaming {
            let navigator = self.ensure_large_navigator(document, &descriptor, anchor)?;
            return Ok(visual_anchor(anchor.line_id, navigator.current_anchor()));
        }
        let layout = self.load_eager_layout(document, descriptor)?;
        let rows = ensure_non_empty(anchor.line_id, layout.rows())?;
        Ok(normalize_anchor_for_rows(rows, anchor).0)
    }

    fn step_within_line_forward(
        &mut self,
        document: &DocumentSnapshot,
        anchor: VisualAnchor,
        count: usize,
        config: ViewportLayoutConfig,
    ) -> Result<WithinLineStep, ViewportLayoutError> {
        let descriptor = self.describe_line(document, anchor.line_id, config)?;
        if descriptor.streaming {
            let navigator = self.ensure_large_navigator(document, &descriptor, anchor)?;
            let step = navigator.step_forward(count)?;
            return Ok(WithinLineStep {
                anchor: visual_anchor(anchor.line_id, step.anchor),
                rows_moved: step.rows_moved,
            });
        }
        let layout = self.load_eager_layout(document, descriptor)?;
        let rows = ensure_non_empty(anchor.line_id, layout.rows())?;
        let (_, row_index) = normalize_anchor_for_rows(rows, anchor);
        let rows_moved = count.min(rows.len() - row_index - 1);
        Ok(WithinLineStep {
            anchor: anchor_for_row_at(anchor.line_id, rows, row_index + rows_moved),
            rows_moved,
        })
    }

    fn step_within_line_backward(
        &mut self,
        document: &DocumentSnapshot,
        anchor: VisualAnchor,
        count: usize,
        config: ViewportLayoutConfig,
    ) -> Result<WithinLineStep, ViewportLayoutError> {
        let descriptor = self.describe_line(document, anchor.line_id, config)?;
        if descriptor.streaming {
            let navigator = self.ensure_large_navigator(document, &descriptor, anchor)?;
            let step = navigator.step_backward(count)?;
            return Ok(WithinLineStep {
                anchor: visual_anchor(anchor.line_id, step.anchor),
                rows_moved: step.rows_moved,
            });
        }
        let layout = self.load_eager_layout(document, descriptor)?;
        let rows = ensure_non_empty(anchor.line_id, layout.rows())?;
        let (_, row_index) = normalize_anchor_for_rows(rows, anchor);
        let rows_moved = count.min(row_index);
        Ok(WithinLineStep {
            anchor: anchor_for_row_at(anchor.line_id, rows, row_index - rows_moved),
            rows_moved,
        })
    }

    fn describe_line(
        &mut self,
        document: &DocumentSnapshot,
        line_id: LineId,
        config: ViewportLayoutConfig,
    ) -> Result<LineDescriptor, ViewportLayoutError> {
        self.synchronize_identity(document);
        self.stats.metadata_reads = self.stats.metadata_reads.saturating_add(1);
        let metadata = self
            .metadata_reader
            .read(document, line_id)?
            .ok_or(ViewportLayoutError::MissingLine { line_id })?;
        let gutter = gutter_for(metadata);
        let streaming = document_line_requires_streaming(metadata, config)?;
        let key = LineLayoutKey::from_metadata(metadata, gutter, config);
        let candidate_weight_upper_bound = if streaming {
            None
        } else {
            eager_layout_allocation_upper_bound(metadata, config.width, config.tab_width, gutter)?
                .map(|bytes| bytes.saturating_add(mem::size_of::<LineLayoutKey>()))
        };
        Ok(LineDescriptor {
            key,
            gutter,
            streaming,
            candidate_weight_upper_bound,
        })
    }

    fn load_eager_layout(
        &mut self,
        document: &DocumentSnapshot,
        descriptor: LineDescriptor,
    ) -> Result<LoadedLineLayout, ViewportLayoutError> {
        let line_id = descriptor.key.line_id;
        self.load_eager_layout_with_frame_limit(document, descriptor, None)?
            .ok_or(ViewportLayoutError::MissingVisualRows { line_id })
    }

    fn load_eager_layout_for_frame(
        &mut self,
        document: &DocumentSnapshot,
        descriptor: LineDescriptor,
        remaining_frame_bytes: usize,
    ) -> Result<Option<LoadedLineLayout>, ViewportLayoutError> {
        self.load_eager_layout_with_frame_limit(document, descriptor, Some(remaining_frame_bytes))
    }

    fn load_eager_layout_with_frame_limit(
        &mut self,
        document: &DocumentSnapshot,
        descriptor: LineDescriptor,
        remaining_frame_bytes: Option<usize>,
    ) -> Result<Option<LoadedLineLayout>, ViewportLayoutError> {
        // Streaming lineからeager lineへ移った時点でchunk decoderをdropし、この後に確保する
        // 3 MiB eager readerとのbuffer共存を防ぐ。Statsはretire時に累積へ移す。
        self.retire_large_navigator();
        if let Some(layout) = self.cache.get(&descriptor.key) {
            self.stats.cache_hits = self.stats.cache_hits.saturating_add(1);
            if remaining_frame_bytes.is_some_and(|remaining| layout.weight_bytes() > remaining) {
                return Ok(None);
            }
            return Ok(Some(LoadedLineLayout::Cached(layout)));
        }
        if let Some(prepared) = self.prepared_layouts.get(&descriptor.key) {
            if remaining_frame_bytes.is_some_and(|remaining| prepared.weight > remaining) {
                return Ok(None);
            }
            return Ok(Some(LoadedLineLayout::Prepared {
                key: descriptor.key,
                layout: Arc::clone(&prepared.layout),
                weight: prepared.weight,
            }));
        }

        let metadata_weight_upper_bound = descriptor.candidate_weight_upper_bound.ok_or(
            ViewportLayoutError::MissingVisualRows {
                line_id: descriptor.key.line_id,
            },
        )?;
        self.stats.decoded_lines = self.stats.decoded_lines.saturating_add(1);
        let line_id = descriptor.key.line_id;
        let line = self
            .reader
            .read(document, line_id)?
            .ok_or(ViewportLayoutError::MissingLine { line_id })?;
        let candidate_weight_upper_bound = eager_layout_decoded_allocation_upper_bound(
            &line,
            descriptor.key.width,
            descriptor.key.tab_width,
            descriptor.gutter,
        )?
        .saturating_add(mem::size_of::<LineLayoutKey>());
        if candidate_weight_upper_bound > metadata_weight_upper_bound {
            return Err(ViewportLayoutError::CacheBudgetExhausted {
                requested: candidate_weight_upper_bound,
                live: self
                    .cache
                    .live_bytes()
                    .saturating_add(self.prepared_owned_bytes),
                limit: metadata_weight_upper_bound,
            });
        }
        let admission = admit_eager_candidate_parts(
            &mut self.cache,
            self.preparing,
            self.prepared_owned_bytes,
            candidate_weight_upper_bound,
        );
        if let Err(error) = admission {
            if remaining_frame_bytes
                .is_some_and(|remaining| candidate_weight_upper_bound > remaining)
            {
                // Candidate上限は実weightより大きい。Frame残量を越える候補だけは、old-frame pinと
                // 同居できない時点で現在の有効frameを返す。残量内の候補ならtransaction台帳の
                // 破損を隠さないようtyped errorを保つ。
                return Ok(None);
            }
            return Err(error);
        }
        self.stats.layout_misses = self.stats.layout_misses.saturating_add(1);
        let layout = layout_line(
            &line,
            descriptor.key.width,
            descriptor.key.tab_width,
            descriptor.gutter,
        )?;
        let weight = estimate_cache_weight(&layout);
        if weight > candidate_weight_upper_bound {
            return Err(ViewportLayoutError::CacheBudgetExhausted {
                requested: weight,
                live: self
                    .cache
                    .live_bytes()
                    .saturating_add(self.prepared_owned_bytes),
                limit: candidate_weight_upper_bound,
            });
        }
        if remaining_frame_bytes.is_some_and(|remaining| weight > remaining) {
            return Ok(None);
        }
        let lookup = descriptor.key.clone();
        match self.cache.insert(descriptor.key, layout, weight) {
            Ok(_) => self
                .cache
                .get(&lookup)
                .map(LoadedLineLayout::Cached)
                .map(Some)
                .ok_or(ViewportLayoutError::CacheEntryUnavailable),
            Err(error) => {
                self.stats.cache_insert_rejections =
                    self.stats.cache_insert_rejections.saturating_add(1);
                let requested = error.weight_bytes();
                let live = error.live_bytes();
                let limit = error.limit_bytes();
                let (key, layout, weight) = error.into_entry();
                let next_prepared = self.prepared_owned_bytes.saturating_add(weight);
                if !self.preparing || next_prepared > DEFAULT_VIEWPORT_STREAM_BYTES {
                    drop(layout);
                    return Err(ViewportLayoutError::CacheBudgetExhausted {
                        requested,
                        live,
                        limit,
                    });
                }
                let layout = Arc::new(layout);
                self.prepared_layouts.insert(
                    key.clone(),
                    PreparedLineLayout {
                        layout: Arc::clone(&layout),
                        weight,
                    },
                );
                self.prepared_owned_bytes = next_prepared;
                Ok(Some(LoadedLineLayout::Prepared {
                    key,
                    layout,
                    weight,
                }))
            }
        }
    }

    #[cfg(test)]
    fn admit_eager_candidate(&mut self, candidate: usize) -> Result<(), ViewportLayoutError> {
        admit_eager_candidate_parts(
            &mut self.cache,
            self.preparing,
            self.prepared_owned_bytes,
            candidate,
        )
    }

    fn ensure_large_navigator(
        &mut self,
        document: &DocumentSnapshot,
        descriptor: &LineDescriptor,
        anchor: VisualAnchor,
    ) -> Result<&mut LargeLineNavigator, ViewportLayoutError> {
        if self
            .large_navigator
            .as_ref()
            .is_none_or(|slot| slot.key != descriptor.key)
        {
            self.retire_large_navigator();
            // Streamed frameは8 MiB visible shareを所有する。Resident eager entriesを同時に残さず、
            // 既存frameがpinする値だけをstate commitまでlive accountingへ残す。
            self.cache.clear();
            // Navigation中に通過したeager lineのprepared layoutはfinal streaming frameから参照されない。
            // Dropしてstreaming windowの8 MiB shareと二重に保持しない。
            self.prepared_layouts.clear();
            self.prepared_owned_bytes = 0;
            // Eager decoderはraw/stripped/textの3 MiB high-waterを再利用のため保持する。
            // Streaming decoderと同時に残すとtransient shareを二重に使うため、空bufferへ戻す。
            self.reader = DocumentLineReader::new();
            let navigator = LargeLineNavigator::new(
                document,
                descriptor.key.line_id,
                descriptor.key.width,
                descriptor.key.tab_width,
                descriptor.gutter,
                large_anchor(anchor),
            )?;
            self.stats.streaming_navigators_created =
                self.stats.streaming_navigators_created.saturating_add(1);
            self.large_navigator = Some(LargeNavigatorSlot {
                key: descriptor.key.clone(),
                navigator,
            });
        }
        let navigator = &mut self
            .large_navigator
            .as_mut()
            .ok_or(ViewportLayoutError::CacheEntryUnavailable)?
            .navigator;
        let _ = navigator.normalize(large_anchor(anchor))?;
        Ok(navigator)
    }

    fn synchronize_identity(&mut self, document: &DocumentSnapshot) {
        let identity = document.identity();
        if self.identity.as_ref() == Some(&identity) {
            return;
        }
        self.retire_large_navigator();
        self.cache.clear();
        self.identity = Some(identity);
    }

    fn retire_large_navigator(&mut self) {
        let Some(slot) = self.large_navigator.take() else {
            return;
        };
        let stats = slot.navigator.stats();
        self.completed_streaming_stats.raw_bytes_read = self
            .completed_streaming_stats
            .raw_bytes_read
            .saturating_add(stats.raw_bytes_read);
        self.completed_streaming_stats.peak_buffered_bytes = self
            .completed_streaming_stats
            .peak_buffered_bytes
            .max(stats.peak_buffered_bytes);
    }
}

/// Decode済みcandidateのlayout allocation前に、cacheまたはprepared領域の格納先を確保する。
///
/// Reader bufferは独立した3 MiB transient share内にあり、この関数は12 MiB cache、8 MiB
/// prepared share、candidateの合計だけを扱う。Reclaimable cache entryを外した後もold frameが
/// pinするentryは`live_bytes`へ残るため、成功時のcandidate allocationを含む合計は20 MiB以下になる。
fn admit_eager_candidate_parts(
    cache: &mut WeightedLruCache<LineLayoutKey, LineLayout>,
    preparing: bool,
    prepared_owned_bytes: usize,
    candidate: usize,
) -> Result<(), ViewportLayoutError> {
    let mut cache_live = cache.live_bytes();
    let global_fits = cache_live
        .saturating_add(prepared_owned_bytes)
        .saturating_add(candidate)
        <= VIEWPORT_EAGER_OWNED_BYTES;
    let cache_fits = cache_live.saturating_add(candidate) <= cache.limit_bytes();
    let prepared_fits = preparing
        && prepared_owned_bytes.saturating_add(candidate) <= DEFAULT_VIEWPORT_STREAM_BYTES;

    if !global_fits || (!cache_fits && !prepared_fits) {
        // Residentのpinされていないentryだけをdropし、old frameが所有するentryは
        // live accountingへ残す。この後の値がcandidate生成中の最小cache footprintになる。
        cache.clear();
        cache_live = cache.live_bytes();
    }

    let global_fits = cache_live
        .saturating_add(prepared_owned_bytes)
        .saturating_add(candidate)
        <= VIEWPORT_EAGER_OWNED_BYTES;
    let cache_fits = cache_live.saturating_add(candidate) <= cache.limit_bytes();
    let prepared_fits = preparing
        && prepared_owned_bytes.saturating_add(candidate) <= DEFAULT_VIEWPORT_STREAM_BYTES;
    if global_fits && (cache_fits || prepared_fits) {
        return Ok(());
    }

    let limit = if !global_fits {
        VIEWPORT_EAGER_OWNED_BYTES
    } else if preparing {
        cache.limit_bytes().max(DEFAULT_VIEWPORT_STREAM_BYTES)
    } else {
        cache.limit_bytes()
    };
    Err(ViewportLayoutError::CacheBudgetExhausted {
        requested: candidate,
        live: cache_live.saturating_add(prepared_owned_bytes),
        limit,
    })
}

#[derive(Clone, Debug)]
struct LineDescriptor {
    key: LineLayoutKey,
    gutter: GutterSpec,
    streaming: bool,
    candidate_weight_upper_bound: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
struct WithinLineStep {
    anchor: VisualAnchor,
    rows_moved: usize,
}

#[derive(Debug)]
struct LargeNavigatorSlot {
    key: LineLayoutKey,
    navigator: LargeLineNavigator,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LineLayoutKey {
    line_id: LineId,
    kind: LineKind,
    commit_id: Option<CommitId>,
    file_id: Option<FileId>,
    hunk_id: Option<HunkId>,
    old_line_number: Option<u64>,
    new_line_number: Option<u64>,
    gutter_old_width: usize,
    gutter_new_width: usize,
    width: usize,
    tab_width: usize,
}

impl LineLayoutKey {
    fn from_metadata(
        line: DocumentLineMetadata,
        gutter: GutterSpec,
        config: ViewportLayoutConfig,
    ) -> Self {
        Self {
            line_id: line.id,
            kind: line.kind,
            commit_id: line.commit_id,
            file_id: line.file_id,
            hunk_id: line.hunk_id,
            old_line_number: line.old_line_number,
            new_line_number: line.new_line_number,
            gutter_old_width: gutter.old_width(),
            gutter_new_width: gutter.new_width(),
            width: config.width,
            tab_width: config.tab_width,
        }
    }
}

impl Hash for LineLayoutKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.line_id.hash(state);
        line_kind_tag(self.kind).hash(state);
        self.commit_id.hash(state);
        self.file_id.hash(state);
        self.hunk_id.hash(state);
        self.old_line_number.hash(state);
        self.new_line_number.hash(state);
        self.gutter_old_width.hash(state);
        self.gutter_new_width.hash(state);
        self.width.hash(state);
        self.tab_width.hash(state);
    }
}

fn large_anchor(anchor: VisualAnchor) -> LargeRowAnchor {
    LargeRowAnchor {
        byte_offset: anchor.byte_offset,
        fragment: anchor.fragment_ordinal,
    }
}

fn visual_anchor(line_id: LineId, anchor: LargeRowAnchor) -> VisualAnchor {
    VisualAnchor::new(line_id, anchor.byte_offset).with_fragment_ordinal(anchor.fragment)
}

#[derive(Clone, Debug)]
struct PreparedLineLayout {
    layout: Arc<LineLayout>,
    weight: usize,
}

enum LoadedLineLayout {
    Cached(CacheHandle<LineLayout>),
    Prepared {
        key: LineLayoutKey,
        layout: Arc<LineLayout>,
        weight: usize,
    },
}

impl LoadedLineLayout {
    fn rows(&self) -> &[VisualRow] {
        match self {
            Self::Cached(layout) => layout.rows(),
            Self::Prepared { layout, .. } => layout.rows(),
        }
    }

    fn weight_bytes(&self) -> usize {
        match self {
            Self::Cached(layout) => layout.weight_bytes(),
            Self::Prepared { weight, .. } => *weight,
        }
    }

    fn shared(&self) -> SharedLineLayout {
        match self {
            Self::Cached(layout) => SharedLineLayout::Cached(layout.clone()),
            Self::Prepared { key, layout, .. } => SharedLineLayout::Prepared {
                key: key.clone(),
                layout: Arc::clone(layout),
            },
        }
    }
}

fn validate_config(config: ViewportLayoutConfig) -> Result<(), LayoutError> {
    if config.tab_width == 0 {
        return Err(LayoutError::InvalidTabWidth {
            actual: config.tab_width,
        });
    }
    if config.width == 0 {
        return Err(LayoutError::TooNarrow {
            actual: config.width,
            required: 1,
        });
    }
    Ok(())
}

fn line_id_at(document: &DocumentSnapshot, index: usize) -> Result<LineId, ViewportLayoutError> {
    document
        .line_id_at(index)
        .ok_or(ViewportLayoutError::UnavailableLineIndex { index })
}

fn ensure_line(document: &DocumentSnapshot, line_id: LineId) -> Result<usize, ViewportLayoutError> {
    let index =
        usize::try_from(line_id.get()).map_err(|_| ViewportLayoutError::MissingLine { line_id })?;
    if index >= document.line_count() {
        return Err(ViewportLayoutError::MissingLine { line_id });
    }
    Ok(index)
}

fn ensure_non_empty(
    line_id: LineId,
    rows: &[VisualRow],
) -> Result<&[VisualRow], ViewportLayoutError> {
    if rows.is_empty() {
        Err(ViewportLayoutError::MissingVisualRows { line_id })
    } else {
        Ok(rows)
    }
}

fn normalize_anchor_for_rows(rows: &[VisualRow], anchor: VisualAnchor) -> (VisualAnchor, usize) {
    let maximum = rows
        .iter()
        .map(|row| row.content_range.end)
        .max()
        .unwrap_or(0);
    let byte_offset = anchor.byte_offset.min(maximum);
    let matching_count = rows
        .iter()
        .filter(|row| row_contains_byte(row, byte_offset))
        .count();

    if matching_count > 0 {
        let fragment_ordinal = anchor.fragment_ordinal.min(matching_count - 1);
        let row_index = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row_contains_byte(row, byte_offset))
            .nth(fragment_ordinal)
            .map_or(0, |(index, _)| index);
        return (
            VisualAnchor::new(anchor.line_id, byte_offset).with_fragment_ordinal(fragment_ordinal),
            row_index,
        );
    }

    let row_index = rows
        .last()
        .filter(|row| row.content_range.end == byte_offset)
        .map(|_| rows.len() - 1)
        .unwrap_or_else(|| {
            rows.iter()
                .position(|row| row.content_range.end >= byte_offset)
                .unwrap_or(rows.len() - 1)
        });
    (VisualAnchor::new(anchor.line_id, byte_offset), row_index)
}

fn row_contains_byte(row: &VisualRow, byte_offset: usize) -> bool {
    row.content_range.contains(&byte_offset)
        || (row.content_range.is_empty() && byte_offset == row.content_range.start)
}

fn anchor_for_row_at(line_id: LineId, rows: &[VisualRow], row_index: usize) -> VisualAnchor {
    let byte_offset = rows[row_index].content_range.start;
    let fragment_ordinal = rows[..row_index]
        .iter()
        .filter(|row| row_contains_byte(row, byte_offset))
        .count();
    VisualAnchor::new(line_id, byte_offset).with_fragment_ordinal(fragment_ordinal)
}

fn gutter_for(metadata: DocumentLineMetadata) -> GutterSpec {
    metadata
        .file_line_bounds
        .map_or_else(GutterSpec::default, |bounds| {
            GutterSpec::new(
                bounds.max_old_line_number.map_or(0, decimal_width),
                bounds.max_new_line_number.map_or(0, decimal_width),
            )
        })
}

/// Fixed line metadataとviewport geometryだけでstreaming経路の要否を返す。
///
/// UI frame preloadも同じ判定を使い、streamed lineを`OwnedDocumentLine`へ戻さない。
pub(crate) fn document_line_requires_streaming(
    metadata: DocumentLineMetadata,
    config: ViewportLayoutConfig,
) -> Result<bool, LayoutError> {
    line_requires_streaming_layout(
        metadata,
        config.width,
        config.tab_width,
        gutter_for(metadata),
    )
}

fn decimal_width(number: u64) -> u8 {
    number.checked_ilog10().map_or(1, |log| log as u8 + 1)
}

fn line_kind_tag(kind: LineKind) -> u8 {
    match kind {
        LineKind::Plain => 0,
        LineKind::CommitHeader => 1,
        LineKind::CommitMetadata => 2,
        LineKind::CommitMessage => 3,
        LineKind::DiffHeader => 4,
        LineKind::FileMetadata => 5,
        LineKind::HunkHeader => 6,
        LineKind::Context => 7,
        LineKind::Deletion => 8,
        LineKind::Addition => 9,
        LineKind::NoNewlineMarker => 10,
        LineKind::Binary => 11,
        LineKind::Submodule => 12,
        LineKind::Fallback => 13,
    }
}

fn estimate_cache_weight(layout: &LineLayout) -> usize {
    mem::size_of::<LineLayoutKey>().saturating_add(layout.retained_bytes())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::{
        DEFAULT_VIEWPORT_CACHE_BYTES, ViewportLayout, ViewportLayoutConfig, ViewportLayoutError,
        VisualAnchor,
    };
    use crate::direct::LayoutError;
    use crate::document::{DocumentLineReader, DocumentSnapshot};
    use crate::input::AppendStore;
    use crate::parser::{GitParser, parse_bytes};
    use crate::storage::StructuralStorage;

    const WIDE: ViewportLayoutConfig = ViewportLayoutConfig::new(80, 4);

    fn line_texts(frame: &super::ViewportRows) -> Vec<&str> {
        frame.rows.iter().map(|row| row.text.as_str()).collect()
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

    fn find_line(document: &DocumentSnapshot, text: &str) -> crate::model::LineId {
        let mut reader = DocumentLineReader::new();
        (0..document.line_count())
            .find_map(|index| {
                let id = document.line_id_at(index).unwrap();
                let line = reader.read(document, id).unwrap().unwrap();
                (line.text == text).then_some(id)
            })
            .unwrap()
    }

    #[test]
    fn large_document_decodes_and_layouts_only_touched_lines() {
        let mut input = String::new();
        for index in 0..20_000 {
            input.push_str(&format!("line-{index:05}\n"));
        }
        let document = parse_bytes(input.as_bytes()).unwrap();
        let mut viewport = ViewportLayout::new();

        let first = viewport.collect(&document, None, 6, WIDE).unwrap();
        assert_eq!(line_texts(&first).len(), 6);
        assert_eq!(viewport.stats().decoded_lines, 6);
        assert_eq!(viewport.stats().layout_misses, 6);

        let moved = viewport
            .step_forward(&document, first.top.unwrap(), 4, WIDE)
            .unwrap();
        let second = viewport.collect(&document, Some(moved), 6, WIDE).unwrap();
        assert_eq!(line_texts(&second).first(), Some(&"line-00004"));
        assert_eq!(line_texts(&second).last(), Some(&"line-00009"));
        assert_eq!(viewport.stats().layout_misses, 10);
        assert!(viewport.stats().decoded_lines < 32);
    }

    #[test]
    fn bottom_alignment_reads_backward_until_body_is_full() {
        let document = parse_bytes(b"zero\none\ntwo\nthree\nfour\nfive\nsix\nseven\n").unwrap();
        let mut viewport = ViewportLayout::new();

        let top = viewport
            .bottom_aligned_top(&document, 3, WIDE)
            .unwrap()
            .unwrap();
        let frame = viewport.collect(&document, Some(top), 3, WIDE).unwrap();

        assert_eq!(line_texts(&frame), vec!["five", "six", "seven"]);
        assert!(frame.at_end);
    }

    #[test]
    fn logical_byte_anchor_survives_resize_and_clamps_at_line_end() {
        let document = parse_bytes(b"abcdefghijklmnopqrstuvwxyz\n").unwrap();
        let line_id = document.line_id_at(0).unwrap();
        let mut viewport = ViewportLayout::new();
        let narrow = ViewportLayoutConfig::new(7, 4);
        let wide = ViewportLayoutConfig::new(10, 4);

        let anchor = viewport
            .anchor_for_byte(&document, line_id, 12, narrow)
            .unwrap();
        assert_eq!(anchor.byte_offset, 12);
        let narrow_frame = viewport
            .collect(&document, Some(anchor), 1, narrow)
            .unwrap();
        let wide_frame = viewport.collect(&document, Some(anchor), 1, wide).unwrap();
        assert_eq!(line_texts(&narrow_frame), vec!["hijklmn"]);
        assert_eq!(line_texts(&wide_frame), vec!["klmnopqrst"]);

        let clamped = viewport
            .normalize(&document, VisualAnchor::new(line_id, usize::MAX), wide)
            .unwrap();
        assert_eq!(clamped.byte_offset, 26);
    }

    #[test]
    fn unchanged_lines_reuse_layout_across_append_only_snapshots() {
        let (raw, mut parser) = streaming_parser();
        push(&raw, &mut parser, b"alpha\n");
        let before = parser.snapshot().unwrap();
        let mut viewport = ViewportLayout::new();
        let _ = viewport.collect(&before, None, 1, WIDE).unwrap();
        assert_eq!(viewport.stats().layout_misses, 1);

        push(&raw, &mut parser, b"beta\n");
        let after = parser.snapshot().unwrap();
        let _ = viewport.collect(&after, None, 1, WIDE).unwrap();

        assert_eq!(viewport.stats().layout_misses, 1);
        assert!(viewport.stats().cache_hits >= 1);
    }

    #[test]
    fn independent_document_lineage_clears_same_id_cache_entry() {
        let first = parse_bytes(b"same\n").unwrap();
        let second = parse_bytes(b"same\n").unwrap();
        assert_ne!(first.identity(), second.identity());
        let mut viewport = ViewportLayout::new();

        let _ = viewport.collect(&first, None, 1, WIDE).unwrap();
        let _ = viewport.collect(&second, None, 1, WIDE).unwrap();

        assert_eq!(viewport.cached_line_count(), 1);
        assert_eq!(viewport.stats().layout_misses, 2);
        assert_eq!(viewport.stats().decoded_lines, 2);
        assert!(viewport.stats().cache_hits >= 2);
    }

    #[test]
    fn width_tab_kind_and_gutter_changes_invalidate_line_layout_key() {
        let (raw, mut parser) = streaming_parser();
        push(
            &raw,
            &mut parser,
            b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-old\n+new\n",
        );
        let active = parser.snapshot().unwrap();
        let deletion = find_line(&active, "-old");
        let mut viewport = ViewportLayout::new();
        let _ = viewport
            .collect(&active, Some(VisualAnchor::new(deletion, 0)), 1, WIDE)
            .unwrap();
        let _ = viewport
            .collect(
                &active,
                Some(VisualAnchor::new(deletion, 0)),
                1,
                ViewportLayoutConfig::new(79, 4),
            )
            .unwrap();
        let _ = viewport
            .collect(
                &active,
                Some(VisualAnchor::new(deletion, 0)),
                1,
                ViewportLayoutConfig::new(80, 8),
            )
            .unwrap();
        assert_eq!(viewport.stats().layout_misses, 3);

        let _ = raw.seal().unwrap();
        let malformed = parser.into_document().unwrap();
        let frame = viewport
            .collect(&malformed, Some(VisualAnchor::new(deletion, 0)), 1, WIDE)
            .unwrap();
        assert_eq!(viewport.stats().layout_misses, 4);
        assert_eq!(frame.rows[0].role, crate::theme::SemanticRole::Dim);

        let (raw, mut parser) = streaming_parser();
        push(
            &raw,
            &mut parser,
            b"diff --git a/y b/y\n--- a/y\n+++ b/y\n@@ -1 +1 @@\n-old\n+new\n",
        );
        let small_gutter = parser.snapshot().unwrap();
        let old = find_line(&small_gutter, "-old");
        let _ = viewport
            .collect(&small_gutter, Some(VisualAnchor::new(old, 0)), 1, WIDE)
            .unwrap();
        let misses_before_growth = viewport.stats().layout_misses;

        push(&raw, &mut parser, b"@@ -1000 +1000 @@\n-old2\n+new2\n");
        let large_gutter = parser.snapshot().unwrap();
        let _ = viewport
            .collect(&large_gutter, Some(VisualAnchor::new(old, 0)), 1, WIDE)
            .unwrap();
        assert_eq!(viewport.stats().layout_misses, misses_before_growth + 1);
    }

    #[test]
    fn cache_accounting_stays_within_explicit_share() {
        let mut input = String::new();
        for index in 0..80 {
            input.push_str(&format!("cache-line-{index:03}\n"));
        }
        let document = parse_bytes(input.as_bytes()).unwrap();
        let budget = 128 * 1024;
        let mut viewport = ViewportLayout::with_cache_budget(budget);

        let _ = viewport.collect(&document, None, 80, WIDE).unwrap();
        let usage = viewport.cache_usage();

        assert_eq!(usage.limit_bytes, budget);
        assert!(usage.resident_bytes <= budget);
        assert!(usage.live_bytes <= budget);
        assert!(viewport.cached_line_count() <= 80);
        assert!(viewport.cached_line_count() > 0);
        assert_eq!(DEFAULT_VIEWPORT_CACHE_BYTES, 12 * 1024 * 1024);
    }

    #[test]
    fn u16_max_row_request_keeps_one_frame_handle_capacity_bounded() {
        let mut input = String::new();
        for index in 0..6_000 {
            input.push_str(&format!("row-{index:04}\n"));
        }
        let document = parse_bytes(input.as_bytes()).unwrap();
        let mut viewport = ViewportLayout::new();

        let frame = viewport
            .collect(&document, None, usize::from(u16::MAX), WIDE)
            .unwrap();

        assert_eq!(frame.rows.len(), super::MAX_VIEWPORT_ROW_HANDLES_PER_FRAME);
        assert!(frame.row_handle_bytes() <= super::VIEWPORT_ROW_HANDLE_FRAME_BYTES);
        assert!(frame.byte_limited());
    }

    #[test]
    fn old_and_prepared_frames_share_one_mibibyte_of_row_handles() {
        let mut input = String::new();
        for index in 0..10_100 {
            input.push_str(&format!("row-{index:05}\n"));
        }
        let document = parse_bytes(input.as_bytes()).unwrap();
        let mut viewport = ViewportLayout::new();
        let old = viewport
            .collect(&document, None, usize::from(u16::MAX), WIDE)
            .unwrap();
        let next = VisualAnchor::new(
            document
                .line_id_at(super::MAX_VIEWPORT_ROW_HANDLES_PER_FRAME)
                .unwrap(),
            0,
        );

        viewport.begin_preparation();
        let prepared = viewport
            .collect(&document, Some(next), usize::from(u16::MAX), WIDE)
            .unwrap();

        assert!(old.row_handle_bytes() <= super::VIEWPORT_ROW_HANDLE_FRAME_BYTES);
        assert!(prepared.row_handle_bytes() <= super::VIEWPORT_ROW_HANDLE_FRAME_BYTES);
        assert!(
            old.row_handle_bytes()
                .saturating_add(prepared.row_handle_bytes())
                <= super::VIEWPORT_ROW_HANDLE_BYTES
        );
    }

    #[test]
    fn cache_prepared_candidate_and_reader_peak_stays_within_24_mibibytes() {
        let document = parse_bytes(b"candidate\n").unwrap();
        let mut viewport = ViewportLayout::new();
        let descriptor = viewport
            .describe_line(&document, document.line_id_at(0).unwrap(), WIDE)
            .unwrap();
        let mut reader = DocumentLineReader::new();
        let line = reader
            .read(&document, document.line_id_at(0).unwrap())
            .unwrap()
            .unwrap();
        let layout = crate::direct::layout_line(&line, 80, 4, descriptor.gutter).unwrap();
        let pinned_cache = 11 * 1024 * 1024;
        viewport
            .cache
            .insert(descriptor.key.clone(), layout, pinned_cache)
            .unwrap();
        let pin = viewport.cache.get(&descriptor.key).unwrap();
        viewport.cache.clear();
        viewport.preparing = true;
        viewport.prepared_owned_bytes = super::DEFAULT_VIEWPORT_STREAM_BYTES;
        let candidate = 1024 * 1024;

        viewport.admit_eager_candidate(candidate).unwrap();
        let eager_peak = viewport
            .cache
            .live_bytes()
            .saturating_add(viewport.prepared_owned_bytes)
            .saturating_add(candidate);
        assert_eq!(eager_peak, super::VIEWPORT_EAGER_OWNED_BYTES);
        assert_eq!(
            eager_peak.saturating_add(super::DEFAULT_VIEWPORT_TRANSIENT_BYTES),
            crate::derived_memory::VIEWPORT_MEMORY_BYTES
        );
        assert!(matches!(
            viewport.admit_eager_candidate(candidate + 1),
            Err(ViewportLayoutError::CacheBudgetExhausted {
                requested,
                live,
                limit: super::VIEWPORT_EAGER_OWNED_BYTES,
            }) if requested == candidate + 1
                && live == pinned_cache + super::DEFAULT_VIEWPORT_STREAM_BYTES
        ));
        drop(pin);
    }

    #[test]
    fn metadata_candidate_upper_bound_covers_actual_cache_weight() {
        let document = parse_bytes(
            "plain\t界\n\
             diff --git a/x.rs b/x.rs\n\
             --- a/x.rs\n\
             +++ b/x.rs\n\
             @@ -123,1 +4567,1 @@\n\
             -\told界\n\
             +\tnew界\n"
                .as_bytes(),
        )
        .unwrap();
        let config = ViewportLayoutConfig::new(24, 4);
        let mut metadata_reader = crate::document::DocumentLineMetadataReader::new();
        let mut line_reader = DocumentLineReader::new();

        for index in 0..document.line_count() {
            let line_id = document.line_id_at(index).unwrap();
            let metadata = metadata_reader.read(&document, line_id).unwrap().unwrap();
            let gutter = super::gutter_for(metadata);
            if crate::direct::line_requires_streaming_layout(
                metadata,
                config.width,
                config.tab_width,
                gutter,
            )
            .unwrap()
            {
                continue;
            }
            let admitted = crate::direct::eager_layout_allocation_upper_bound(
                metadata,
                config.width,
                config.tab_width,
                gutter,
            )
            .unwrap()
            .unwrap()
            .saturating_add(std::mem::size_of::<super::LineLayoutKey>());
            let line = line_reader.read(&document, line_id).unwrap().unwrap();
            let decoded = crate::direct::eager_layout_decoded_allocation_upper_bound(
                &line,
                config.width,
                config.tab_width,
                gutter,
            )
            .unwrap()
            .saturating_add(std::mem::size_of::<super::LineLayoutKey>());
            let layout =
                crate::direct::layout_line(&line, config.width, config.tab_width, gutter).unwrap();
            let actual = super::estimate_cache_weight(&layout);
            assert!(decoded <= admitted, "line {line_id} decoded bound widened");
            assert!(
                actual <= decoded,
                "line {line_id} retained {actual} bytes above decoded bound {decoded}"
            );
        }
    }

    #[test]
    fn collect_preserves_sticky_ids_and_reports_typed_failures() {
        let document =
            parse_bytes(b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n")
                .unwrap();
        let addition = find_line(&document, "+new");
        let mut viewport = ViewportLayout::new();
        let frame = viewport
            .collect(&document, Some(VisualAnchor::new(addition, 0)), 1, WIDE)
            .unwrap();
        assert!(frame.rows[0].commit_id.is_some());
        assert!(frame.rows[0].file_id.is_some());
        assert!(frame.rows[0].hunk_id.is_some());

        let too_narrow = viewport.collect(
            &document,
            Some(VisualAnchor::new(addition, 0)),
            1,
            ViewportLayoutConfig::new(10, 4),
        );
        assert!(matches!(
            too_narrow,
            Err(ViewportLayoutError::Layout(LayoutError::TooNarrow { .. }))
        ));

        let missing = viewport.normalize(
            &document,
            VisualAnchor::new(crate::model::LineId::new(999), 0),
            WIDE,
        );
        assert!(matches!(
            missing,
            Err(ViewportLayoutError::MissingLine { .. })
        ));
    }

    #[test]
    fn cache_hit_avoids_relayout_and_tiny_budget_rejects_transactionally() {
        let document = parse_bytes(b"same line\n").unwrap();
        let mut viewport = ViewportLayout::new();
        let _ = viewport.collect(&document, None, 1, WIDE).unwrap();
        let misses = viewport.stats().layout_misses;
        let _ = viewport.collect(&document, None, 1, WIDE).unwrap();
        assert_eq!(viewport.stats().layout_misses, misses);
        assert!(viewport.stats().cache_hits >= 1);

        let mut uncached = ViewportLayout::with_cache_budget(1);
        let decoded_before = uncached.stats().decoded_lines;
        let misses_before = uncached.stats().layout_misses;
        let frame = uncached.collect(&document, None, 1, WIDE);
        assert!(matches!(
            frame,
            Err(ViewportLayoutError::CacheBudgetExhausted { limit: 1, .. })
        ));
        assert_eq!(uncached.cached_line_count(), 0);
        assert_eq!(uncached.stats().cache_insert_rejections, 0);
        assert_eq!(uncached.stats().decoded_lines, decoded_before + 1);
        assert_eq!(uncached.stats().layout_misses, misses_before);
        assert_eq!(uncached.cache_usage().live_bytes, 0);
    }

    #[test]
    fn first_last_and_visual_row_steps_cross_wrapped_lines() {
        let document = parse_bytes(b"abcdefghij\nlast\n").unwrap();
        let config = ViewportLayoutConfig::new(4, 4);
        let mut viewport = ViewportLayout::new();
        let first = viewport.first(&document, config).unwrap().unwrap();
        let last = viewport.last(&document, config).unwrap().unwrap();

        let third = viewport.step_forward(&document, first, 2, config).unwrap();
        let third_frame = viewport.collect(&document, Some(third), 1, config).unwrap();
        assert_eq!(line_texts(&third_frame), vec!["ij"]);

        let previous = viewport.step_backward(&document, last, 1, config).unwrap();
        let previous_frame = viewport
            .collect(&document, Some(previous), 1, config)
            .unwrap();
        assert_eq!(line_texts(&previous_frame), vec!["ij"]);
    }

    #[test]
    fn tab_fragments_have_unique_forward_and_backward_anchors() {
        let document =
            parse_bytes(b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -0,0 +1,1 @@\n+\tZ\n").unwrap();
        let addition = find_line(&document, "+\tZ");
        // Source prefixが10cellなので、width 11では4cellのtabが4 visual rowsへ分かれる。
        let narrow = ViewportLayoutConfig::new(11, 4);
        let mut viewport = ViewportLayout::new();
        let mut anchors = vec![
            viewport
                .anchor_for_byte(&document, addition, 0, narrow)
                .unwrap(),
        ];

        for _ in 0..4 {
            let next = viewport
                .step_forward(&document, *anchors.last().unwrap(), 1, narrow)
                .unwrap();
            let frame = viewport.collect(&document, Some(next), 1, narrow).unwrap();
            assert_eq!(frame.top, Some(next));
            anchors.push(next);
        }

        assert_eq!(
            anchors
                .iter()
                .map(|anchor| (anchor.byte_offset, anchor.fragment_ordinal))
                .collect::<Vec<_>>(),
            vec![(0, 0), (1, 1), (1, 2), (1, 3), (2, 0)]
        );

        let mut current = *anchors.last().unwrap();
        for expected in anchors[..anchors.len() - 1].iter().rev() {
            current = viewport
                .step_backward(&document, current, 1, narrow)
                .unwrap();
            assert_eq!(&current, expected);
        }
    }

    #[test]
    fn resize_clamps_tab_fragment_ordinal_to_the_new_fragment_count() {
        let document =
            parse_bytes(b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -0,0 +1,1 @@\n+\tZ\n").unwrap();
        let addition = find_line(&document, "+\tZ");
        let mut viewport = ViewportLayout::new();
        let narrow = ViewportLayoutConfig::new(11, 4);
        let two_tab_rows = ViewportLayoutConfig::new(12, 4);
        let one_tab_row = ViewportLayoutConfig::new(14, 4);
        let third_fragment = VisualAnchor::new(addition, 1).with_fragment_ordinal(3);

        let same = viewport
            .normalize(&document, third_fragment, narrow)
            .unwrap();
        assert_eq!(same.fragment_ordinal, 3);
        let clamped_to_second = viewport
            .normalize(&document, third_fragment, two_tab_rows)
            .unwrap();
        assert_eq!(clamped_to_second.byte_offset, 1);
        assert_eq!(clamped_to_second.fragment_ordinal, 1);
        let clamped_to_first = viewport
            .normalize(&document, third_fragment, one_tab_row)
            .unwrap();
        assert_eq!(clamped_to_first.byte_offset, 1);
        assert_eq!(clamped_to_first.fragment_ordinal, 0);
    }

    #[test]
    fn streaming_window_does_not_decode_or_copy_full_line_text() {
        let mut input = "x".repeat(128 * 1024);
        input.push('\n');
        let document = parse_bytes(input.as_bytes()).unwrap();
        let mut viewport = ViewportLayout::new();

        let first = viewport.collect(&document, None, 1, WIDE).unwrap();
        let reads_after_first = viewport.stats().streaming_raw_bytes_read;
        let second = viewport.collect(&document, None, 1, WIDE).unwrap();
        assert_eq!(first.rows.len(), 1);
        assert_eq!(second.rows.len(), 1);
        assert_eq!(viewport.cached_line_count(), 0);
        assert_eq!(viewport.stats().decoded_lines, 0);
        assert_eq!(viewport.stats().layout_misses, 0);
        assert_eq!(viewport.stats().streaming_navigators_created, 1);
        assert_eq!(viewport.stats().streaming_raw_bytes_read, reads_after_first);
        assert!(second.streaming_bytes() <= super::DEFAULT_VIEWPORT_STREAM_BYTES);
    }

    #[test]
    fn width_one_routes_sub_mebibyte_line_through_streaming_rows() {
        let mut input = "x".repeat(6_000);
        input.push('\n');
        let document = parse_bytes(input.as_bytes()).unwrap();
        let config = ViewportLayoutConfig::new(1, 4);
        let mut viewport = ViewportLayout::new();

        let frame = viewport.collect(&document, None, 4, config).unwrap();

        assert_eq!(line_texts(&frame), vec!["x", "x", "x", "x"]);
        assert_eq!(viewport.stats().decoded_lines, 0);
        assert_eq!(viewport.stats().streaming_navigators_created, 1);
        assert!(frame.streaming_bytes() <= super::DEFAULT_VIEWPORT_STREAM_BYTES);
    }

    #[test]
    fn repeated_forward_steps_reuse_the_huge_line_decoder_prefix() {
        let mut input = "x".repeat(100_000);
        input.push('\n');
        let document = parse_bytes(input.as_bytes()).unwrap();
        let config = ViewportLayoutConfig::new(1, 4);
        let mut viewport = ViewportLayout::new();
        let mut anchor = viewport.first(&document, config).unwrap().unwrap();
        let _ = viewport
            .collect(&document, Some(anchor), 8, config)
            .unwrap();

        for _ in 0..512 {
            anchor = viewport.step_forward(&document, anchor, 1, config).unwrap();
            let frame = viewport
                .collect(&document, Some(anchor), 8, config)
                .unwrap();
            assert_eq!(frame.top, Some(anchor));
        }

        let stats = viewport.stats();
        assert_eq!(stats.streaming_navigators_created, 1);
        assert!(stats.streaming_raw_bytes_read <= input.len() as u64);
        assert_eq!(anchor.byte_offset, 512);
    }

    #[test]
    fn streamed_tab_fragments_step_and_clamp_across_resize() {
        let mut input = "\t".repeat(1_500);
        input.push('\n');
        let document = parse_bytes(input.as_bytes()).unwrap();
        let line_id = document.line_id_at(0).unwrap();
        let width_one = ViewportLayoutConfig::new(1, 4);
        let width_two = ViewportLayoutConfig::new(2, 4);
        let width_four = ViewportLayoutConfig::new(4, 4);
        let mut viewport = ViewportLayout::new();
        let mut anchors = vec![viewport.first(&document, width_one).unwrap().unwrap()];

        for _ in 0..4 {
            anchors.push(
                viewport
                    .step_forward(&document, *anchors.last().unwrap(), 1, width_one)
                    .unwrap(),
            );
        }
        assert_eq!(
            anchors
                .iter()
                .map(|anchor| (anchor.byte_offset, anchor.fragment_ordinal))
                .collect::<Vec<_>>(),
            vec![(0, 0), (0, 1), (0, 2), (0, 3), (1, 0)]
        );

        let fourth = VisualAnchor::new(line_id, 0).with_fragment_ordinal(3);
        let two = viewport.normalize(&document, fourth, width_two).unwrap();
        let one = viewport.normalize(&document, fourth, width_four).unwrap();
        assert_eq!((two.byte_offset, two.fragment_ordinal), (0, 1));
        assert_eq!((one.byte_offset, one.fragment_ordinal), (0, 0));
    }

    #[test]
    fn huge_line_first_last_page_and_bottom_alignment_keep_stable_anchors() {
        let mut input = "x".repeat(6_000);
        input.push('\n');
        let document = parse_bytes(input.as_bytes()).unwrap();
        let config = ViewportLayoutConfig::new(1, 4);
        let mut viewport = ViewportLayout::new();
        let first = viewport.first(&document, config).unwrap().unwrap();
        let page = viewport.step_forward(&document, first, 50, config).unwrap();
        let returned = viewport.step_backward(&document, page, 50, config).unwrap();
        let last = viewport.last(&document, config).unwrap().unwrap();
        let bottom = viewport
            .bottom_aligned_top(&document, 3, config)
            .unwrap()
            .unwrap();
        let frame = viewport
            .collect(&document, Some(bottom), 3, config)
            .unwrap();

        assert_eq!(returned, first);
        assert_eq!(page.byte_offset, 50);
        assert_eq!(last.byte_offset, 5_999);
        assert_eq!(bottom.byte_offset, 5_997);
        assert_eq!(line_texts(&frame), vec!["x", "x", "x"]);
        assert!(frame.at_end);
    }

    #[test]
    fn empty_document_collects_no_rows_and_still_validates_geometry() {
        let document = parse_bytes(b"").unwrap();
        let mut viewport = ViewportLayout::new();
        assert_eq!(
            viewport.collect(&document, None, 10, WIDE).unwrap(),
            super::ViewportRows {
                top: None,
                rows: Vec::new(),
                at_end: true,
                streaming_bytes: 0,
                eager_bytes: 0,
                byte_limited: false,
            }
        );
        assert!(matches!(
            viewport.first(&document, ViewportLayoutConfig::new(80, 0)),
            Err(ViewportLayoutError::Layout(LayoutError::InvalidTabWidth {
                actual: 0
            }))
        ));
    }
}
