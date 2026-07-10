//! Structural record と小さな文字列 payload を、共有 RAM budget と unlinked tempfile へ保存する。
//!
//! [`StructuralStorage`] は record table と blob arena が共有する encoded-resident ledger と、spill
//! 先の file descriptor を所有する。Ledger は record/blob page の確保済みbyte、record encode
//! scratch、固定control struct、page descriptor、sparse extent/blob entry、Arc allocation headerの
//! 2 word見積もりを課金する。Allocator block header、alignment/capacity slack、callerのread buffer、
//! convenience readの一時buffer、OS page cacheは含めない。この値は課金対象のencoded capacityと
//! logical entry数を256 MiB以内へ制限する契約であり、process RSSの上限ではない。
//!
//! Payload 枠を使い切った後の追加 data は固定長 extent へ直接書く。tempfile の作成、allocation、
//! positioned I/O のいずれかが失敗した場合は error を返し、RAM へ戻して保持しない。
//!
//! Disk encoding は caller が [`FixedRecord`] で明示する。Rust の型 layout や serde format に
//! 依存しないため、record width と stable ID の対応は process lifetime を通して変わらない。

use std::fs::{File, Permissions};
use std::io;
use std::marker::PhantomData;
use std::mem::size_of;
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use thiserror::Error;

/// Structural index 全体の既定 resident-memory 上限。
pub const STRUCTURAL_RESIDENT_BYTES: usize = 256 * 1024 * 1024;
/// Sparse extent directory と page descriptor 用に payload から分離する既定 reserve。
pub const STRUCTURAL_DIRECTORY_BYTES: usize = 4 * 1024 * 1024;
/// RAM record/blob をまとめる既定 page size。
pub const STRUCTURAL_PAGE_BYTES: usize = 64 * 1024;
/// Spill file 上で table record をまとめる既定 extent size。
pub const STRUCTURAL_EXTENT_BYTES: u64 = 8 * 1024 * 1024;

// BlobRefはdiskへserializeされるstructural fieldなので、pointerではなくprocess内で単調増加する
// tokenを持つ。2^64個のstorage/arena生成はaddress spaceと実行時間の双方を先に使い切る。
static NEXT_STORAGE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_ARENA_ID: AtomicU64 = AtomicU64::new(1);

/// Caller が所有する record の固定長 disk codec。
///
/// `ENCODED_LEN` は0より大きく、`encode` と `decode` はその長さの sliceだけを受け取る。実装は
/// integer の endian、optional value の sentinel、reserved byte を明示し、Rust のmemory layoutを
/// byte列へcopyしてはならない。
pub trait FixedRecord: Sized + Send + Sync + 'static {
    const ENCODED_LEN: usize;

    fn encode(&self, output: &mut [u8]) -> Result<(), RecordCodecError>;

    fn decode(input: &[u8]) -> Result<Self, RecordCodecError>;
}

/// Record codec が入力幅または field value を表現できなかった時のfailure。
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("{message}")]
pub struct RecordCodecError {
    message: &'static str,
}

impl RecordCodecError {
    #[must_use]
    pub const fn new(message: &'static str) -> Self {
        Self { message }
    }
}

/// Structural store の設定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageConfig {
    resident_limit_bytes: usize,
    directory_reserve_bytes: usize,
    ram_page_bytes: usize,
    disk_extent_bytes: u64,
}

impl StorageConfig {
    /// Test の小さい境界を含む任意のbudgetを検証して作る。
    pub fn new(
        resident_limit_bytes: usize,
        directory_reserve_bytes: usize,
        ram_page_bytes: usize,
        disk_extent_bytes: u64,
    ) -> Result<Self, StorageError> {
        if resident_limit_bytes == 0 {
            return Err(StorageError::InvalidConfig(
                "resident limit must be greater than zero",
            ));
        }
        if directory_reserve_bytes == 0 || directory_reserve_bytes > resident_limit_bytes {
            return Err(StorageError::InvalidConfig(
                "directory reserve must be within the resident limit",
            ));
        }
        if ram_page_bytes == 0 || ram_page_bytes > u32::MAX as usize {
            return Err(StorageError::InvalidConfig(
                "RAM page size must fit a non-zero u32",
            ));
        }
        if disk_extent_bytes == 0 {
            return Err(StorageError::InvalidConfig(
                "disk extent size must be greater than zero",
            ));
        }
        Ok(Self {
            resident_limit_bytes,
            directory_reserve_bytes,
            ram_page_bytes,
            disk_extent_bytes,
        })
    }

    #[must_use]
    pub const fn resident_limit_bytes(self) -> usize {
        self.resident_limit_bytes
    }

    #[must_use]
    pub const fn directory_reserve_bytes(self) -> usize {
        self.directory_reserve_bytes
    }

    #[must_use]
    pub const fn ram_page_bytes(self) -> usize {
        self.ram_page_bytes
    }

    #[must_use]
    pub const fn disk_extent_bytes(self) -> u64 {
        self.disk_extent_bytes
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            resident_limit_bytes: STRUCTURAL_RESIDENT_BYTES,
            directory_reserve_bytes: STRUCTURAL_DIRECTORY_BYTES,
            ram_page_bytes: STRUCTURAL_PAGE_BYTES,
            disk_extent_bytes: STRUCTURAL_EXTENT_BYTES,
        }
    }
}

/// 課金対象に限定したresident-memory ledgerの現在値。Process RSSとは一致しない。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResidentBytes {
    pub payload: usize,
    pub directory: usize,
    pub total: usize,
    pub limit: usize,
}

/// Structural storage のfailure。
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("invalid structural storage configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("structural storage {resource} lock was poisoned")]
    LockPoisoned { resource: &'static str },
    #[error("structural storage allocation of {requested} bytes failed")]
    Allocation { requested: usize },
    #[error(
        "structural sparse directory budget exceeded: requested {requested} bytes with {available} bytes available"
    )]
    DirectoryBudgetExceeded { requested: usize, available: usize },
    #[error("structural offset or length overflow")]
    OffsetOverflow,
    #[error("record codec failed: {0}")]
    Codec(#[from] RecordCodecError),
    #[error("invalid blob reference")]
    InvalidBlobReference,
    #[error("blob reference belongs to a different structural storage or arena")]
    BlobReferenceMismatch,
    #[error("structural storage I/O failed: {0}")]
    Io(#[from] io::Error),
}

#[derive(Clone, Debug)]
struct ResidentBudget {
    inner: Arc<Mutex<ResidentBudgetState>>,
    payload_limit: usize,
    directory_limit: usize,
}

#[derive(Debug, Default)]
struct ResidentBudgetState {
    payload: usize,
    directory: usize,
}

#[derive(Clone, Copy, Debug)]
enum ChargeKind {
    Payload,
    Directory,
}

#[derive(Debug)]
struct BudgetCharge {
    budget: ResidentBudget,
    kind: ChargeKind,
    bytes: usize,
}

impl Drop for BudgetCharge {
    fn drop(&mut self) {
        let mut state = match self.budget.inner.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        match self.kind {
            ChargeKind::Payload => state.payload = state.payload.saturating_sub(self.bytes),
            ChargeKind::Directory => {
                state.directory = state.directory.saturating_sub(self.bytes);
            }
        }
    }
}

impl ResidentBudget {
    fn new(config: StorageConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ResidentBudgetState::default())),
            payload_limit: config
                .resident_limit_bytes
                .saturating_sub(config.directory_reserve_bytes),
            directory_limit: config.directory_reserve_bytes,
        }
    }

    fn reserve_payload(&self, bytes: usize) -> Result<Option<BudgetCharge>, StorageError> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| StorageError::LockPoisoned { resource: "budget" })?;
        let Some(next) = state.payload.checked_add(bytes) else {
            return Ok(None);
        };
        if next > self.payload_limit {
            return Ok(None);
        }
        state.payload = next;
        Ok(Some(BudgetCharge {
            budget: self.clone(),
            kind: ChargeKind::Payload,
            bytes,
        }))
    }

    fn reserve_directory(&self, bytes: usize) -> Result<BudgetCharge, StorageError> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| StorageError::LockPoisoned { resource: "budget" })?;
        let next =
            state
                .directory
                .checked_add(bytes)
                .ok_or(StorageError::DirectoryBudgetExceeded {
                    requested: bytes,
                    available: self.directory_limit.saturating_sub(state.directory),
                })?;
        if next > self.directory_limit {
            return Err(StorageError::DirectoryBudgetExceeded {
                requested: bytes,
                available: self.directory_limit.saturating_sub(state.directory),
            });
        }
        state.directory = next;
        Ok(BudgetCharge {
            budget: self.clone(),
            kind: ChargeKind::Directory,
            bytes,
        })
    }

    fn snapshot(&self) -> Result<ResidentBytes, StorageError> {
        let state = self
            .inner
            .lock()
            .map_err(|_| StorageError::LockPoisoned { resource: "budget" })?;
        let total = state.payload.saturating_add(state.directory);
        Ok(ResidentBytes {
            payload: state.payload,
            directory: state.directory,
            total,
            limit: self.payload_limit.saturating_add(self.directory_limit),
        })
    }
}

/// Record table と blob arena が共有する budget と spill descriptor。
///
/// Tempfile は最初にRAM枠を超えたappendで作る。同じhandleから作ったtable/arenaは1つのdescriptorを
/// 共有し、互いに重ならないabsolute regionへpositioned writeする。
#[derive(Clone)]
pub struct StructuralStorage {
    inner: Arc<StructuralStorageInner>,
}

struct StructuralStorageInner {
    storage_id: u64,
    blob_arena_id: u64,
    config: StorageConfig,
    budget: ResidentBudget,
    spill: Mutex<SpillFileState>,
    blob_arena: Arc<RwLock<BlobArenaInner>>,
    _container_charge: BudgetCharge,
}

#[derive(Debug, Default)]
struct SpillFileState {
    file: Option<Arc<File>>,
    next_offset: u64,
    _file_charge: Option<BudgetCharge>,
}

struct PreparedDiskRegion<'a> {
    spill: MutexGuard<'a, SpillFileState>,
    file: Arc<File>,
    start: u64,
    end: u64,
}

impl PreparedDiskRegion<'_> {
    fn commit(mut self) -> (Arc<File>, u64) {
        self.spill.next_offset = self.end;
        (self.file, self.start)
    }
}

impl StructuralStorage {
    /// 256 MiB resident budgetを持つ既定storageを作る。
    pub fn new() -> Result<Self, StorageError> {
        Self::with_config(StorageConfig::default())
    }

    /// 検証済み設定からstorageを作る。小さいlimitを渡すとproductionと同じspill pathをtestできる。
    pub fn with_config(config: StorageConfig) -> Result<Self, StorageError> {
        let storage_id = NEXT_STORAGE_ID.fetch_add(1, Ordering::Relaxed);
        let blob_arena_id = NEXT_ARENA_ID.fetch_add(1, Ordering::Relaxed);
        let budget = ResidentBudget::new(config);
        // `size_of`で追える固定control structと3つのArc allocation headerを課金する。Allocatorの
        // block header/alignment slackはplatform依存なのでledger契約には含めない。
        let control_bytes = size_of::<StructuralStorageInner>()
            .saturating_add(size_of::<RwLock<BlobArenaInner>>())
            .saturating_add(size_of::<Mutex<ResidentBudgetState>>())
            .saturating_add(6 * size_of::<usize>());
        let container_charge = budget.reserve_directory(control_bytes)?;
        Ok(Self {
            inner: Arc::new(StructuralStorageInner {
                storage_id,
                blob_arena_id,
                config,
                budget,
                spill: Mutex::new(SpillFileState::default()),
                blob_arena: Arc::new(RwLock::new(BlobArenaInner {
                    pages: Vec::new(),
                    disk_blobs: Vec::new(),
                    spill_file: None,
                })),
                _container_charge: container_charge,
            }),
        })
    }

    /// 現在RAMに残るpayloadとsparse directoryの課金値を返す。
    pub fn resident_bytes(&self) -> Result<ResidentBytes, StorageError> {
        self.inner.budget.snapshot()
    }

    /// このstorageを共有する空の固定長record tableを作る。
    pub fn table<R: FixedRecord>(&self) -> Result<SpillTable<R>, StorageError> {
        SpillTable::new(self.clone())
    }

    /// このstorageを共有する空のblob arenaを作る。
    #[must_use]
    pub fn blob_arena(&self) -> BlobArena {
        BlobArena {
            storage: self.clone(),
            storage_id: self.inner.storage_id,
            arena_id: self.inner.blob_arena_id,
            inner: Arc::clone(&self.inner.blob_arena),
        }
    }

    #[cfg(test)]
    fn isolated_blob_arena(&self) -> BlobArena {
        BlobArena {
            storage: self.clone(),
            storage_id: self.inner.storage_id,
            arena_id: NEXT_ARENA_ID.fetch_add(1, Ordering::Relaxed),
            inner: Arc::new(RwLock::new(BlobArenaInner {
                pages: Vec::new(),
                disk_blobs: Vec::new(),
                spill_file: None,
            })),
        }
    }

    fn prepare_disk_region(&self, length: u64) -> Result<PreparedDiskRegion<'_>, StorageError> {
        let mut spill = self
            .inner
            .spill
            .lock()
            .map_err(|_| StorageError::LockPoisoned {
                resource: "spill allocator",
            })?;
        let end = spill
            .next_offset
            .checked_add(length)
            .ok_or(StorageError::OffsetOverflow)?;
        let file = match spill.file.as_ref() {
            Some(file) => Arc::clone(file),
            None => {
                // File本体とArc allocation headerはdescriptorの作成時だけ課金し、clone handleは
                // 各container structの`size_of`課金に含める。
                let file_control_bytes = size_of::<File>().saturating_add(2 * size_of::<usize>());
                let file_charge = self.inner.budget.reserve_directory(file_control_bytes)?;
                let file = Arc::new(secure_tempfile()?);
                spill.file = Some(Arc::clone(&file));
                spill._file_charge = Some(file_charge);
                file
            }
        };
        let start = spill.next_offset;
        Ok(PreparedDiskRegion {
            spill,
            file,
            start,
            end,
        })
    }
}

/// Append-only fixed-record table。
///
/// RAM pageを一度でも確保できなくなった後は、後続recordをすべてdisk extentへ書く。既存RAM pageを
/// evictionせず、`index`とrecordの対応を変えない。Disk extentは固定capacityなので、directoryは
/// recordごとではなくextentごとに1 entryだけを保持する。
pub struct SpillTable<R: FixedRecord> {
    storage: StructuralStorage,
    inner: Arc<RwLock<SpillTableInner>>,
    record_size: usize,
    records_per_page: usize,
    records_per_extent: u64,
    extent_bytes: u64,
    marker: PhantomData<R>,
}

struct SpillTableInner {
    ram_pages: Vec<RamRecordPage>,
    ram_records: u64,
    disk: Option<DiskRecordState>,
    len: u64,
    encode_buffer: Vec<u8>,
    _container_charge: BudgetCharge,
}

#[derive(Clone, Copy)]
struct RamAppendPlan {
    needs_page: bool,
    next_page_records: usize,
    next_ram_records: u64,
    start: usize,
    end: usize,
}

struct RamRecordPage {
    bytes: Vec<u8>,
    records: usize,
    _payload_charge: BudgetCharge,
    _directory_charge: BudgetCharge,
}

struct DiskRecordState {
    file: Arc<File>,
    extents: Vec<DiskExtent>,
}

struct DiskExtent {
    first_index: u64,
    file_offset: u64,
    records: u64,
    _directory_charge: BudgetCharge,
}

impl<R: FixedRecord> Clone for SpillTable<R> {
    fn clone(&self) -> Self {
        Self {
            storage: self.storage.clone(),
            inner: Arc::clone(&self.inner),
            record_size: self.record_size,
            records_per_page: self.records_per_page,
            records_per_extent: self.records_per_extent,
            extent_bytes: self.extent_bytes,
            marker: PhantomData,
        }
    }
}

impl<R: FixedRecord> SpillTable<R> {
    fn new(storage: StructuralStorage) -> Result<Self, StorageError> {
        if R::ENCODED_LEN == 0 {
            return Err(StorageError::InvalidConfig(
                "fixed record width must be greater than zero",
            ));
        }
        let record_size = R::ENCODED_LEN;
        let page_bytes = storage.inner.config.ram_page_bytes.max(record_size);
        let records_per_page = (page_bytes / record_size).max(1);
        let record_size_u64 =
            u64::try_from(record_size).map_err(|_| StorageError::OffsetOverflow)?;
        let extent_bytes = storage.inner.config.disk_extent_bytes.max(record_size_u64);
        let records_per_extent = (extent_bytes / record_size_u64).max(1);

        let control_bytes = record_size
            .saturating_add(size_of::<RwLock<SpillTableInner>>())
            .saturating_add(2 * size_of::<usize>());
        let container_charge = storage.inner.budget.reserve_directory(control_bytes)?;
        let mut encode_buffer = fallible_zeroed(record_size)?;
        encode_buffer.fill(0);
        Ok(Self {
            storage,
            inner: Arc::new(RwLock::new(SpillTableInner {
                ram_pages: Vec::new(),
                ram_records: 0,
                disk: None,
                len: 0,
                encode_buffer,
                _container_charge: container_charge,
            })),
            record_size,
            records_per_page,
            records_per_extent,
            extent_bytes,
            marker: PhantomData,
        })
    }

    /// Recordを末尾へ追加し、0始まりのstable indexを返す。
    pub fn append(&self, record: &R) -> Result<u64, StorageError> {
        let mut inner = self.inner.write().map_err(|_| StorageError::LockPoisoned {
            resource: "record table",
        })?;
        record.encode(&mut inner.encode_buffer)?;
        let index = inner.len;
        let next_len = inner
            .len
            .checked_add(1)
            .ok_or(StorageError::OffsetOverflow)?;

        let ram_plan = if inner.disk.is_none() {
            let (needs_page, current_page_records) = match inner.ram_pages.last() {
                Some(page) if page.records < self.records_per_page => (false, page.records),
                Some(page) if page.records == self.records_per_page => (true, 0),
                Some(_) => return Err(StorageError::OffsetOverflow),
                None => (true, 0),
            };
            let next_page_records = current_page_records
                .checked_add(1)
                .filter(|records| *records <= self.records_per_page)
                .ok_or(StorageError::OffsetOverflow)?;
            let next_ram_records = inner
                .ram_records
                .checked_add(1)
                .ok_or(StorageError::OffsetOverflow)?;
            let start = current_page_records
                .checked_mul(self.record_size)
                .ok_or(StorageError::OffsetOverflow)?;
            let end = start
                .checked_add(self.record_size)
                .ok_or(StorageError::OffsetOverflow)?;
            Some(RamAppendPlan {
                needs_page,
                next_page_records,
                next_ram_records,
                start,
                end,
            })
        } else {
            None
        };

        if let Some(plan) = ram_plan
            && plan.needs_page
            && !self.try_allocate_ram_page(&mut inner)?
        {
            self.allocate_disk_extent(&mut inner)?;
        }

        if inner.disk.is_none() {
            let plan = ram_plan.ok_or(StorageError::OffsetOverflow)?;
            let SpillTableInner {
                ram_pages,
                ram_records,
                len,
                encode_buffer,
                ..
            } = &mut *inner;
            let page = ram_pages
                .last_mut()
                .ok_or(StorageError::InvalidConfig("RAM page was not allocated"))?;
            let destination = page
                .bytes
                .get_mut(plan.start..plan.end)
                .ok_or(StorageError::OffsetOverflow)?;
            destination.copy_from_slice(encode_buffer);
            page.records = plan.next_page_records;
            *ram_records = plan.next_ram_records;
            *len = next_len;
            return Ok(index);
        }

        let needs_extent = inner.disk.as_ref().is_some_and(|disk| {
            disk.extents
                .last()
                .is_none_or(|extent| extent.records >= self.records_per_extent)
        });
        if needs_extent {
            self.allocate_disk_extent(&mut inner)?;
        }
        let (file, offset, next_extent_records) = {
            let disk = inner
                .disk
                .as_ref()
                .ok_or(StorageError::InvalidConfig("disk state was not allocated"))?;
            let extent = disk
                .extents
                .last()
                .ok_or(StorageError::InvalidConfig("disk extent was not allocated"))?;
            let relative = extent
                .records
                .checked_mul(
                    u64::try_from(self.record_size).map_err(|_| StorageError::OffsetOverflow)?,
                )
                .ok_or(StorageError::OffsetOverflow)?;
            let next_extent_records = extent
                .records
                .checked_add(1)
                .filter(|records| *records <= self.records_per_extent)
                .ok_or(StorageError::OffsetOverflow)?;
            (
                Arc::clone(&disk.file),
                extent
                    .file_offset
                    .checked_add(relative)
                    .ok_or(StorageError::OffsetOverflow)?,
                next_extent_records,
            )
        };
        write_all_at(&file, &inner.encode_buffer, offset)?;
        if let Some(extent) = inner.disk.as_mut().and_then(|disk| disk.extents.last_mut()) {
            extent.records = next_extent_records;
        }
        inner.len = next_len;
        Ok(index)
    }

    /// Stable indexのrecordをdecodeする便利API。現在のhigh-water mark以降は`None`を返す。
    ///
    /// Disk recordでは1 record分の一時Vecを作る。このbufferは256 MiB ledgerに含めない。Repeated
    /// readを行うworkerは[`Self::get_with_buffer`]へ専用bufferを渡し、allocationを再利用する。
    pub fn get(&self, index: u64) -> Result<Option<R>, StorageError> {
        let mut buffer = Vec::new();
        self.get_with_buffer(index, &mut buffer)
    }

    /// Stable indexのrecordをdecodeし、disk read時はcallerのbuffer capacityを再利用する。
    ///
    /// Bufferはcallerが所有する一時領域なのでstructural ledgerへ課金しない。RAM recordを読む場合は
    /// buffer内容を変更しない。
    pub fn get_with_buffer(
        &self,
        index: u64,
        buffer: &mut Vec<u8>,
    ) -> Result<Option<R>, StorageError> {
        let inner = self.inner.read().map_err(|_| StorageError::LockPoisoned {
            resource: "record table",
        })?;
        if index >= inner.len {
            return Ok(None);
        }
        if index < inner.ram_records {
            let index = usize::try_from(index).map_err(|_| StorageError::OffsetOverflow)?;
            let page_index = index / self.records_per_page;
            let slot = index % self.records_per_page;
            let page = inner
                .ram_pages
                .get(page_index)
                .ok_or(StorageError::OffsetOverflow)?;
            if slot >= page.records {
                return Err(StorageError::OffsetOverflow);
            }
            let start = slot
                .checked_mul(self.record_size)
                .ok_or(StorageError::OffsetOverflow)?;
            let end = start
                .checked_add(self.record_size)
                .ok_or(StorageError::OffsetOverflow)?;
            return R::decode(&page.bytes[start..end])
                .map(Some)
                .map_err(StorageError::from);
        }

        let relative = index
            .checked_sub(inner.ram_records)
            .ok_or(StorageError::OffsetOverflow)?;
        let extent_index = usize::try_from(relative / self.records_per_extent)
            .map_err(|_| StorageError::OffsetOverflow)?;
        let slot = relative % self.records_per_extent;
        let disk = inner.disk.as_ref().ok_or(StorageError::OffsetOverflow)?;
        let extent = disk
            .extents
            .get(extent_index)
            .ok_or(StorageError::OffsetOverflow)?;
        if index < extent.first_index || slot >= extent.records {
            return Err(StorageError::OffsetOverflow);
        }
        let offset = extent
            .file_offset
            .checked_add(
                slot.checked_mul(
                    u64::try_from(self.record_size).map_err(|_| StorageError::OffsetOverflow)?,
                )
                .ok_or(StorageError::OffsetOverflow)?,
            )
            .ok_or(StorageError::OffsetOverflow)?;
        prepare_output(buffer, self.record_size)?;
        read_exact_at(&disk.file, buffer, offset)?;
        R::decode(buffer).map(Some).map_err(StorageError::from)
    }

    /// Append済みrecord数を返す。
    pub fn len(&self) -> Result<u64, StorageError> {
        let inner = self.inner.read().map_err(|_| StorageError::LockPoisoned {
            resource: "record table",
        })?;
        Ok(inner.len)
    }

    /// Recordが1件もないかを返す。
    pub fn is_empty(&self) -> Result<bool, StorageError> {
        self.len().map(|len| len == 0)
    }

    /// RAM枠を超え、1件以上をspill領域へ割り当てたかを返す。
    pub fn is_spilled(&self) -> Result<bool, StorageError> {
        let inner = self.inner.read().map_err(|_| StorageError::LockPoisoned {
            resource: "record table",
        })?;
        Ok(inner.disk.is_some())
    }

    fn try_allocate_ram_page(&self, inner: &mut SpillTableInner) -> Result<bool, StorageError> {
        let next_page_count = inner
            .ram_pages
            .len()
            .checked_add(1)
            .ok_or(StorageError::OffsetOverflow)?;
        let page_bytes = self
            .records_per_page
            .checked_mul(self.record_size)
            .ok_or(StorageError::OffsetOverflow)?;
        let Some(payload_charge) = self.storage.inner.budget.reserve_payload(page_bytes)? else {
            return Ok(false);
        };
        let directory_charge = self
            .storage
            .inner
            .budget
            .reserve_directory(size_of::<RamRecordPage>())?;
        let bytes = fallible_zeroed(page_bytes)?;
        inner
            .ram_pages
            .try_reserve_exact(1)
            .map_err(|_| StorageError::Allocation {
                requested: size_of::<RamRecordPage>(),
            })?;
        inner.ram_pages.push(RamRecordPage {
            bytes,
            records: 0,
            _payload_charge: payload_charge,
            _directory_charge: directory_charge,
        });
        debug_assert_eq!(inner.ram_pages.len(), next_page_count);
        Ok(true)
    }

    fn allocate_disk_extent(&self, inner: &mut SpillTableInner) -> Result<(), StorageError> {
        // Offset overflowはdirectory ledgerやVec capacityを変更する前に判定する。Guardを保持して
        // directoryを確保し終えてからcommitするため、並行tableのregionも重ならない。
        let prepared = self.storage.prepare_disk_region(self.extent_bytes)?;
        let directory_charge = self
            .storage
            .inner
            .budget
            .reserve_directory(size_of::<DiskExtent>())?;
        match inner.disk.as_mut() {
            Some(disk) => {
                disk.extents
                    .try_reserve_exact(1)
                    .map_err(|_| StorageError::Allocation {
                        requested: size_of::<DiskExtent>(),
                    })?;
                let (file, file_offset) = prepared.commit();
                debug_assert!(Arc::ptr_eq(&disk.file, &file));
                disk.extents.push(DiskExtent {
                    first_index: inner.len,
                    file_offset,
                    records: 0,
                    _directory_charge: directory_charge,
                });
            }
            None => {
                let mut extents = Vec::new();
                extents
                    .try_reserve_exact(1)
                    .map_err(|_| StorageError::Allocation {
                        requested: size_of::<DiskExtent>(),
                    })?;
                let (file, file_offset) = prepared.commit();
                extents.push(DiskExtent {
                    first_index: inner.len,
                    file_offset,
                    records: 0,
                    _directory_charge: directory_charge,
                });
                inner.disk = Some(DiskRecordState { file, extents });
            }
        }
        Ok(())
    }
}

/// Blob の保存 tier と位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobLocation {
    Empty,
    Memory { page: u32, offset: u32 },
    Disk { offset: u64 },
}

/// Blob arenaへappendしたbyte列のstable reference。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlobRef {
    storage_id: u64,
    arena_id: u64,
    location: BlobLocation,
    len: u64,
}

impl BlobRef {
    #[must_use]
    pub const fn location(self) -> BlobLocation {
        self.location
    }

    #[must_use]
    pub const fn len(self) -> u64 {
        self.len
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// Path、commit subject、object IDのような可変長structural payloadを保存するarena。
///
/// 1 blobは1つのRAM page内、またはspill file上の連続領域へ置く。RAM pageに収まらないappendで
/// payload budgetを確保できない場合はdiskへ直接書き、disk write failure後にRAMへ複写し直さない。
#[derive(Clone)]
pub struct BlobArena {
    storage: StructuralStorage,
    storage_id: u64,
    arena_id: u64,
    inner: Arc<RwLock<BlobArenaInner>>,
}

struct BlobArenaInner {
    pages: Vec<BlobPage>,
    disk_blobs: Vec<DiskBlob>,
    spill_file: Option<Arc<File>>,
}

struct BlobPage {
    bytes: Vec<u8>,
    used: usize,
    _payload_charge: BudgetCharge,
    _directory_charge: BudgetCharge,
}

struct DiskBlob {
    offset: u64,
    len: u64,
    _directory_charge: BudgetCharge,
}

impl BlobArena {
    /// Byte列をarenaへappendし、arena lifetime中に不変のreferenceを返す。
    pub fn append(&self, bytes: &[u8]) -> Result<BlobRef, StorageError> {
        if bytes.is_empty() {
            return Ok(BlobRef {
                storage_id: self.storage_id,
                arena_id: self.arena_id,
                location: BlobLocation::Empty,
                len: 0,
            });
        }
        let len = u64::try_from(bytes.len()).map_err(|_| StorageError::OffsetOverflow)?;
        let mut inner = self.inner.write().map_err(|_| StorageError::LockPoisoned {
            resource: "blob arena",
        })?;

        let last_page_index = inner.pages.len().checked_sub(1);
        if let Some(page) = inner.pages.last_mut()
            && page.bytes.len().saturating_sub(page.used) >= bytes.len()
        {
            let page_index = u32::try_from(last_page_index.ok_or(StorageError::OffsetOverflow)?)
                .map_err(|_| StorageError::OffsetOverflow)?;
            let offset = u32::try_from(page.used).map_err(|_| StorageError::OffsetOverflow)?;
            let end = page
                .used
                .checked_add(bytes.len())
                .ok_or(StorageError::OffsetOverflow)?;
            page.bytes[page.used..end].copy_from_slice(bytes);
            page.used = end;
            return Ok(BlobRef {
                storage_id: self.storage_id,
                arena_id: self.arena_id,
                location: BlobLocation::Memory {
                    page: page_index,
                    offset,
                },
                len,
            });
        }

        let page_bytes = self.storage.inner.config.ram_page_bytes.max(bytes.len());
        let next_page_index =
            u32::try_from(inner.pages.len()).map_err(|_| StorageError::OffsetOverflow)?;
        if page_bytes <= u32::MAX as usize
            && let Some(payload_charge) = self.storage.inner.budget.reserve_payload(page_bytes)?
        {
            let directory_charge = self
                .storage
                .inner
                .budget
                .reserve_directory(size_of::<BlobPage>())?;
            let mut page_bytes_buffer = fallible_zeroed(page_bytes)?;
            page_bytes_buffer[..bytes.len()].copy_from_slice(bytes);
            inner
                .pages
                .try_reserve_exact(1)
                .map_err(|_| StorageError::Allocation {
                    requested: size_of::<BlobPage>(),
                })?;
            inner.pages.push(BlobPage {
                bytes: page_bytes_buffer,
                used: bytes.len(),
                _payload_charge: payload_charge,
                _directory_charge: directory_charge,
            });
            return Ok(BlobRef {
                storage_id: self.storage_id,
                arena_id: self.arena_id,
                location: BlobLocation::Memory {
                    page: next_page_index,
                    offset: 0,
                },
                len,
            });
        }

        let prepared = self.storage.prepare_disk_region(len)?;
        let directory_charge = self
            .storage
            .inner
            .budget
            .reserve_directory(size_of::<DiskBlob>())?;
        inner
            .disk_blobs
            .try_reserve_exact(1)
            .map_err(|_| StorageError::Allocation {
                requested: size_of::<DiskBlob>(),
            })?;
        let (file, offset) = prepared.commit();
        write_all_at(&file, bytes, offset)?;
        inner.disk_blobs.push(DiskBlob {
            offset,
            len,
            _directory_charge: directory_charge,
        });
        inner.spill_file = Some(file);
        Ok(BlobRef {
            storage_id: self.storage_id,
            arena_id: self.arena_id,
            location: BlobLocation::Disk { offset },
            len,
        })
    }

    /// Blobを新しいVecへcopyして返す。
    pub fn read(&self, reference: BlobRef) -> Result<Vec<u8>, StorageError> {
        let mut output = Vec::new();
        self.read_into(reference, &mut output)?;
        Ok(output)
    }

    /// Packed recordからdecodeしたlocation/lengthを、このarenaに属する[`BlobRef`]へ戻す。
    ///
    /// Identity tokenはcallerから受け取らず、このarenaの値を付与する。`Empty`はlength 0だけ、RAM
    /// locationはappend済みpageの使用範囲、disk locationはこのarenaがappendした完全一致rangeだけを
    /// 受け付ける。別tableのspill領域をblobとしてbindすることはできない。
    pub fn bind(&self, location: BlobLocation, len: u64) -> Result<BlobRef, StorageError> {
        match (location, len) {
            (BlobLocation::Empty, 0) => {}
            (BlobLocation::Empty, _) | (_, 0) => {
                return Err(StorageError::InvalidBlobReference);
            }
            (BlobLocation::Memory { page, offset }, len) => {
                let requested =
                    usize::try_from(len).map_err(|_| StorageError::InvalidBlobReference)?;
                let inner = self.inner.read().map_err(|_| StorageError::LockPoisoned {
                    resource: "blob arena",
                })?;
                let page = inner
                    .pages
                    .get(usize::try_from(page).map_err(|_| StorageError::InvalidBlobReference)?)
                    .ok_or(StorageError::InvalidBlobReference)?;
                let start =
                    usize::try_from(offset).map_err(|_| StorageError::InvalidBlobReference)?;
                let _ = start
                    .checked_add(requested)
                    .filter(|end| *end <= page.used)
                    .ok_or(StorageError::InvalidBlobReference)?;
            }
            (BlobLocation::Disk { offset }, len) => {
                let inner = self.inner.read().map_err(|_| StorageError::LockPoisoned {
                    resource: "blob arena",
                })?;
                let index = inner
                    .disk_blobs
                    .binary_search_by_key(&offset, |blob| blob.offset)
                    .map_err(|_| StorageError::InvalidBlobReference)?;
                if inner.disk_blobs[index].len != len {
                    return Err(StorageError::InvalidBlobReference);
                }
            }
        }
        Ok(BlobRef {
            storage_id: self.storage_id,
            arena_id: self.arena_id,
            location,
            len,
        })
    }

    /// Blobをcallerのbufferへcopyする。既存capacityは再利用し、buffer内容は置き換える。
    pub fn read_into(&self, reference: BlobRef, output: &mut Vec<u8>) -> Result<(), StorageError> {
        if reference.storage_id != self.storage_id || reference.arena_id != self.arena_id {
            return Err(StorageError::BlobReferenceMismatch);
        }
        let requested = usize::try_from(reference.len).map_err(|_| StorageError::OffsetOverflow)?;
        prepare_output(output, requested)?;
        if reference.is_empty() {
            return matches!(reference.location, BlobLocation::Empty)
                .then_some(())
                .ok_or(StorageError::InvalidBlobReference);
        }
        let inner = self.inner.read().map_err(|_| StorageError::LockPoisoned {
            resource: "blob arena",
        })?;
        match reference.location {
            BlobLocation::Empty => Err(StorageError::InvalidBlobReference),
            BlobLocation::Memory { page, offset } => {
                let page = inner
                    .pages
                    .get(usize::try_from(page).map_err(|_| StorageError::OffsetOverflow)?)
                    .ok_or(StorageError::InvalidBlobReference)?;
                let start =
                    usize::try_from(offset).map_err(|_| StorageError::InvalidBlobReference)?;
                let end = start
                    .checked_add(requested)
                    .filter(|end| *end <= page.used)
                    .ok_or(StorageError::InvalidBlobReference)?;
                output.copy_from_slice(&page.bytes[start..end]);
                Ok(())
            }
            BlobLocation::Disk { offset } => {
                let file = inner
                    .spill_file
                    .as_ref()
                    .ok_or(StorageError::InvalidBlobReference)?;
                read_exact_at(file, output, offset).map_err(StorageError::from)
            }
        }
    }
}

fn fallible_zeroed(length: usize) -> Result<Vec<u8>, StorageError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| StorageError::Allocation { requested: length })?;
    output.resize(length, 0);
    Ok(output)
}

fn prepare_output(output: &mut Vec<u8>, length: usize) -> Result<(), StorageError> {
    output.clear();
    output
        .try_reserve_exact(length)
        .map_err(|_| StorageError::Allocation { requested: length })?;
    output.resize(length, 0);
    Ok(())
}

pub(crate) fn secure_tempfile() -> io::Result<File> {
    let file = tempfile::tempfile()?;
    file.set_permissions(Permissions::from_mode(0o600))?;
    Ok(file)
}

pub(crate) fn read_exact_at(file: &File, mut output: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !output.is_empty() {
        match file.read_at(output, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "positioned read reached EOF",
                ));
            }
            Ok(count) => {
                offset = offset
                    .checked_add(count as u64)
                    .ok_or_else(|| io::Error::other("positioned read offset overflow"))?;
                output = &mut output[count..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(crate) fn write_all_at(file: &File, mut input: &[u8], mut offset: u64) -> io::Result<()> {
    while !input.is_empty() {
        match file.write_at(input, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "positioned write made no progress",
                ));
            }
            Ok(count) => {
                offset = offset
                    .checked_add(count as u64)
                    .ok_or_else(|| io::Error::other("positioned write offset overflow"))?;
                input = &input[count..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct TestRecord {
        ordinal: u64,
        tag: u32,
    }

    impl FixedRecord for TestRecord {
        const ENCODED_LEN: usize = 12;

        fn encode(&self, output: &mut [u8]) -> Result<(), RecordCodecError> {
            if output.len() != Self::ENCODED_LEN {
                return Err(RecordCodecError::new("wrong test record output width"));
            }
            output[..8].copy_from_slice(&self.ordinal.to_le_bytes());
            output[8..12].copy_from_slice(&self.tag.to_le_bytes());
            Ok(())
        }

        fn decode(input: &[u8]) -> Result<Self, RecordCodecError> {
            if input.len() != Self::ENCODED_LEN {
                return Err(RecordCodecError::new("wrong test record input width"));
            }
            let ordinal = u64::from_le_bytes(
                input[..8]
                    .try_into()
                    .map_err(|_| RecordCodecError::new("invalid ordinal bytes"))?,
            );
            let tag = u32::from_le_bytes(
                input[8..12]
                    .try_into()
                    .map_err(|_| RecordCodecError::new("invalid tag bytes"))?,
            );
            Ok(Self { ordinal, tag })
        }
    }

    fn tiny_config() -> StorageConfig {
        StorageConfig::new(2_048, 1_792, 64, 120).unwrap()
    }

    fn disk_only_config(extent_bytes: u64) -> StorageConfig {
        StorageConfig::new(16 * 1024, 16 * 1024, 64, extent_bytes).unwrap()
    }

    fn blob_config() -> StorageConfig {
        StorageConfig::new(2_048, 1_984, 32, 64).unwrap()
    }

    #[test]
    fn fixed_records_cross_ram_and_sparse_disk_extents() {
        let storage = StructuralStorage::with_config(tiny_config()).unwrap();
        let table = storage.table::<TestRecord>().unwrap();
        for ordinal in 0..40_u64 {
            let index = table
                .append(&TestRecord {
                    ordinal,
                    tag: ordinal as u32 ^ 0x55aa,
                })
                .unwrap();
            assert_eq!(index, ordinal);
        }

        assert!(table.is_spilled().unwrap());
        assert_eq!(table.len().unwrap(), 40);
        for ordinal in 0..40_u64 {
            assert_eq!(
                table.get(ordinal).unwrap(),
                Some(TestRecord {
                    ordinal,
                    tag: ordinal as u32 ^ 0x55aa,
                })
            );
        }
        assert_eq!(table.get(40).unwrap(), None);
        let resident = storage.resident_bytes().unwrap();
        assert!(resident.total <= resident.limit);
        assert!(resident.payload <= 256);
        assert!(resident.directory <= 1_792);
    }

    #[test]
    fn cloned_table_observes_the_same_append_only_records() {
        let storage = StructuralStorage::with_config(tiny_config()).unwrap();
        let table = storage.table::<TestRecord>().unwrap();
        let clone = table.clone();
        table
            .append(&TestRecord {
                ordinal: 7,
                tag: 11,
            })
            .unwrap();
        assert_eq!(
            clone.get(0).unwrap(),
            Some(TestRecord {
                ordinal: 7,
                tag: 11
            })
        );
    }

    #[test]
    fn blob_arena_keeps_stable_memory_and_disk_references() {
        let storage = StructuralStorage::with_config(blob_config()).unwrap();
        let arena = storage.blob_arena();
        let first = arena.append(b"small blob").unwrap();
        let second = arena.append(&[b'x'; 80]).unwrap();
        let empty = arena.append(b"").unwrap();

        assert!(matches!(first.location(), BlobLocation::Memory { .. }));
        assert!(matches!(second.location(), BlobLocation::Disk { .. }));
        assert_eq!(arena.read(first).unwrap(), b"small blob");
        assert_eq!(arena.read(second).unwrap(), vec![b'x'; 80]);
        assert_eq!(arena.read(empty).unwrap(), b"");
        let resident = storage.resident_bytes().unwrap();
        assert!(resident.total <= resident.limit);
    }

    #[test]
    fn packed_blob_locations_bind_back_to_the_origin_arena() {
        let storage = StructuralStorage::with_config(blob_config()).unwrap();
        let arena = storage.blob_arena();
        let empty = arena.append(b"").unwrap();
        let memory = arena.append(b"memory").unwrap();
        let disk = arena.append(&[b'd'; 80]).unwrap();

        for reference in [empty, memory, disk] {
            let rebound = arena.bind(reference.location(), reference.len()).unwrap();
            assert_eq!(rebound, reference);
            assert_eq!(arena.read(rebound).unwrap(), arena.read(reference).unwrap());
        }

        assert!(matches!(
            arena.bind(BlobLocation::Empty, 1),
            Err(StorageError::InvalidBlobReference)
        ));
        assert!(matches!(
            arena.bind(memory.location(), 0),
            Err(StorageError::InvalidBlobReference)
        ));
        assert!(matches!(
            arena.bind(disk.location(), disk.len() + 1),
            Err(StorageError::InvalidBlobReference)
        ));
    }

    #[test]
    fn shared_budget_forces_later_table_to_spill() {
        let storage = StructuralStorage::with_config(tiny_config()).unwrap();
        let first = storage.table::<TestRecord>().unwrap();
        let second = storage.table::<TestRecord>().unwrap();
        for ordinal in 0..20_u64 {
            first.append(&TestRecord { ordinal, tag: 1 }).unwrap();
        }
        for ordinal in 0..20_u64 {
            second.append(&TestRecord { ordinal, tag: 2 }).unwrap();
        }
        assert!(first.is_spilled().unwrap() || second.is_spilled().unwrap());
        assert!(storage.resident_bytes().unwrap().total <= 2_048);
    }

    #[test]
    fn exhausted_sparse_directory_returns_error_without_growing_ram() {
        let base = StructuralStorage::with_config(tiny_config()).unwrap();
        let before_table = base.resident_bytes().unwrap().directory;
        let table = base.table::<TestRecord>().unwrap();
        let after_table = base.resident_bytes().unwrap().directory;
        let table_control = after_table - before_table;
        drop(table);
        drop(base);

        let storage_control = StructuralStorage::with_config(tiny_config())
            .unwrap()
            .resident_bytes()
            .unwrap()
            .directory;
        let directory = storage_control + table_control + size_of::<RamRecordPage>();
        let config = StorageConfig::new(directory + 64, directory, 64, 120).unwrap();
        let storage = StructuralStorage::with_config(config).unwrap();
        let table = storage.table::<TestRecord>().unwrap();
        for ordinal in 0..5_u64 {
            table.append(&TestRecord { ordinal, tag: 1 }).unwrap();
        }

        let error = table
            .append(&TestRecord { ordinal: 5, tag: 1 })
            .unwrap_err();
        assert!(matches!(
            error,
            StorageError::DirectoryBudgetExceeded { .. }
        ));
        assert_eq!(table.len().unwrap(), 5);
        assert!(!table.is_spilled().unwrap());
        let resident = storage.resident_bytes().unwrap();
        assert!(resident.total <= resident.limit);
    }

    #[test]
    fn repeated_blob_handles_share_one_arena_and_reject_foreign_references() {
        let first_storage = StructuralStorage::with_config(blob_config()).unwrap();
        let first = first_storage.blob_arena();
        let repeated = first_storage.blob_arena();
        assert!(Arc::ptr_eq(&first.inner, &repeated.inner));
        let resident_before_handles = first_storage.resident_bytes().unwrap();
        for _ in 0..1_000 {
            drop(first_storage.blob_arena());
        }
        assert_eq!(
            first_storage.resident_bytes().unwrap(),
            resident_before_handles
        );

        let memory = first.append(b"memory").unwrap();
        let disk = first.append(&[b'd'; 80]).unwrap();
        assert_eq!(repeated.read(memory).unwrap(), b"memory");
        assert_eq!(repeated.read(disk).unwrap(), vec![b'd'; 80]);

        let second_storage = StructuralStorage::with_config(blob_config()).unwrap();
        let second = second_storage.blob_arena();
        for reference in [memory, disk] {
            let mut untouched = b"keep".to_vec();
            assert!(matches!(
                second.read_into(reference, &mut untouched),
                Err(StorageError::BlobReferenceMismatch)
            ));
            assert_eq!(untouched, b"keep");
        }

        let isolated = first_storage.isolated_blob_arena();
        assert!(matches!(
            isolated.read(memory),
            Err(StorageError::BlobReferenceMismatch)
        ));
    }

    #[test]
    fn non_divisible_extent_width_keeps_every_record_addressable() {
        let storage = StructuralStorage::with_config(disk_only_config(25)).unwrap();
        let table = storage.table::<TestRecord>().unwrap();
        assert_eq!(table.records_per_extent, 2);
        for ordinal in 0..7_u64 {
            table
                .append(&TestRecord {
                    ordinal,
                    tag: ordinal as u32,
                })
                .unwrap();
        }
        for ordinal in 0..7_u64 {
            assert_eq!(table.get(ordinal).unwrap().unwrap().ordinal, ordinal);
        }

        let inner = table.inner.read().unwrap();
        let extents = &inner.disk.as_ref().unwrap().extents;
        assert_eq!(extents.len(), 4);
        assert!(
            extents
                .windows(2)
                .all(|pair| pair[1].file_offset - pair[0].file_offset == 25)
        );
    }

    #[test]
    fn tables_and_blob_arena_share_non_overlapping_spill_regions() {
        let storage = StructuralStorage::with_config(disk_only_config(25)).unwrap();
        let first = storage.table::<TestRecord>().unwrap();
        let second = storage.table::<TestRecord>().unwrap();
        let blobs = storage.blob_arena();

        first
            .append(&TestRecord {
                ordinal: 11,
                tag: 1,
            })
            .unwrap();
        let blob = blobs.append(b"between table extents").unwrap();
        second
            .append(&TestRecord {
                ordinal: 22,
                tag: 2,
            })
            .unwrap();
        first
            .append(&TestRecord {
                ordinal: 33,
                tag: 3,
            })
            .unwrap();

        assert_eq!(first.get(0).unwrap().unwrap().ordinal, 11);
        assert_eq!(first.get(1).unwrap().unwrap().ordinal, 33);
        assert_eq!(second.get(0).unwrap().unwrap().ordinal, 22);
        assert_eq!(blobs.read(blob).unwrap(), b"between table extents");
        let first_file = Arc::clone(&first.inner.read().unwrap().disk.as_ref().unwrap().file);
        let second_file = Arc::clone(&second.inner.read().unwrap().disk.as_ref().unwrap().file);
        assert!(Arc::ptr_eq(&first_file, &second_file));
    }

    #[test]
    fn concurrent_appends_cross_the_spill_boundary_without_reusing_indices() {
        let config = StorageConfig::new(64 * 1024, 64 * 1024 - 256, 64, 65).unwrap();
        let storage = StructuralStorage::with_config(config).unwrap();
        let table = storage.table::<TestRecord>().unwrap();
        let mut threads = Vec::new();
        for worker in 0..8_u64 {
            let table = table.clone();
            threads.push(std::thread::spawn(move || {
                let mut appended = Vec::new();
                for item in 0..50_u64 {
                    let ordinal = worker * 1_000 + item;
                    let index = table
                        .append(&TestRecord {
                            ordinal,
                            tag: worker as u32,
                        })
                        .unwrap();
                    appended.push((index, ordinal));
                }
                appended
            }));
        }
        let mut appended = threads
            .into_iter()
            .flat_map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        appended.sort_unstable_by_key(|(index, _)| *index);
        assert_eq!(appended.len(), 400);
        for (expected_index, (index, ordinal)) in appended.into_iter().enumerate() {
            assert_eq!(index, expected_index as u64);
            assert_eq!(table.get(index).unwrap().unwrap().ordinal, ordinal);
        }
        assert!(table.is_spilled().unwrap());
    }

    #[test]
    fn reusable_record_buffer_is_unchanged_for_ram_and_reused_for_disk() {
        let storage = StructuralStorage::with_config(tiny_config()).unwrap();
        let table = storage.table::<TestRecord>().unwrap();
        for ordinal in 0..40_u64 {
            table
                .append(&TestRecord {
                    ordinal,
                    tag: ordinal as u32,
                })
                .unwrap();
        }
        let mut buffer = Vec::with_capacity(64);
        buffer.extend_from_slice(b"unchanged");
        let capacity = buffer.capacity();
        assert_eq!(
            table
                .get_with_buffer(0, &mut buffer)
                .unwrap()
                .unwrap()
                .ordinal,
            0
        );
        assert_eq!(buffer, b"unchanged");
        assert_eq!(
            table
                .get_with_buffer(39, &mut buffer)
                .unwrap()
                .unwrap()
                .ordinal,
            39
        );
        assert_eq!(buffer.capacity(), capacity);
        assert_eq!(buffer.len(), TestRecord::ENCODED_LEN);
    }

    #[test]
    fn counter_overflow_is_detected_before_ram_record_bytes_change() {
        let storage = StructuralStorage::with_config(tiny_config()).unwrap();
        let table = storage.table::<TestRecord>().unwrap();
        table.append(&TestRecord { ordinal: 1, tag: 1 }).unwrap();
        let (before_bytes, before_records, before_ram_records) = {
            let mut inner = table.inner.write().unwrap();
            inner.len = u64::MAX;
            (
                inner.ram_pages[0].bytes.clone(),
                inner.ram_pages[0].records,
                inner.ram_records,
            )
        };

        assert!(matches!(
            table.append(&TestRecord { ordinal: 2, tag: 2 }),
            Err(StorageError::OffsetOverflow)
        ));
        let inner = table.inner.read().unwrap();
        assert_eq!(inner.ram_pages[0].bytes, before_bytes);
        assert_eq!(inner.ram_pages[0].records, before_records);
        assert_eq!(inner.ram_records, before_ram_records);
    }

    #[test]
    fn ram_counter_overflow_is_detected_before_allocating_an_empty_page() {
        let storage = StructuralStorage::with_config(tiny_config()).unwrap();
        let table = storage.table::<TestRecord>().unwrap();
        table.inner.write().unwrap().ram_records = u64::MAX;
        let resident_before = storage.resident_bytes().unwrap();

        assert!(matches!(
            table.append(&TestRecord { ordinal: 1, tag: 1 }),
            Err(StorageError::OffsetOverflow)
        ));

        let inner = table.inner.read().unwrap();
        assert!(inner.ram_pages.is_empty());
        assert_eq!(inner.len, 0);
        assert_eq!(storage.resident_bytes().unwrap(), resident_before);
    }

    #[test]
    fn spill_offset_overflow_precedes_directory_and_capacity_changes() {
        let storage = StructuralStorage::with_config(disk_only_config(25)).unwrap();
        let table = storage.table::<TestRecord>().unwrap();
        let arena = storage.blob_arena();
        storage.inner.spill.lock().unwrap().next_offset = u64::MAX;
        let resident_before = storage.resident_bytes().unwrap();

        assert!(matches!(
            table.append(&TestRecord { ordinal: 1, tag: 1 }),
            Err(StorageError::OffsetOverflow)
        ));
        assert!(table.inner.read().unwrap().disk.is_none());
        assert_eq!(storage.resident_bytes().unwrap(), resident_before);

        assert!(matches!(
            arena.append(b"disk blob"),
            Err(StorageError::OffsetOverflow)
        ));
        let inner = arena.inner.read().unwrap();
        assert!(inner.disk_blobs.is_empty());
        assert_eq!(inner.disk_blobs.capacity(), 0);
        assert_eq!(storage.resident_bytes().unwrap(), resident_before);
    }

    #[test]
    fn ledger_charges_fixed_budget_and_spill_file_allocations() {
        let storage = StructuralStorage::with_config(disk_only_config(25)).unwrap();
        let expected_storage_control = size_of::<StructuralStorageInner>()
            + size_of::<RwLock<BlobArenaInner>>()
            + size_of::<Mutex<ResidentBudgetState>>()
            + 6 * size_of::<usize>();
        assert_eq!(
            storage.resident_bytes().unwrap().directory,
            expected_storage_control
        );

        let table = storage.table::<TestRecord>().unwrap();
        let before_spill = storage.resident_bytes().unwrap().directory;
        table.append(&TestRecord { ordinal: 1, tag: 1 }).unwrap();
        let after_spill = storage.resident_bytes().unwrap().directory;
        let expected_spill_control =
            size_of::<DiskExtent>() + size_of::<File>() + 2 * size_of::<usize>();
        assert_eq!(after_spill - before_spill, expected_spill_control);
    }

    #[test]
    fn spill_descriptor_has_owner_only_permissions() {
        let file = secure_tempfile().unwrap();
        let metadata = file.metadata().unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(metadata.nlink(), 0);
    }

    #[test]
    fn invalid_config_is_rejected() {
        assert!(StorageConfig::new(0, 0, 1, 1).is_err());
        assert!(StorageConfig::new(8, 9, 1, 1).is_err());
        assert!(StorageConfig::new(8, 1, 0, 1).is_err());
        assert!(StorageConfig::new(8, 1, 1, 0).is_err());
    }
}
