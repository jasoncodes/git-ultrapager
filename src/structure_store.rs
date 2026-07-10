//! Git の commit / file / hunk metadata を固定長 table と blob arena へ保存する。
//!
//! [`StructureBuilder`] は parser thread だけが所有する single writer である。Parser の入れ子に
//! 合わせ、未確定値は commit / file / hunk ごとに最大1件の [`OpenCommit`]、[`OpenFile`]、
//! [`OpenHunk`] として保持する。親が閉じる時点で fixed record へ変換し、以後は更新しない。
//! File ID と hunk ID の子一覧は出現順が連続するため、resident `Vec` ではなく先頭 ID と件数を
//! [`FileIdRange`] / [`HunkIdRange`] に保存する。
//!
//! [`StructureSnapshot`] は table ごとの high-water mark と open overlay の `Arc` clone だけを持つ。
//! Snapshot 作成後に同じ builder が record を追加しても、reader は high-water mark 以降を読まない。
//! 可変長の object ID、subject、path は [`BlobArena`] に1回だけ保存し、[`StructureReader`] が再利用
//! bufferへ復元する。Disk encoding は Rust の enum discriminant や struct layout に依存しない。

use std::fmt;
use std::num::NonZeroU64;
use std::ops::Range;
use std::str;
use std::sync::Arc;

use thiserror::Error;

use crate::model::{
    ChangeStatus, CommitId, CommitKind, FileId, FileMode, HunkId, HunkKind, LineId, LineSpan,
    SourceRange,
};
use crate::storage::{
    BlobArena, BlobLocation, BlobRef, FixedRecord, RecordCodecError, SpillTable, StorageError,
    StructuralStorage,
};

const OPTIONAL_U64_NONE: u64 = u64::MAX;
const PACKED_BLOB_BYTES: usize = 24;
const COMMIT_RECORD_BYTES: usize = 96;
const FILE_RECORD_BYTES: usize = 144;
const HUNK_RECORD_BYTES: usize = 80;

const BLOB_NONE: u8 = 0;
const BLOB_EMPTY: u8 = 1;
const BLOB_MEMORY: u8 = 2;
const BLOB_DISK: u8 = 3;

/// Structural metadata layer の failure。
#[derive(Debug, Error)]
pub enum StructureStoreError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("structural metadata builder is poisoned after a failed append")]
    Poisoned,
    #[error("cannot {operation} while {open_kind} {open_id} is still open")]
    OpenChild {
        operation: &'static str,
        open_kind: &'static str,
        open_id: u64,
    },
    #[error("cannot {operation} because no {required_kind} is open")]
    MissingOpenParent {
        operation: &'static str,
        required_kind: &'static str,
    },
    #[error(
        "cannot {operation}: expected open {kind} {expected_id}, but caller referenced {actual_id}"
    )]
    OpenParentMismatch {
        operation: &'static str,
        kind: &'static str,
        expected_id: u64,
        actual_id: u64,
    },
    #[error("{kind} stable ID {id} is reserved as an on-disk sentinel")]
    ReservedId { kind: &'static str, id: u64 },
    #[error("{kind} ID range starting at {first} with count {count} overflows u64")]
    IdRangeOverflow {
        kind: &'static str,
        first: u64,
        count: u64,
    },
    #[error("closed line span [{start}, {end}) must have its end after its start")]
    InvalidLineSpan { start: u64, end: u64 },
    #[error(
        "{child_kind} {child_id} starts at {child_start}, before parent {parent_kind} {parent_id} starts at {parent_start}"
    )]
    ChildStartsBeforeParent {
        child_kind: &'static str,
        child_id: u64,
        child_start: u64,
        parent_kind: &'static str,
        parent_id: u64,
        parent_start: u64,
    },
    #[error(
        "{kind} {id} starts at {start}, before preceding {kind} {previous_id} ends at {previous_end}"
    )]
    SiblingSpanOverlap {
        kind: &'static str,
        id: u64,
        start: u64,
        previous_id: u64,
        previous_end: u64,
    },
    #[error(
        "parent {parent_kind} {parent_id} ends at {parent_end}, before child {child_kind} {child_id} ends at {child_end}"
    )]
    ChildEndsAfterParent {
        parent_kind: &'static str,
        parent_id: u64,
        parent_end: u64,
        child_kind: &'static str,
        child_id: u64,
        child_end: u64,
    },
    #[error("{kind} {id} is still open and has no exclusive line end")]
    UnclosedSpan { kind: &'static str, id: u64 },
    #[error("{field} value {value} is reserved as an on-disk optional sentinel")]
    ReservedOptionalValue { field: &'static str, value: u64 },
    #[error("file {file_id} mode {mode:?} has incompatible addition/deletion counts")]
    FileCountModeMismatch { file_id: u64, mode: FileMode },
    #[error("hunk {hunk_id} kind {kind:?} has incompatible source ranges")]
    HunkRangeKindMismatch { hunk_id: u64, kind: HunkKind },
    #[error("{kind} table expected next stable ID {expected}, but append returned {actual}")]
    AppendIdentityMismatch {
        kind: &'static str,
        expected: u64,
        actual: u64,
    },
    #[error("{kind} stable ID {id} is outside snapshot high-water mark {high_water}")]
    OutsideSnapshot {
        kind: &'static str,
        id: u64,
        high_water: u64,
    },
    #[error("{kind} slot {expected} decoded stable ID {actual}")]
    IdentityMismatch {
        kind: &'static str,
        expected: u64,
        actual: u64,
    },
    #[error(
        "{child_kind} {child_id} references {parent_kind} {parent_id}, outside high-water mark {parent_count}"
    )]
    InvalidParent {
        child_kind: &'static str,
        child_id: u64,
        parent_kind: &'static str,
        parent_id: u64,
        parent_count: u64,
    },
    #[error("{parent_kind} {parent_id} child range does not contain {child_kind} {child_id}")]
    ParentRangeMismatch {
        parent_kind: &'static str,
        parent_id: u64,
        child_kind: &'static str,
        child_id: u64,
    },
    #[error("{kind} {id} child range ends at {end}, beyond snapshot high-water mark {high_water}")]
    ChildRangeOutsideSnapshot {
        kind: &'static str,
        id: u64,
        end: u64,
        high_water: u64,
    },
    #[error("{field} blob is not valid UTF-8: {source}")]
    InvalidUtf8 {
        field: &'static str,
        #[source]
        source: str::Utf8Error,
    },
}

/// Commit が所有する連続 File ID 範囲。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIdRange {
    start: u64,
    count: u64,
}

impl FileIdRange {
    /// 次のfile IDをcursorとして保持する空範囲を返す。
    ///
    /// `start`は親を開始した時点のfile high-water markである。子が0件でもcursorを失わないため、
    /// no-file commitを後続commitのfile範囲と区別できる。
    #[must_use]
    pub const fn empty_at(start: u64) -> Self {
        Self { start, count: 0 }
    }

    /// 範囲の挿入cursor。空範囲でも親開始時のchild high-water markを返す。
    #[must_use]
    pub const fn start(self) -> u64 {
        self.start
    }

    /// 範囲の先頭 ID。空範囲では `None`。
    #[must_use]
    pub const fn first(self) -> Option<FileId> {
        if self.count == 0 {
            None
        } else {
            Some(FileId::new(self.start))
        }
    }

    /// 範囲内の file 数。
    #[must_use]
    pub const fn count(self) -> u64 {
        self.count
    }

    /// Exclusive end。範囲 codec を通過した値では overflow しない。
    #[must_use]
    pub const fn end_exclusive(self) -> u64 {
        self.start.saturating_add(self.count)
    }

    /// ID が範囲内かを返す。
    #[must_use]
    pub fn contains(self, id: FileId) -> bool {
        id.get() >= self.start && id.get() < self.end_exclusive()
    }

    /// 範囲末尾のID。空範囲では`None`。
    #[must_use]
    pub fn last(self) -> Option<FileId> {
        let offset = self.count.checked_sub(1)?;
        self.start.checked_add(offset).map(FileId::new)
    }

    /// 0始まりoffsetのID。範囲外では`None`。
    #[must_use]
    pub fn get(self, offset: u64) -> Option<FileId> {
        (offset < self.count)
            .then_some(offset)
            .and_then(|offset| self.start.checked_add(offset))
            .map(FileId::new)
    }

    /// Stable ID順のiterator。
    pub fn iter(self) -> impl DoubleEndedIterator<Item = FileId> {
        (self.start..self.end_exclusive()).map(FileId::new)
    }

    fn push(&mut self, id: FileId) -> Result<(), StructureStoreError> {
        validate_id("file", id.get())?;
        let expected = checked_range_end("file", self.start, self.count)?;
        if id.get() != expected {
            return Err(StructureStoreError::AppendIdentityMismatch {
                kind: "file child",
                expected,
                actual: id.get(),
            });
        }
        self.count = self
            .count
            .checked_add(1)
            .ok_or(StructureStoreError::IdRangeOverflow {
                kind: "file",
                first: self.start,
                count: self.count,
            })?;
        validate_file_range(*self)
    }
}

/// File が所有する連続 Hunk ID 範囲。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HunkIdRange {
    start: u64,
    count: u64,
}

impl HunkIdRange {
    /// 次のhunk IDをcursorとして保持する空範囲を返す。
    #[must_use]
    pub const fn empty_at(start: u64) -> Self {
        Self { start, count: 0 }
    }

    /// 範囲の挿入cursor。空範囲でも親開始時のchild high-water markを返す。
    #[must_use]
    pub const fn start(self) -> u64 {
        self.start
    }

    /// 範囲の先頭 ID。空範囲では `None`。
    #[must_use]
    pub const fn first(self) -> Option<HunkId> {
        if self.count == 0 {
            None
        } else {
            Some(HunkId::new(self.start))
        }
    }

    /// 範囲内の hunk 数。
    #[must_use]
    pub const fn count(self) -> u64 {
        self.count
    }

    /// Exclusive end。範囲 codec を通過した値では overflow しない。
    #[must_use]
    pub const fn end_exclusive(self) -> u64 {
        self.start.saturating_add(self.count)
    }

    /// ID が範囲内かを返す。
    #[must_use]
    pub fn contains(self, id: HunkId) -> bool {
        id.get() >= self.start && id.get() < self.end_exclusive()
    }

    /// 範囲末尾のID。空範囲では`None`。
    #[must_use]
    pub fn last(self) -> Option<HunkId> {
        let offset = self.count.checked_sub(1)?;
        self.start.checked_add(offset).map(HunkId::new)
    }

    /// 0始まりoffsetのID。範囲外では`None`。
    #[must_use]
    pub fn get(self, offset: u64) -> Option<HunkId> {
        (offset < self.count)
            .then_some(offset)
            .and_then(|offset| self.start.checked_add(offset))
            .map(HunkId::new)
    }

    /// Stable ID順のiterator。
    pub fn iter(self) -> impl DoubleEndedIterator<Item = HunkId> {
        (self.start..self.end_exclusive()).map(HunkId::new)
    }

    fn push(&mut self, id: HunkId) -> Result<(), StructureStoreError> {
        validate_id("hunk", id.get())?;
        let expected = checked_range_end("hunk", self.start, self.count)?;
        if id.get() != expected {
            return Err(StructureStoreError::AppendIdentityMismatch {
                kind: "hunk child",
                expected,
                actual: id.get(),
            });
        }
        self.count = self
            .count
            .checked_add(1)
            .ok_or(StructureStoreError::IdRangeOverflow {
                kind: "hunk",
                first: self.start,
                count: self.count,
            })?;
        validate_hunk_range(*self)
    }
}

/// File 内の旧側・新側 gutter に現れた最大 source line 番号。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StructureFileLineBounds {
    /// 旧側番号を持つlineがまだなければ`None`。File close後はfixed file recordへ移る。
    pub max_old_line_number: Option<u64>,
    /// 新側番号を持つlineがまだなければ`None`。File close後はfixed file recordへ移る。
    pub max_new_line_number: Option<u64>,
}

impl StructureFileLineBounds {
    /// Source line metadata を1件取り込み、各側の最大値だけを保持する。
    pub fn observe(&mut self, old: Option<u64>, new: Option<u64>) {
        if let Some(number) = old {
            self.max_old_line_number = Some(
                self.max_old_line_number
                    .map_or(number, |current| current.max(number)),
            );
        }
        if let Some(number) = new {
            self.max_new_line_number = Some(
                self.max_new_line_number
                    .map_or(number, |current| current.max(number)),
            );
        }
    }
}

/// Fixed tableへ未確定のまま書かず、snapshotだけへcloneするopen commit。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenCommit {
    /// Finalized commit tableの次のindexと一致するstable ID。
    id: CommitId,
    /// Commit headerの有無からparserが確定したgroup種別。
    pub kind: CommitKind,
    /// Headerから読めたobject ID。Commit close時にblob arenaへ移る。
    pub object_id: Option<String>,
    /// 最初の非空commit message。到着前または存在しない時は`None`。
    pub subject: Option<String>,
    /// Startは開始時に固定し、open中のexclusive endは`None`。
    span: LineSpan,
    /// このcommit配下で開始済みのfileを表す連続範囲。Open fileも件数に含む。
    files: FileIdRange,
}

impl OpenCommit {
    /// Finalized commit tableの次のindexと一致するstable ID。
    #[must_use]
    pub const fn id(&self) -> CommitId {
        self.id
    }

    /// Startだけが確定したopen line span。
    #[must_use]
    pub const fn span(&self) -> LineSpan {
        self.span
    }

    /// このcommit配下で開始済みのfileを表す連続範囲。
    #[must_use]
    pub const fn files(&self) -> FileIdRange {
        self.files
    }
}

/// Fixed tableへ未確定のまま書かず、snapshotだけへcloneするopen file。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenFile {
    /// Finalized file tableの次のindexに一致するstable ID。
    id: FileId,
    /// Parser lifecycle中にopenな[`OpenCommit`]のID。
    commit_id: CommitId,
    /// 旧側path。File close時にblob arenaへ移る。
    pub old_path: Option<String>,
    /// 新側path。File close時にblob arenaへ移る。
    pub new_path: Option<String>,
    /// Headerとpayloadから現時点で確定した変更種別。
    pub status: ChangeStatus,
    /// Rich parseの可否。後続lineでfallbackへdowngradeできる。
    pub mode: FileMode,
    /// Startは開始時に固定し、open中のexclusive endは`None`。
    span: LineSpan,
    /// このfile配下で開始済みのhunkを表す連続範囲。Open hunkも件数に含む。
    hunks: HunkIdRange,
    /// Rich modeの追加数。Fallbackへ移った時は`None`へ変更する。
    pub additions: Option<u64>,
    /// Rich modeの削除数。Fallbackへ移った時は`None`へ変更する。
    pub deletions: Option<u64>,
    /// Source line appendに伴って増加し、file close時にfile recordへ移る。
    pub line_bounds: StructureFileLineBounds,
}

impl OpenFile {
    /// Finalized file tableの次のindexに一致するstable ID。
    #[must_use]
    pub const fn id(&self) -> FileId {
        self.id
    }

    /// Parser lifecycle中にopenな親commitのID。
    #[must_use]
    pub const fn commit_id(&self) -> CommitId {
        self.commit_id
    }

    /// Startだけが確定したopen line span。
    #[must_use]
    pub const fn span(&self) -> LineSpan {
        self.span
    }

    /// このfile配下で開始済みのhunkを表す連続範囲。
    #[must_use]
    pub const fn hunks(&self) -> HunkIdRange {
        self.hunks
    }

    /// Syntax 判定と通常 sticky header に使う path を返す。
    #[must_use]
    pub fn primary_path(&self) -> Option<&str> {
        self.new_path.as_deref().or(self.old_path.as_deref())
    }
}

/// Fixed tableへ未確定のまま書かず、snapshotだけへcloneするopen hunk。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenHunk {
    /// Finalized hunk tableの次のindexと一致するstable ID。
    id: HunkId,
    /// Parser lifecycle中にopenな[`OpenFile`]のID。
    file_id: FileId,
    /// Header grammarと後続lineから現時点で確定したhunk種別。
    pub kind: HunkKind,
    /// Hunk header自身のlogical-line ID。
    header_line: LineId,
    /// Startは開始時に固定し、open中のexclusive endは`None`。
    span: LineSpan,
    /// Unified old-side range。それ以外のkindでは`None`。
    pub old_range: Option<SourceRange>,
    /// Unified new-side range。それ以外のkindでは`None`。
    pub new_range: Option<SourceRange>,
}

impl OpenHunk {
    /// Finalized hunk tableの次のindexと一致するstable ID。
    #[must_use]
    pub const fn id(&self) -> HunkId {
        self.id
    }

    /// Parser lifecycle中にopenな親fileのID。
    #[must_use]
    pub const fn file_id(&self) -> FileId {
        self.file_id
    }

    /// Hunk header自身のlogical-line ID。
    #[must_use]
    pub const fn header_line(&self) -> LineId {
        self.header_line
    }

    /// Startだけが確定したopen line span。
    #[must_use]
    pub const fn span(&self) -> LineSpan {
        self.span
    }
}

/// Finalized record と open overlay を同じ形で読める commit view。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoredCommitRef<'a> {
    /// Snapshot内で不変のstable ID。
    pub id: CommitId,
    /// Commit group種別。
    pub kind: CommitKind,
    /// Reader bufferまたはopen overlayを借用するobject ID。
    pub object_id: Option<&'a str>,
    /// Reader bufferまたはopen overlayを借用するsubject。
    pub subject: Option<&'a str>,
    /// Open viewだけexclusive endが`None`。
    pub span: LineSpan,
    /// Snapshot世代でこのcommitが所有するfile範囲。
    pub files: FileIdRange,
    /// Fixed tableからdecodeしたviewなら`true`、overlayなら`false`。
    pub finalized: bool,
}

/// Blob stringを復元せずに読めるcommit metadata。
///
/// Logical-line metadataの参照検証、structure jump、件数表示はobject IDやsubjectを必要としない。
/// それらのhot pathがblob arenaのI/Oと可変長buffer確保を発生させないよう、文字列を持つ
/// [`StoredCommitRef`]とは別のviewとして返す。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoredCommitMetadata {
    /// Snapshot内で不変のstable ID。
    pub id: CommitId,
    /// Commit group種別。
    pub kind: CommitKind,
    /// Open viewだけexclusive endが`None`。
    pub span: LineSpan,
    /// Snapshot世代でこのcommitが所有するfile範囲。
    pub files: FileIdRange,
    /// Fixed tableからdecodeしたviewなら`true`、overlayなら`false`。
    pub finalized: bool,
}

impl<'a> StoredCommitRef<'a> {
    /// 可変長文字列の借用を除いたmetadata viewへ変換する。
    #[must_use]
    pub const fn metadata(self) -> StoredCommitMetadata {
        StoredCommitMetadata {
            id: self.id,
            kind: self.kind,
            span: self.span,
            files: self.files,
            finalized: self.finalized,
        }
    }
}

/// Finalized record と open overlay を同じ形で読める file view。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoredFileRef<'a> {
    /// Snapshot内で不変のstable ID。
    pub id: FileId,
    /// Snapshot内の親commit ID。
    pub commit_id: CommitId,
    /// Reader bufferまたはopen overlayを借用する旧側path。
    pub old_path: Option<&'a str>,
    /// Reader bufferまたはopen overlayを借用する新側path。
    pub new_path: Option<&'a str>,
    /// Snapshot世代で確定した変更種別。
    pub status: ChangeStatus,
    /// Snapshot世代で確定したparse mode。
    pub mode: FileMode,
    /// Open viewだけexclusive endが`None`。
    pub span: LineSpan,
    /// Snapshot世代でこのfileが所有するhunk範囲。
    pub hunks: HunkIdRange,
    /// Rich modeの追加数。Fallback modeでは`None`。
    pub additions: Option<u64>,
    /// Rich modeの削除数。Fallback modeでは`None`。
    pub deletions: Option<u64>,
    /// 旧側・新側gutterに現れた最大source line番号。
    pub line_bounds: StructureFileLineBounds,
    /// Fixed tableからdecodeしたviewなら`true`、overlayなら`false`。
    pub finalized: bool,
}

impl<'a> StoredFileRef<'a> {
    /// Syntax 判定と通常 sticky header に使う path を返す。
    #[must_use]
    pub fn primary_path(self) -> Option<&'a str> {
        self.new_path.or(self.old_path)
    }

    /// Path借用を除いたmetadata viewへ変換する。
    #[must_use]
    pub const fn metadata(self) -> StoredFileMetadata {
        StoredFileMetadata {
            id: self.id,
            commit_id: self.commit_id,
            status: self.status,
            mode: self.mode,
            span: self.span,
            hunks: self.hunks,
            additions: self.additions,
            deletions: self.deletions,
            line_bounds: self.line_bounds,
            finalized: self.finalized,
        }
    }
}

/// Path blobを復元せずに読めるfile metadata。
///
/// Parent relation、mode、span、child range、行数だけを使う処理向けであり、old/new pathの有無や
/// 内容は契約に含めない。Pathを表示またはfilterするcallerは[`StructureReader::read_file`]を使う。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoredFileMetadata {
    /// Snapshot内で不変のstable ID。
    pub id: FileId,
    /// Snapshot内の親commit ID。
    pub commit_id: CommitId,
    /// Snapshot世代で確定した変更種別。
    pub status: ChangeStatus,
    /// Snapshot世代で確定したparse mode。
    pub mode: FileMode,
    /// Open viewだけexclusive endが`None`。
    pub span: LineSpan,
    /// Snapshot世代でこのfileが所有するhunk範囲。
    pub hunks: HunkIdRange,
    /// Rich modeの追加数。Fallback modeでは`None`。
    pub additions: Option<u64>,
    /// Rich modeの削除数。Fallback modeでは`None`。
    pub deletions: Option<u64>,
    /// 旧側・新側gutterに現れた最大source line番号。
    pub line_bounds: StructureFileLineBounds,
    /// Fixed tableからdecodeしたviewなら`true`、overlayなら`false`。
    pub finalized: bool,
}

/// Finalized record と open overlay を同じ形で読める hunk view。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoredHunk {
    /// Snapshot内で不変のstable ID。
    pub id: HunkId,
    /// Snapshot内の親file ID。
    pub file_id: FileId,
    /// Snapshot世代で確定したhunk種別。
    pub kind: HunkKind,
    /// Hunk header自身のlogical-line ID。
    pub header_line: LineId,
    /// Open viewだけexclusive endが`None`。
    pub span: LineSpan,
    /// Unified old-side range。それ以外のkindでは`None`。
    pub old_range: Option<SourceRange>,
    /// Unified new-side range。それ以外のkindでは`None`。
    pub new_range: Option<SourceRange>,
    /// Fixed tableからdecodeしたviewなら`true`、overlayなら`false`。
    pub finalized: bool,
}

/// Tableごとに1件以上がdisk tierへ移ったかを表すdiagnostic。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StructureSpillStatus {
    /// Commit tableにdisk extentが存在する。
    pub commits: bool,
    /// File tableにdisk extentが存在する。
    pub files: bool,
    /// Hunk tableにdisk extentが存在する。
    pub hunks: bool,
}

/// Open overlayを含むsnapshot可視件数。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StructureCounts {
    /// Commit件数。
    pub commits: u64,
    /// File件数。
    pub files: u64,
    /// Hunk件数。
    pub hunks: u64,
}

/// Append成功済みの同種recordの末尾位置。次のsibling開始とparent closeの検証にだけ使う。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FinalizedSpanEnd {
    id: u64,
    end_exclusive: u64,
}

/// Parser threadだけが更新するstructural metadata writer。
pub struct StructureBuilder {
    commits: SpillTable<CommitRecord>,
    files: SpillTable<FileRecord>,
    hunks: SpillTable<HunkRecord>,
    blobs: BlobArena,
    open_commit: Option<Arc<OpenCommit>>,
    open_file: Option<Arc<OpenFile>>,
    open_hunk: Option<Arc<OpenHunk>>,
    last_commit: Option<FinalizedSpanEnd>,
    last_file: Option<FinalizedSpanEnd>,
    last_hunk: Option<FinalizedSpanEnd>,
    poisoned: bool,
}

impl fmt::Debug for StructureBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StructureBuilder")
            .field(
                "open_commit",
                &self.open_commit.as_ref().map(|value| value.id),
            )
            .field("open_file", &self.open_file.as_ref().map(|value| value.id))
            .field("open_hunk", &self.open_hunk.as_ref().map(|value| value.id))
            .field("last_commit", &self.last_commit)
            .field("last_file", &self.last_file)
            .field("last_hunk", &self.last_hunk)
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}

impl StructureBuilder {
    /// Shared 256 MiB ledgerから3つのtableと1つのblob arenaを作る。
    pub fn new(storage: &StructuralStorage) -> Result<Self, StructureStoreError> {
        Ok(Self {
            commits: storage.table()?,
            files: storage.table()?,
            hunks: storage.table()?,
            blobs: storage.blob_arena(),
            open_commit: None,
            open_file: None,
            open_hunk: None,
            last_commit: None,
            last_file: None,
            last_hunk: None,
            poisoned: false,
        })
    }

    /// 次のcommitを開始する。先行commitの子が残っている場合は状態を変更しない。
    pub fn begin_commit(
        &mut self,
        kind: CommitKind,
        object_id: Option<String>,
        start: LineId,
    ) -> Result<CommitId, StructureStoreError> {
        self.ensure_healthy()?;
        self.validate_current_overlays()?;
        if let Some(hunk) = self.open_hunk.as_ref() {
            return Err(open_child("begin commit", "hunk", hunk.id.get()));
        }
        if let Some(file) = self.open_file.as_ref() {
            return Err(open_child("begin commit", "file", file.id.get()));
        }
        if let Some(commit) = self.open_commit.as_ref() {
            return Err(open_child("begin commit", "commit", commit.id.get()));
        }
        validate_id("line", start.get())?;
        let id = CommitId::new(self.commits.len()?);
        let file_start = self.files.len()?;
        validate_id("commit", id.get())?;
        validate_sibling_start("commit", id.get(), start.get(), self.last_commit)?;
        self.open_commit = Some(Arc::new(OpenCommit {
            id,
            kind,
            object_id,
            subject: None,
            span: LineSpan::open(start),
            files: FileIdRange::empty_at(file_start),
        }));
        Ok(id)
    }

    /// 現在openなcommitのimmutable view。Open commitがなければ`None`。
    #[must_use]
    pub fn open_commit(&self) -> Option<&OpenCommit> {
        self.open_commit.as_deref()
    }

    /// 現在openなcommitをparserが更新するためのmutable view。
    pub fn open_commit_mut(&mut self) -> Option<&mut OpenCommit> {
        self.open_commit.as_mut().map(Arc::make_mut)
    }

    /// 現在のcommit配下で次のfileを開始する。
    #[allow(clippy::too_many_arguments)]
    pub fn begin_file(
        &mut self,
        commit_id: CommitId,
        old_path: Option<String>,
        new_path: Option<String>,
        status: ChangeStatus,
        mode: FileMode,
        start: LineId,
    ) -> Result<FileId, StructureStoreError> {
        self.ensure_healthy()?;
        self.validate_current_overlays()?;
        if let Some(hunk) = self.open_hunk.as_ref() {
            return Err(open_child("begin file", "hunk", hunk.id.get()));
        }
        if let Some(file) = self.open_file.as_ref() {
            return Err(open_child("begin file", "file", file.id.get()));
        }
        let previous_file = self.last_file;
        let commit = self
            .open_commit
            .as_deref()
            .ok_or(StructureStoreError::MissingOpenParent {
                operation: "begin file",
                required_kind: "commit",
            })?;
        if commit.id != commit_id {
            return Err(StructureStoreError::OpenParentMismatch {
                operation: "begin file",
                kind: "commit",
                expected_id: commit.id.get(),
                actual_id: commit_id.get(),
            });
        }
        validate_id("line", start.get())?;
        let id = FileId::new(self.files.len()?);
        let hunk_start = self.hunks.len()?;
        validate_id("file", id.get())?;
        validate_child_start(
            "commit",
            commit.id.get(),
            commit.span.start.get(),
            "file",
            id.get(),
            start.get(),
        )?;
        validate_sibling_start("file", id.get(), start.get(), previous_file)?;
        let mut next_range = commit.files;
        next_range.push(id)?;
        let initial_count = mode.is_rich().then_some(0);
        // 入力検証が終わるまでCOWを発生させない。失敗したbeginが既存snapshotとの共有だけを
        // 解除すると、論理値が同じでも次回snapshotのallocation特性が呼び出し順に依存する。
        let commit = self
            .open_commit
            .as_mut()
            .ok_or(StructureStoreError::MissingOpenParent {
                operation: "begin file",
                required_kind: "commit",
            })?;
        Arc::make_mut(commit).files = next_range;
        self.open_file = Some(Arc::new(OpenFile {
            id,
            commit_id,
            old_path,
            new_path,
            status,
            mode,
            span: LineSpan::open(start),
            hunks: HunkIdRange::empty_at(hunk_start),
            additions: initial_count,
            deletions: initial_count,
            line_bounds: StructureFileLineBounds::default(),
        }));
        Ok(id)
    }

    /// 現在openなfileのimmutable view。Open fileがなければ`None`。
    #[must_use]
    pub fn open_file(&self) -> Option<&OpenFile> {
        self.open_file.as_deref()
    }

    /// 現在openなfileをparserが更新するためのmutable view。
    pub fn open_file_mut(&mut self) -> Option<&mut OpenFile> {
        self.open_file.as_mut().map(Arc::make_mut)
    }

    /// 現在のfile配下で次のhunkを開始する。
    #[allow(clippy::too_many_arguments)]
    pub fn begin_hunk(
        &mut self,
        file_id: FileId,
        kind: HunkKind,
        header_line: LineId,
        old_range: Option<SourceRange>,
        new_range: Option<SourceRange>,
    ) -> Result<HunkId, StructureStoreError> {
        self.ensure_healthy()?;
        self.validate_current_overlays()?;
        if let Some(hunk) = self.open_hunk.as_ref() {
            return Err(open_child("begin hunk", "hunk", hunk.id.get()));
        }
        let previous_hunk = self.last_hunk;
        let file = self
            .open_file
            .as_deref()
            .ok_or(StructureStoreError::MissingOpenParent {
                operation: "begin hunk",
                required_kind: "file",
            })?;
        if file.id != file_id {
            return Err(StructureStoreError::OpenParentMismatch {
                operation: "begin hunk",
                kind: "file",
                expected_id: file.id.get(),
                actual_id: file_id.get(),
            });
        }
        validate_id("line", header_line.get())?;
        let id = HunkId::new(self.hunks.len()?);
        validate_id("hunk", id.get())?;
        validate_hunk_ranges(id.get(), kind, old_range, new_range)?;
        validate_child_start(
            "file",
            file.id.get(),
            file.span.start.get(),
            "hunk",
            id.get(),
            header_line.get(),
        )?;
        validate_sibling_start("hunk", id.get(), header_line.get(), previous_hunk)?;
        let mut next_range = file.hunks;
        next_range.push(id)?;
        let file = self
            .open_file
            .as_mut()
            .ok_or(StructureStoreError::MissingOpenParent {
                operation: "begin hunk",
                required_kind: "file",
            })?;
        Arc::make_mut(file).hunks = next_range;
        self.open_hunk = Some(Arc::new(OpenHunk {
            id,
            file_id,
            kind,
            header_line,
            span: LineSpan::open(header_line),
            old_range,
            new_range,
        }));
        Ok(id)
    }

    /// 現在openなhunkのimmutable view。Open hunkがなければ`None`。
    #[must_use]
    pub fn open_hunk(&self) -> Option<&OpenHunk> {
        self.open_hunk.as_deref()
    }

    /// 現在openなhunkをparserが更新するためのmutable view。
    pub fn open_hunk_mut(&mut self) -> Option<&mut OpenHunk> {
        self.open_hunk.as_mut().map(Arc::make_mut)
    }

    /// Open hunkを閉じ、fixed tableへ1 recordとしてappendする。
    pub fn finish_hunk(&mut self, end_exclusive: u64) -> Result<HunkId, StructureStoreError> {
        self.ensure_healthy()?;
        self.validate_current_overlays()?;
        let hunk = self
            .open_hunk
            .clone()
            .ok_or(StructureStoreError::MissingOpenParent {
                operation: "finish hunk",
                required_kind: "hunk",
            })?;
        let record = HunkRecord::from_open(&hunk, end_exclusive)?;
        let expected = hunk.id.get();
        let result = self
            .append_or_poison_hunk(&record)
            .and_then(|actual| validate_append_identity("hunk", expected, actual));
        if result.is_err() {
            self.poisoned = true;
        }
        result?;
        self.last_hunk = Some(FinalizedSpanEnd {
            id: hunk.id.get(),
            end_exclusive,
        });
        self.open_hunk = None;
        Ok(hunk.id)
    }

    /// Open fileを閉じ、line boundsを含むfixed recordを1件appendする。
    pub fn finish_file(&mut self, end_exclusive: u64) -> Result<FileId, StructureStoreError> {
        self.ensure_healthy()?;
        self.validate_current_overlays()?;
        if let Some(hunk) = self.open_hunk.as_ref() {
            return Err(open_child("finish file", "hunk", hunk.id.get()));
        }
        let file = self
            .open_file
            .clone()
            .ok_or(StructureStoreError::MissingOpenParent {
                operation: "finish file",
                required_kind: "file",
            })?;
        validate_last_child_end(
            "file",
            file.id.get(),
            end_exclusive,
            "hunk",
            file.hunks.last().map(HunkId::get),
            self.last_hunk,
        )?;
        // Mutable overlayの値をblobへ書く前に検証し、callerが修正できる入力errorではbuilderを
        // poisonしない。ここから先のfailureはblob/tableのappend後かもしれないため再試行できない。
        let record = FileRecord::from_open(
            &file,
            end_exclusive,
            PackedBlobRef::NONE,
            PackedBlobRef::NONE,
        )?;
        let result = self.append_finalized_file(&file, record);
        if result.is_err() {
            self.poisoned = true;
        }
        result?;
        self.last_file = Some(FinalizedSpanEnd {
            id: file.id.get(),
            end_exclusive,
        });
        self.open_file = None;
        Ok(file.id)
    }

    /// Open commitを閉じ、object IDとsubjectをblob arenaへ移してfixed tableへappendする。
    pub fn finish_commit(&mut self, end_exclusive: u64) -> Result<CommitId, StructureStoreError> {
        self.ensure_healthy()?;
        self.validate_current_overlays()?;
        if let Some(hunk) = self.open_hunk.as_ref() {
            return Err(open_child("finish commit", "hunk", hunk.id.get()));
        }
        if let Some(file) = self.open_file.as_ref() {
            return Err(open_child("finish commit", "file", file.id.get()));
        }
        let commit = self
            .open_commit
            .clone()
            .ok_or(StructureStoreError::MissingOpenParent {
                operation: "finish commit",
                required_kind: "commit",
            })?;
        validate_last_child_end(
            "commit",
            commit.id.get(),
            end_exclusive,
            "file",
            commit.files.last().map(FileId::get),
            self.last_file,
        )?;
        // SpanとID rangeを先に検証し、invalid overlayでblob arenaを進めない。
        let record = CommitRecord::from_open(
            &commit,
            end_exclusive,
            PackedBlobRef::NONE,
            PackedBlobRef::NONE,
        )?;
        let result = self.append_finalized_commit(&commit, record);
        if result.is_err() {
            self.poisoned = true;
        }
        result?;
        self.last_commit = Some(FinalizedSpanEnd {
            id: commit.id.get(),
            end_exclusive,
        });
        self.open_commit = None;
        Ok(commit.id)
    }

    /// Table high-water markと最大3件のopen overlayを固定する。
    pub fn snapshot(&self) -> Result<StructureSnapshot, StructureStoreError> {
        self.ensure_healthy()?;
        let commit_high_water = self.commits.len()?;
        let file_high_water = self.files.len()?;
        let hunk_high_water = self.hunks.len()?;
        validate_open_overlay(
            commit_high_water,
            file_high_water,
            hunk_high_water,
            self.open_commit.as_deref(),
            self.open_file.as_deref(),
            self.open_hunk.as_deref(),
        )?;
        Ok(StructureSnapshot {
            commits: self.commits.clone(),
            files: self.files.clone(),
            hunks: self.hunks.clone(),
            blobs: self.blobs.clone(),
            commit_high_water,
            file_high_water,
            hunk_high_water,
            open_commit: self.open_commit.clone(),
            open_file: self.open_file.clone(),
            open_hunk: self.open_hunk.clone(),
        })
    }

    /// Tableのspill状態を返す。Blob arenaはlocationがrecordごとに異なるためreader viewで判別する。
    pub fn spill_status(&self) -> Result<StructureSpillStatus, StructureStoreError> {
        Ok(StructureSpillStatus {
            commits: self.commits.is_spilled()?,
            files: self.files.is_spilled()?,
            hunks: self.hunks.is_spilled()?,
        })
    }

    /// Fixed tableとopen overlayを合算した現在の可視件数を返す。
    pub fn visible_counts(&self) -> Result<StructureCounts, StructureStoreError> {
        self.ensure_healthy()?;
        Ok(StructureCounts {
            commits: visible_count(self.commits.len()?, self.open_commit.is_some(), "commit")?,
            files: visible_count(self.files.len()?, self.open_file.is_some(), "file")?,
            hunks: visible_count(self.hunks.len()?, self.open_hunk.is_some(), "hunk")?,
        })
    }

    fn ensure_healthy(&self) -> Result<(), StructureStoreError> {
        if self.poisoned {
            Err(StructureStoreError::Poisoned)
        } else {
            Ok(())
        }
    }

    fn validate_current_overlays(&self) -> Result<(), StructureStoreError> {
        let commit_high_water = self.commits.len()?;
        let file_high_water = self.files.len()?;
        let hunk_high_water = self.hunks.len()?;
        validate_open_overlay(
            commit_high_water,
            file_high_water,
            hunk_high_water,
            self.open_commit.as_deref(),
            self.open_file.as_deref(),
            self.open_hunk.as_deref(),
        )
    }

    fn append_or_poison_hunk(&mut self, record: &HunkRecord) -> Result<u64, StructureStoreError> {
        match self.hunks.append(record) {
            Ok(id) => Ok(id),
            Err(error) => {
                self.poisoned = true;
                Err(error.into())
            }
        }
    }

    fn append_finalized_file(
        &mut self,
        file: &OpenFile,
        mut record: FileRecord,
    ) -> Result<(), StructureStoreError> {
        record.old_path = append_optional_blob(&self.blobs, file.old_path.as_deref())?;
        record.new_path = append_optional_blob(&self.blobs, file.new_path.as_deref())?;
        let file_id = self.files.append(&record)?;
        validate_append_identity("file", file.id.get(), file_id)
    }

    fn append_finalized_commit(
        &mut self,
        commit: &OpenCommit,
        mut record: CommitRecord,
    ) -> Result<(), StructureStoreError> {
        record.object_id = append_optional_blob(&self.blobs, commit.object_id.as_deref())?;
        record.subject = append_optional_blob(&self.blobs, commit.subject.as_deref())?;
        let id = self.commits.append(&record)?;
        validate_append_identity("commit", commit.id.get(), id)
    }
}

/// Structural tableのimmutable high-water markとopen overlay。
#[derive(Clone)]
pub struct StructureSnapshot {
    commits: SpillTable<CommitRecord>,
    files: SpillTable<FileRecord>,
    hunks: SpillTable<HunkRecord>,
    blobs: BlobArena,
    commit_high_water: u64,
    file_high_water: u64,
    hunk_high_water: u64,
    open_commit: Option<Arc<OpenCommit>>,
    open_file: Option<Arc<OpenFile>>,
    open_hunk: Option<Arc<OpenHunk>>,
}

impl StructureSnapshot {
    /// Finalized commit tableのsnapshot時点のrecord数。
    #[must_use]
    pub const fn finalized_commit_count(&self) -> u64 {
        self.commit_high_water
    }

    /// Open overlayを含むcommit数。
    #[must_use]
    pub const fn commit_count(&self) -> u64 {
        self.commit_high_water + self.open_commit.is_some() as u64
    }

    /// Finalized file tableのsnapshot時点のrecord数。
    #[must_use]
    pub const fn finalized_file_count(&self) -> u64 {
        self.file_high_water
    }

    /// Open overlayを含むfile数。
    #[must_use]
    pub const fn file_count(&self) -> u64 {
        self.file_high_water + self.open_file.is_some() as u64
    }

    /// Finalized hunk tableのsnapshot時点のrecord数。
    #[must_use]
    pub const fn finalized_hunk_count(&self) -> u64 {
        self.hunk_high_water
    }

    /// Open overlayを含むhunk数。
    #[must_use]
    pub const fn hunk_count(&self) -> u64 {
        self.hunk_high_water + self.open_hunk.is_some() as u64
    }

    /// Open overlayを含む可視件数をoverflow検査付きで返す。
    pub fn visible_counts(&self) -> Result<StructureCounts, StructureStoreError> {
        Ok(StructureCounts {
            commits: visible_count(self.commit_high_water, self.open_commit.is_some(), "commit")?,
            files: visible_count(self.file_high_water, self.open_file.is_some(), "file")?,
            hunks: visible_count(self.hunk_high_water, self.open_hunk.is_some(), "hunk")?,
        })
    }

    /// Snapshot内の0始まりordinalをstable commit IDへ写す。
    ///
    /// Finalized tableに続くopen overlayも1件として数える。Ordinalが可視件数以上なら`None`であり、
    /// `u64::MAX`を加算しないため境界入力でもoverflowしない。
    #[must_use]
    pub const fn commit_id_at(&self, ordinal: u64) -> Option<CommitId> {
        if ordinal < self.commit_high_water
            || (ordinal == self.commit_high_water && self.open_commit.is_some())
        {
            Some(CommitId::new(ordinal))
        } else {
            None
        }
    }

    /// Snapshot内の0始まりordinalをstable file IDへ写す。Open overlayも可視件数に含む。
    #[must_use]
    pub const fn file_id_at(&self, ordinal: u64) -> Option<FileId> {
        if ordinal < self.file_high_water
            || (ordinal == self.file_high_water && self.open_file.is_some())
        {
            Some(FileId::new(ordinal))
        } else {
            None
        }
    }

    /// Snapshot内の0始まりordinalをstable hunk IDへ写す。Open overlayも可視件数に含む。
    #[must_use]
    pub const fn hunk_id_at(&self, ordinal: u64) -> Option<HunkId> {
        if ordinal < self.hunk_high_water
            || (ordinal == self.hunk_high_water && self.open_hunk.is_some())
        {
            Some(HunkId::new(ordinal))
        } else {
            None
        }
    }

    /// Snapshot取得時にopenだったcommit。Table high-water markと同じIDを持つ。
    #[must_use]
    pub fn open_commit(&self) -> Option<&OpenCommit> {
        self.open_commit.as_deref()
    }

    /// Snapshot取得時にopenだったfile。Table high-water markと同じIDを持つ。
    #[must_use]
    pub fn open_file(&self) -> Option<&OpenFile> {
        self.open_file.as_deref()
    }

    /// Snapshot取得時にopenだったhunk。Table high-water markと同じIDを持つ。
    #[must_use]
    pub fn open_hunk(&self) -> Option<&OpenHunk> {
        self.open_hunk.as_deref()
    }

    /// File grammarを1件も検出していないsnapshotかを返す。
    #[must_use]
    pub const fn is_plain(&self) -> bool {
        self.file_count() == 0
    }
}

impl fmt::Debug for StructureSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StructureSnapshot")
            .field("commit_count", &self.commit_count())
            .field("file_count", &self.file_count())
            .field("hunk_count", &self.hunk_count())
            .field(
                "open_commit",
                &self.open_commit.as_ref().map(|value| value.id),
            )
            .field("open_file", &self.open_file.as_ref().map(|value| value.id))
            .field("open_hunk", &self.open_hunk.as_ref().map(|value| value.id))
            .finish()
    }
}

/// Metadata readerがdecodeしたfixed record件数。
///
/// Seekの計算量と親metadata再利用をtestから観測するdiagnosticであり、I/O byte数ではない。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StructureReadStats {
    /// Finalized commit recordをdecodeした回数。Open overlay参照は含めない。
    pub commit_records: u64,
    /// Finalized file recordをdecodeした回数。Open overlay参照は含めない。
    pub file_records: u64,
    /// Finalized hunk recordをdecodeした回数。Open overlay参照は含めない。
    pub hunk_records: u64,
}

/// Fixed recordだけをcaller-owned bufferへdecodeする再利用可能reader。
///
/// Commit subject、object ID、file pathが不要なline参照検証とnavigation向けのreaderであり、
/// [`BlobArena`]には触れない。Spill tierでもbufferは各recordの固定encoded幅までしか増えず、同じ
/// readerを反復利用すると追加allocationを行わない。Open overlayはtable上の次IDとして同じviewへ
/// 写し、finalized recordと同じparent/child-range検証を適用する。
#[derive(Debug, Default)]
pub struct StructureMetadataReader {
    commit_record: Vec<u8>,
    file_record: Vec<u8>,
    hunk_record: Vec<u8>,
    parent_record: Vec<u8>,
    stats: StructureReadStats,
}

impl StructureMetadataReader {
    /// Spill record用bufferをまだ確保していないreaderを作る。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            commit_record: Vec::new(),
            file_record: Vec::new(),
            hunk_record: Vec::new(),
            parent_record: Vec::new(),
            stats: StructureReadStats {
                commit_records: 0,
                file_records: 0,
                hunk_records: 0,
            },
        }
    }

    /// Reader作成後または直近reset後のfixed-record decode回数を返す。
    #[must_use]
    pub const fn statistics(&self) -> StructureReadStats {
        self.stats
    }

    /// Decode回数だけを0へ戻し、再利用bufferは保持する。
    pub fn reset_statistics(&mut self) {
        self.stats = StructureReadStats::default();
    }

    /// Stable commit IDをblob I/Oなしで読む。Snapshot外のIDは`Ok(None)`となる。
    pub fn read_commit(
        &mut self,
        snapshot: &StructureSnapshot,
        id: CommitId,
    ) -> Result<Option<StoredCommitMetadata>, StructureStoreError> {
        if id.get() == snapshot.commit_high_water {
            return Ok(snapshot
                .open_commit
                .as_ref()
                .filter(|value| value.id == id)
                .map(|commit| StoredCommitMetadata {
                    id: commit.id,
                    kind: commit.kind,
                    span: commit.span,
                    files: commit.files,
                    finalized: false,
                }));
        }
        if id.get() > snapshot.commit_high_water {
            return Ok(None);
        }
        self.stats.commit_records = self.stats.commit_records.saturating_add(1);
        let Some(record) = snapshot
            .commits
            .get_with_buffer(id.get(), &mut self.commit_record)?
        else {
            return Ok(None);
        };
        validate_identity("commit", id.get(), record.id)?;
        validate_child_high_water(
            "commit",
            id.get(),
            record.files.end_exclusive(),
            snapshot.file_count(),
        )?;
        Ok(Some(StoredCommitMetadata {
            id,
            kind: record.kind,
            span: record.span.as_line_span(),
            files: record.files,
            finalized: true,
        }))
    }

    /// Stable file IDをpath blob I/Oなしで読み、親commitのchild範囲まで検証する。
    pub fn read_file(
        &mut self,
        snapshot: &StructureSnapshot,
        id: FileId,
    ) -> Result<Option<StoredFileMetadata>, StructureStoreError> {
        let Some(file) = self.read_file_without_parent(snapshot, id)? else {
            return Ok(None);
        };
        validate_file_parent(snapshot, file.id, file.commit_id, &mut self.parent_record)?;
        Ok(Some(file))
    }

    /// Callerが直前に同じsnapshotから読んだ親commitを再利用してfileを読む。
    ///
    /// `parent`は同じ`StructureSnapshot`の[`Self::read_commit`]結果でなければならない。この前提を
    /// 課すことで逐次走査中に親recordを子ごとにdecodeせず、ID・可視世代・child rangeだけを検証する。
    pub(crate) fn read_file_in_commit(
        &mut self,
        snapshot: &StructureSnapshot,
        id: FileId,
        parent: StoredCommitMetadata,
    ) -> Result<Option<StoredFileMetadata>, StructureStoreError> {
        let Some(file) = self.read_file_without_parent(snapshot, id)? else {
            return Ok(None);
        };
        validate_supplied_commit_parent(snapshot, file.id, file.commit_id, parent)?;
        Ok(Some(file))
    }

    fn read_file_without_parent(
        &mut self,
        snapshot: &StructureSnapshot,
        id: FileId,
    ) -> Result<Option<StoredFileMetadata>, StructureStoreError> {
        if id.get() == snapshot.file_high_water {
            let Some(file) = snapshot.open_file.as_ref().filter(|value| value.id == id) else {
                return Ok(None);
            };
            return Ok(Some(StoredFileMetadata {
                id: file.id,
                commit_id: file.commit_id,
                status: file.status,
                mode: file.mode,
                span: file.span,
                hunks: file.hunks,
                additions: file.additions,
                deletions: file.deletions,
                line_bounds: file.line_bounds,
                finalized: false,
            }));
        }
        if id.get() > snapshot.file_high_water {
            return Ok(None);
        }
        self.stats.file_records = self.stats.file_records.saturating_add(1);
        let Some(record) = snapshot
            .files
            .get_with_buffer(id.get(), &mut self.file_record)?
        else {
            return Ok(None);
        };
        validate_identity("file", id.get(), record.id)?;
        let commit_id = CommitId::new(record.commit_id);
        validate_child_high_water(
            "file",
            id.get(),
            record.hunks.end_exclusive(),
            snapshot.hunk_count(),
        )?;
        Ok(Some(StoredFileMetadata {
            id,
            commit_id,
            status: record.status,
            mode: record.mode,
            span: record.span.as_line_span(),
            hunks: record.hunks,
            additions: record.additions,
            deletions: record.deletions,
            line_bounds: record.line_bounds,
            finalized: true,
        }))
    }

    /// Stable hunk IDをblob I/Oなしで読み、親fileのchild範囲まで検証する。
    pub fn read_hunk(
        &mut self,
        snapshot: &StructureSnapshot,
        id: HunkId,
    ) -> Result<Option<StoredHunk>, StructureStoreError> {
        let Some(hunk) = self.read_hunk_without_parent(snapshot, id)? else {
            return Ok(None);
        };
        validate_hunk_parent(snapshot, hunk.id, hunk.file_id, &mut self.parent_record)?;
        Ok(Some(hunk))
    }

    /// Callerが直前に同じsnapshotから読んだ親fileを再利用してhunkを読む。
    ///
    /// `parent`は同じ`StructureSnapshot`の[`Self::read_file`]または
    /// [`Self::read_file_in_commit`]結果でなければならない。親recordはこの呼び出しではdecodeしない。
    pub(crate) fn read_hunk_in_file(
        &mut self,
        snapshot: &StructureSnapshot,
        id: HunkId,
        parent: StoredFileMetadata,
    ) -> Result<Option<StoredHunk>, StructureStoreError> {
        let Some(hunk) = self.read_hunk_without_parent(snapshot, id)? else {
            return Ok(None);
        };
        validate_supplied_file_parent(snapshot, hunk.id, hunk.file_id, parent)?;
        Ok(Some(hunk))
    }

    fn read_hunk_without_parent(
        &mut self,
        snapshot: &StructureSnapshot,
        id: HunkId,
    ) -> Result<Option<StoredHunk>, StructureStoreError> {
        if id.get() == snapshot.hunk_high_water {
            let Some(hunk) = snapshot.open_hunk.as_ref().filter(|value| value.id == id) else {
                return Ok(None);
            };
            return Ok(Some(StoredHunk {
                id: hunk.id,
                file_id: hunk.file_id,
                kind: hunk.kind,
                header_line: hunk.header_line,
                span: hunk.span,
                old_range: hunk.old_range,
                new_range: hunk.new_range,
                finalized: false,
            }));
        }
        if id.get() > snapshot.hunk_high_water {
            return Ok(None);
        }
        self.stats.hunk_records = self.stats.hunk_records.saturating_add(1);
        let Some(record) = snapshot
            .hunks
            .get_with_buffer(id.get(), &mut self.hunk_record)?
        else {
            return Ok(None);
        };
        validate_identity("hunk", id.get(), record.id)?;
        let file_id = FileId::new(record.file_id);
        Ok(Some(StoredHunk {
            id,
            file_id,
            kind: record.kind,
            header_line: LineId::new(record.header_line),
            span: record.span.as_line_span(),
            old_range: record.old_range,
            new_range: record.new_range,
            finalized: true,
        }))
    }

    /// `span.start >= line`となる最初のcommit cursorを返す。末尾ではcommit件数と等しい。
    pub fn commit_lower_bound(
        &mut self,
        snapshot: &StructureSnapshot,
        line: u64,
    ) -> Result<u64, StructureStoreError> {
        let count = snapshot.visible_counts()?.commits;
        let mut span_at = |reader: &mut Self, index| reader.commit_span_at(snapshot, index);
        self.bound_by_start(count, line, false, &mut span_at)
    }

    /// `span.start > line`となる最初のcommit cursorを返す。末尾ではcommit件数と等しい。
    pub fn commit_upper_bound(
        &mut self,
        snapshot: &StructureSnapshot,
        line: u64,
    ) -> Result<u64, StructureStoreError> {
        let count = snapshot.visible_counts()?.commits;
        let mut span_at = |reader: &mut Self, index| reader.commit_span_at(snapshot, index);
        self.bound_by_start(count, line, true, &mut span_at)
    }

    /// `span.start >= line`となる最初のfile cursorを返す。末尾ではfile件数と等しい。
    pub fn file_lower_bound(
        &mut self,
        snapshot: &StructureSnapshot,
        line: u64,
    ) -> Result<u64, StructureStoreError> {
        let count = snapshot.visible_counts()?.files;
        let mut span_at = |reader: &mut Self, index| reader.file_span_at(snapshot, index);
        self.bound_by_start(count, line, false, &mut span_at)
    }

    /// `span.start > line`となる最初のfile cursorを返す。末尾ではfile件数と等しい。
    pub fn file_upper_bound(
        &mut self,
        snapshot: &StructureSnapshot,
        line: u64,
    ) -> Result<u64, StructureStoreError> {
        let count = snapshot.visible_counts()?.files;
        let mut span_at = |reader: &mut Self, index| reader.file_span_at(snapshot, index);
        self.bound_by_start(count, line, true, &mut span_at)
    }

    /// `span.start >= line`となる最初のhunk cursorを返す。末尾ではhunk件数と等しい。
    pub fn hunk_lower_bound(
        &mut self,
        snapshot: &StructureSnapshot,
        line: u64,
    ) -> Result<u64, StructureStoreError> {
        let count = snapshot.visible_counts()?.hunks;
        let mut span_at = |reader: &mut Self, index| reader.hunk_span_at(snapshot, index);
        self.bound_by_start(count, line, false, &mut span_at)
    }

    /// `span.start > line`となる最初のhunk cursorを返す。末尾ではhunk件数と等しい。
    pub fn hunk_upper_bound(
        &mut self,
        snapshot: &StructureSnapshot,
        line: u64,
    ) -> Result<u64, StructureStoreError> {
        let count = snapshot.visible_counts()?.hunks;
        let mut span_at = |reader: &mut Self, index| reader.hunk_span_at(snapshot, index);
        self.bound_by_start(count, line, true, &mut span_at)
    }

    /// `line`より前に開始する`nth`件目のcommitを返す。`nth=1`が直前である。
    pub fn nth_commit_before(
        &mut self,
        snapshot: &StructureSnapshot,
        line: u64,
        nth: NonZeroU64,
    ) -> Result<Option<CommitId>, StructureStoreError> {
        Ok(self
            .commit_lower_bound(snapshot, line)?
            .checked_sub(nth.get())
            .map(CommitId::new))
    }

    /// `line`より後に開始する`nth`件目のcommitを返す。`nth=1`が直後である。
    pub fn nth_commit_after(
        &mut self,
        snapshot: &StructureSnapshot,
        line: u64,
        nth: NonZeroU64,
    ) -> Result<Option<CommitId>, StructureStoreError> {
        let count = snapshot.visible_counts()?.commits;
        Ok(
            nth_after_cursor(self.commit_upper_bound(snapshot, line)?, count, nth)
                .map(CommitId::new),
        )
    }

    /// `line`より前に開始する`nth`件目のfileを返す。`nth=1`が直前である。
    pub fn nth_file_before(
        &mut self,
        snapshot: &StructureSnapshot,
        line: u64,
        nth: NonZeroU64,
    ) -> Result<Option<FileId>, StructureStoreError> {
        Ok(self
            .file_lower_bound(snapshot, line)?
            .checked_sub(nth.get())
            .map(FileId::new))
    }

    /// `line`より後に開始する`nth`件目のfileを返す。`nth=1`が直後である。
    pub fn nth_file_after(
        &mut self,
        snapshot: &StructureSnapshot,
        line: u64,
        nth: NonZeroU64,
    ) -> Result<Option<FileId>, StructureStoreError> {
        let count = snapshot.visible_counts()?.files;
        Ok(nth_after_cursor(self.file_upper_bound(snapshot, line)?, count, nth).map(FileId::new))
    }

    /// `line`より前に開始する`nth`件目のhunkを返す。`nth=1`が直前である。
    pub fn nth_hunk_before(
        &mut self,
        snapshot: &StructureSnapshot,
        line: u64,
        nth: NonZeroU64,
    ) -> Result<Option<HunkId>, StructureStoreError> {
        Ok(self
            .hunk_lower_bound(snapshot, line)?
            .checked_sub(nth.get())
            .map(HunkId::new))
    }

    /// `line`より後に開始する`nth`件目のhunkを返す。`nth=1`が直後である。
    pub fn nth_hunk_after(
        &mut self,
        snapshot: &StructureSnapshot,
        line: u64,
        nth: NonZeroU64,
    ) -> Result<Option<HunkId>, StructureStoreError> {
        let count = snapshot.visible_counts()?.hunks;
        Ok(nth_after_cursor(self.hunk_upper_bound(snapshot, line)?, count, nth).map(HunkId::new))
    }

    /// `lines`と交差するcommitの半開cursor範囲を返す。
    pub fn commits_intersecting(
        &mut self,
        snapshot: &StructureSnapshot,
        lines: Range<u64>,
    ) -> Result<Range<u64>, StructureStoreError> {
        let count = snapshot.visible_counts()?.commits;
        let mut span_at = |reader: &mut Self, index| reader.commit_span_at(snapshot, index);
        self.intersecting_cursors(count, lines, &mut span_at)
    }

    /// `lines`と交差するfileをstable ID範囲で返す。空でも挿入cursorを保持する。
    pub fn files_intersecting(
        &mut self,
        snapshot: &StructureSnapshot,
        lines: Range<u64>,
    ) -> Result<FileIdRange, StructureStoreError> {
        let count = snapshot.visible_counts()?.files;
        let mut span_at = |reader: &mut Self, index| reader.file_span_at(snapshot, index);
        let cursors = self.intersecting_cursors(count, lines, &mut span_at)?;
        Ok(FileIdRange {
            start: cursors.start,
            count: cursors.end - cursors.start,
        })
    }

    /// `lines`と交差するhunkをstable ID範囲で返す。空でも挿入cursorを保持する。
    pub fn hunks_intersecting(
        &mut self,
        snapshot: &StructureSnapshot,
        lines: Range<u64>,
    ) -> Result<HunkIdRange, StructureStoreError> {
        let count = snapshot.visible_counts()?.hunks;
        let mut span_at = |reader: &mut Self, index| reader.hunk_span_at(snapshot, index);
        let cursors = self.intersecting_cursors(count, lines, &mut span_at)?;
        Ok(HunkIdRange {
            start: cursors.start,
            count: cursors.end - cursors.start,
        })
    }

    fn commit_span_at(
        &mut self,
        snapshot: &StructureSnapshot,
        index: u64,
    ) -> Result<LineSpan, StructureStoreError> {
        self.read_commit(snapshot, CommitId::new(index))?
            .map(|value| value.span)
            .ok_or(StructureStoreError::OutsideSnapshot {
                kind: "commit",
                id: index,
                high_water: snapshot.commit_count(),
            })
    }

    fn file_span_at(
        &mut self,
        snapshot: &StructureSnapshot,
        index: u64,
    ) -> Result<LineSpan, StructureStoreError> {
        self.read_file_without_parent(snapshot, FileId::new(index))?
            .map(|value| value.span)
            .ok_or(StructureStoreError::OutsideSnapshot {
                kind: "file",
                id: index,
                high_water: snapshot.file_count(),
            })
    }

    fn hunk_span_at(
        &mut self,
        snapshot: &StructureSnapshot,
        index: u64,
    ) -> Result<LineSpan, StructureStoreError> {
        self.read_hunk_without_parent(snapshot, HunkId::new(index))?
            .map(|value| value.span)
            .ok_or(StructureStoreError::OutsideSnapshot {
                kind: "hunk",
                id: index,
                high_water: snapshot.hunk_count(),
            })
    }

    fn bound_by_start<F>(
        &mut self,
        count: u64,
        line: u64,
        strict: bool,
        span_at: &mut F,
    ) -> Result<u64, StructureStoreError>
    where
        F: FnMut(&mut Self, u64) -> Result<LineSpan, StructureStoreError>,
    {
        let mut low = 0;
        let mut high = count;
        while low < high {
            let middle = low + (high - low) / 2;
            let start = span_at(self, middle)?.start.get();
            if start < line || (strict && start == line) {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Ok(low)
    }

    fn intersecting_cursors<F>(
        &mut self,
        count: u64,
        lines: Range<u64>,
        span_at: &mut F,
    ) -> Result<Range<u64>, StructureStoreError>
    where
        F: FnMut(&mut Self, u64) -> Result<LineSpan, StructureStoreError>,
    {
        let mut start = self.bound_by_start(count, lines.start, false, span_at)?;
        if lines.is_empty() {
            return Ok(start..start);
        }
        if start > 0 {
            let previous = span_at(self, start - 1)?;
            if previous.end_exclusive.is_none_or(|end| end > lines.start) {
                start -= 1;
            }
        }
        let end = self.bound_by_start(count, lines.end, false, span_at)?;
        Ok(start..end.max(start))
    }
}

fn nth_after_cursor(cursor: u64, count: u64, nth: NonZeroU64) -> Option<u64> {
    cursor
        .checked_add(nth.get() - 1)
        .filter(|index| *index < count)
}

/// Fixed recordとblob stringをcaller-owned bufferへdecodeする再利用可能reader。
///
/// 返されたstring sliceは次に同じreaderでcommitまたはfileを読むまで有効である。Record bufferはtable
/// ごとに分け、parent range検証用bufferも保持するため、spill recordの反復readでallocationしない。
#[derive(Debug, Default)]
pub struct StructureReader {
    commit_record: Vec<u8>,
    file_record: Vec<u8>,
    hunk_record: Vec<u8>,
    parent_record: Vec<u8>,
    first_string: Vec<u8>,
    second_string: Vec<u8>,
}

impl StructureReader {
    /// Bufferをまだ確保していないreaderを作る。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            commit_record: Vec::new(),
            file_record: Vec::new(),
            hunk_record: Vec::new(),
            parent_record: Vec::new(),
            first_string: Vec::new(),
            second_string: Vec::new(),
        }
    }

    /// Stable commit IDをsnapshotから読む。Open overlayにも同じtyped viewを返す。
    pub fn read_commit<'a>(
        &'a mut self,
        snapshot: &'a StructureSnapshot,
        id: CommitId,
    ) -> Result<Option<StoredCommitRef<'a>>, StructureStoreError> {
        if id.get() == snapshot.commit_high_water {
            return Ok(snapshot
                .open_commit
                .as_ref()
                .filter(|value| value.id == id)
                .map(|commit| StoredCommitRef {
                    id: commit.id,
                    kind: commit.kind,
                    object_id: commit.object_id.as_deref(),
                    subject: commit.subject.as_deref(),
                    span: commit.span,
                    files: commit.files,
                    finalized: false,
                }));
        }
        if id.get() > snapshot.commit_high_water {
            return Ok(None);
        }
        let Some(record) = snapshot
            .commits
            .get_with_buffer(id.get(), &mut self.commit_record)?
        else {
            return Ok(None);
        };
        validate_identity("commit", id.get(), record.id)?;
        validate_child_high_water(
            "commit",
            id.get(),
            record.files.end_exclusive(),
            snapshot.file_count(),
        )?;
        read_optional_blob(&snapshot.blobs, record.object_id, &mut self.first_string)?;
        read_optional_blob(&snapshot.blobs, record.subject, &mut self.second_string)?;
        let object_id = optional_utf8(record.object_id, &self.first_string, "commit object ID")?;
        let subject = optional_utf8(record.subject, &self.second_string, "commit subject")?;
        Ok(Some(StoredCommitRef {
            id,
            kind: record.kind,
            object_id,
            subject,
            span: record.span.as_line_span(),
            files: record.files,
            finalized: true,
        }))
    }

    /// Stable file IDをsnapshotから読み、parent commitのchild範囲も検証する。
    pub fn read_file<'a>(
        &'a mut self,
        snapshot: &'a StructureSnapshot,
        id: FileId,
    ) -> Result<Option<StoredFileRef<'a>>, StructureStoreError> {
        if id.get() == snapshot.file_high_water {
            let Some(file) = snapshot.open_file.as_ref().filter(|value| value.id == id) else {
                return Ok(None);
            };
            validate_file_parent(snapshot, file.id, file.commit_id, &mut self.parent_record)?;
            return Ok(Some(StoredFileRef {
                id: file.id,
                commit_id: file.commit_id,
                old_path: file.old_path.as_deref(),
                new_path: file.new_path.as_deref(),
                status: file.status,
                mode: file.mode,
                span: file.span,
                hunks: file.hunks,
                additions: file.additions,
                deletions: file.deletions,
                line_bounds: file.line_bounds,
                finalized: false,
            }));
        }
        if id.get() > snapshot.file_high_water {
            return Ok(None);
        }
        let Some(record) = snapshot
            .files
            .get_with_buffer(id.get(), &mut self.file_record)?
        else {
            return Ok(None);
        };
        validate_identity("file", id.get(), record.id)?;
        let commit_id = CommitId::new(record.commit_id);
        validate_file_parent(snapshot, id, commit_id, &mut self.parent_record)?;
        validate_child_high_water(
            "file",
            id.get(),
            record.hunks.end_exclusive(),
            snapshot.hunk_count(),
        )?;
        read_optional_blob(&snapshot.blobs, record.old_path, &mut self.first_string)?;
        read_optional_blob(&snapshot.blobs, record.new_path, &mut self.second_string)?;
        let old_path = optional_utf8(record.old_path, &self.first_string, "old path")?;
        let new_path = optional_utf8(record.new_path, &self.second_string, "new path")?;
        Ok(Some(StoredFileRef {
            id,
            commit_id,
            old_path,
            new_path,
            status: record.status,
            mode: record.mode,
            span: record.span.as_line_span(),
            hunks: record.hunks,
            additions: record.additions,
            deletions: record.deletions,
            line_bounds: record.line_bounds,
            finalized: true,
        }))
    }

    /// Callerが同じsnapshotから読んだ親commit metadataを再利用してfileを読む。
    ///
    /// `parent`は[`Self::read_commit`]の戻り値を[`StoredCommitRef::metadata`]で変換した値でなければ
    /// ならない。Parent tableを再decodeしないため、同一commit内のfile逐次走査に使う。
    pub(crate) fn read_file_in_commit<'a>(
        &'a mut self,
        snapshot: &'a StructureSnapshot,
        id: FileId,
        parent: StoredCommitMetadata,
    ) -> Result<Option<StoredFileRef<'a>>, StructureStoreError> {
        if id.get() == snapshot.file_high_water {
            let Some(file) = snapshot.open_file.as_ref().filter(|value| value.id == id) else {
                return Ok(None);
            };
            validate_supplied_commit_parent(snapshot, file.id, file.commit_id, parent)?;
            return Ok(Some(StoredFileRef {
                id: file.id,
                commit_id: file.commit_id,
                old_path: file.old_path.as_deref(),
                new_path: file.new_path.as_deref(),
                status: file.status,
                mode: file.mode,
                span: file.span,
                hunks: file.hunks,
                additions: file.additions,
                deletions: file.deletions,
                line_bounds: file.line_bounds,
                finalized: false,
            }));
        }
        if id.get() > snapshot.file_high_water {
            return Ok(None);
        }
        let Some(record) = snapshot
            .files
            .get_with_buffer(id.get(), &mut self.file_record)?
        else {
            return Ok(None);
        };
        validate_identity("file", id.get(), record.id)?;
        let commit_id = CommitId::new(record.commit_id);
        validate_supplied_commit_parent(snapshot, id, commit_id, parent)?;
        validate_child_high_water(
            "file",
            id.get(),
            record.hunks.end_exclusive(),
            snapshot.hunk_count(),
        )?;
        read_optional_blob(&snapshot.blobs, record.old_path, &mut self.first_string)?;
        read_optional_blob(&snapshot.blobs, record.new_path, &mut self.second_string)?;
        let old_path = optional_utf8(record.old_path, &self.first_string, "old path")?;
        let new_path = optional_utf8(record.new_path, &self.second_string, "new path")?;
        Ok(Some(StoredFileRef {
            id,
            commit_id,
            old_path,
            new_path,
            status: record.status,
            mode: record.mode,
            span: record.span.as_line_span(),
            hunks: record.hunks,
            additions: record.additions,
            deletions: record.deletions,
            line_bounds: record.line_bounds,
            finalized: true,
        }))
    }

    /// Stable hunk IDをsnapshotから読み、parent fileのchild範囲も検証する。
    pub fn read_hunk(
        &mut self,
        snapshot: &StructureSnapshot,
        id: HunkId,
    ) -> Result<Option<StoredHunk>, StructureStoreError> {
        if id.get() == snapshot.hunk_high_water {
            let Some(hunk) = snapshot.open_hunk.as_ref().filter(|value| value.id == id) else {
                return Ok(None);
            };
            validate_hunk_parent(snapshot, hunk.id, hunk.file_id, &mut self.parent_record)?;
            return Ok(Some(StoredHunk {
                id: hunk.id,
                file_id: hunk.file_id,
                kind: hunk.kind,
                header_line: hunk.header_line,
                span: hunk.span,
                old_range: hunk.old_range,
                new_range: hunk.new_range,
                finalized: false,
            }));
        }
        if id.get() > snapshot.hunk_high_water {
            return Ok(None);
        }
        let Some(record) = snapshot
            .hunks
            .get_with_buffer(id.get(), &mut self.hunk_record)?
        else {
            return Ok(None);
        };
        validate_identity("hunk", id.get(), record.id)?;
        let file_id = FileId::new(record.file_id);
        validate_hunk_parent(snapshot, id, file_id, &mut self.parent_record)?;
        Ok(Some(StoredHunk {
            id,
            file_id,
            kind: record.kind,
            header_line: LineId::new(record.header_line),
            span: record.span.as_line_span(),
            old_range: record.old_range,
            new_range: record.new_range,
            finalized: true,
        }))
    }

    /// Callerが同じsnapshotから読んだ親file metadataを再利用してhunkを読む。
    ///
    /// `parent`は[`Self::read_file`]または[`Self::read_file_in_commit`]の戻り値を
    /// [`StoredFileRef::metadata`]で変換した値でなければならない。Parent tableはdecodeしない。
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "同一snapshotの親metadataを再利用するcrate内境界として保持する"
        )
    )]
    pub(crate) fn read_hunk_in_file(
        &mut self,
        snapshot: &StructureSnapshot,
        id: HunkId,
        parent: StoredFileMetadata,
    ) -> Result<Option<StoredHunk>, StructureStoreError> {
        if id.get() == snapshot.hunk_high_water {
            let Some(hunk) = snapshot.open_hunk.as_ref().filter(|value| value.id == id) else {
                return Ok(None);
            };
            validate_supplied_file_parent(snapshot, hunk.id, hunk.file_id, parent)?;
            return Ok(Some(StoredHunk {
                id: hunk.id,
                file_id: hunk.file_id,
                kind: hunk.kind,
                header_line: hunk.header_line,
                span: hunk.span,
                old_range: hunk.old_range,
                new_range: hunk.new_range,
                finalized: false,
            }));
        }
        if id.get() > snapshot.hunk_high_water {
            return Ok(None);
        }
        let Some(record) = snapshot
            .hunks
            .get_with_buffer(id.get(), &mut self.hunk_record)?
        else {
            return Ok(None);
        };
        validate_identity("hunk", id.get(), record.id)?;
        let file_id = FileId::new(record.file_id);
        validate_supplied_file_parent(snapshot, id, file_id, parent)?;
        Ok(Some(StoredHunk {
            id,
            file_id,
            kind: record.kind,
            header_line: LineId::new(record.header_line),
            span: record.span.as_line_span(),
            old_range: record.old_range,
            new_range: record.new_range,
            finalized: true,
        }))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClosedLineSpan {
    start: u64,
    end_exclusive: u64,
}

impl ClosedLineSpan {
    fn close(span: LineSpan, end_exclusive: u64) -> Result<Self, StructureStoreError> {
        validate_id("line", span.start.get())?;
        if end_exclusive <= span.start.get() {
            return Err(StructureStoreError::InvalidLineSpan {
                start: span.start.get(),
                end: end_exclusive,
            });
        }
        Ok(Self {
            start: span.start.get(),
            end_exclusive,
        })
    }

    const fn as_line_span(self) -> LineSpan {
        LineSpan {
            start: LineId::new(self.start),
            end_exclusive: Some(self.end_exclusive),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PackedBlobRef {
    tag: u8,
    location: u64,
    len: u64,
}

impl PackedBlobRef {
    const NONE: Self = Self {
        tag: BLOB_NONE,
        location: 0,
        len: 0,
    };

    fn from_blob(reference: Option<BlobRef>) -> Result<Self, RecordCodecError> {
        let Some(reference) = reference else {
            return Ok(Self::NONE);
        };
        match reference.location() {
            BlobLocation::Empty if reference.is_empty() => Ok(Self {
                tag: BLOB_EMPTY,
                location: 0,
                len: 0,
            }),
            BlobLocation::Memory { page, offset } if !reference.is_empty() => Ok(Self {
                tag: BLOB_MEMORY,
                location: (u64::from(page) << 32) | u64::from(offset),
                len: reference.len(),
            }),
            BlobLocation::Disk { offset } if !reference.is_empty() => Ok(Self {
                tag: BLOB_DISK,
                location: offset,
                len: reference.len(),
            }),
            _ => Err(RecordCodecError::new("invalid optional blob reference")),
        }
    }

    const fn is_some(self) -> bool {
        self.tag != BLOB_NONE
    }

    fn bind(self, arena: &BlobArena) -> Result<Option<BlobRef>, StorageError> {
        let location = match self.tag {
            BLOB_NONE => return Ok(None),
            BLOB_EMPTY => BlobLocation::Empty,
            BLOB_MEMORY => BlobLocation::Memory {
                page: (self.location >> 32) as u32,
                offset: self.location as u32,
            },
            BLOB_DISK => BlobLocation::Disk {
                offset: self.location,
            },
            _ => {
                return Err(StorageError::Codec(RecordCodecError::new(
                    "unknown blob tag",
                )));
            }
        };
        arena.bind(location, self.len).map(Some)
    }

    fn encode(self, output: &mut [u8]) -> Result<(), RecordCodecError> {
        if output.len() != PACKED_BLOB_BYTES {
            return Err(RecordCodecError::new("invalid packed blob output width"));
        }
        validate_packed_blob(self)?;
        output.fill(0);
        output[0] = self.tag;
        put_u64(output, 8, self.location)?;
        put_u64(output, 16, self.len)
    }

    fn decode(input: &[u8]) -> Result<Self, RecordCodecError> {
        if input.len() != PACKED_BLOB_BYTES {
            return Err(RecordCodecError::new("invalid packed blob input width"));
        }
        require_zero(&input[1..8], "non-zero packed blob reserved bytes")?;
        let value = Self {
            tag: input[0],
            location: get_u64(input, 8)?,
            len: get_u64(input, 16)?,
        };
        validate_packed_blob(value)?;
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CommitRecord {
    id: u64,
    kind: CommitKind,
    span: ClosedLineSpan,
    files: FileIdRange,
    object_id: PackedBlobRef,
    subject: PackedBlobRef,
}

impl CommitRecord {
    fn from_open(
        commit: &OpenCommit,
        end_exclusive: u64,
        object_id: PackedBlobRef,
        subject: PackedBlobRef,
    ) -> Result<Self, StructureStoreError> {
        validate_id("commit", commit.id.get())?;
        validate_file_range(commit.files)?;
        Ok(Self {
            id: commit.id.get(),
            kind: commit.kind,
            span: ClosedLineSpan::close(commit.span, end_exclusive)?,
            files: commit.files,
            object_id,
            subject,
        })
    }
}

impl FixedRecord for CommitRecord {
    const ENCODED_LEN: usize = COMMIT_RECORD_BYTES;

    fn encode(&self, output: &mut [u8]) -> Result<(), RecordCodecError> {
        require_width(
            output,
            Self::ENCODED_LEN,
            "invalid commit record output width",
        )?;
        validate_id_codec(self.id, "commit ID uses reserved sentinel")?;
        validate_closed_span_codec(self.span)?;
        validate_file_range_codec(self.files)?;
        output.fill(0);
        put_u64(output, 0, self.id)?;
        output[8] = encode_commit_kind(self.kind);
        put_u64(output, 16, self.span.start)?;
        put_u64(output, 24, self.span.end_exclusive)?;
        encode_file_range(self.files, output, 32)?;
        self.object_id.encode(&mut output[48..72])?;
        self.subject.encode(&mut output[72..96])
    }

    fn decode(input: &[u8]) -> Result<Self, RecordCodecError> {
        require_width(
            input,
            Self::ENCODED_LEN,
            "invalid commit record input width",
        )?;
        require_zero(&input[9..16], "non-zero commit reserved bytes")?;
        let value = Self {
            id: get_u64(input, 0)?,
            kind: decode_commit_kind(input[8])?,
            span: ClosedLineSpan {
                start: get_u64(input, 16)?,
                end_exclusive: get_u64(input, 24)?,
            },
            files: decode_file_range(input, 32)?,
            object_id: PackedBlobRef::decode(&input[48..72])?,
            subject: PackedBlobRef::decode(&input[72..96])?,
        };
        validate_id_codec(value.id, "commit ID uses reserved sentinel")?;
        validate_closed_span_codec(value.span)?;
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileRecord {
    id: u64,
    commit_id: u64,
    span: ClosedLineSpan,
    hunks: HunkIdRange,
    additions: Option<u64>,
    deletions: Option<u64>,
    old_path: PackedBlobRef,
    new_path: PackedBlobRef,
    status: ChangeStatus,
    mode: FileMode,
    line_bounds: StructureFileLineBounds,
}

impl FileRecord {
    fn from_open(
        file: &OpenFile,
        end_exclusive: u64,
        old_path: PackedBlobRef,
        new_path: PackedBlobRef,
    ) -> Result<Self, StructureStoreError> {
        validate_id("file", file.id.get())?;
        validate_id("commit", file.commit_id.get())?;
        validate_hunk_range(file.hunks)?;
        validate_optional_u64("additions", file.additions)?;
        validate_optional_u64("deletions", file.deletions)?;
        validate_optional_u64(
            "maximum old line number",
            file.line_bounds.max_old_line_number,
        )?;
        validate_optional_u64(
            "maximum new line number",
            file.line_bounds.max_new_line_number,
        )?;
        validate_file_counts(file.id.get(), file.mode, file.additions, file.deletions)?;
        Ok(Self {
            id: file.id.get(),
            commit_id: file.commit_id.get(),
            span: ClosedLineSpan::close(file.span, end_exclusive)?,
            hunks: file.hunks,
            additions: file.additions,
            deletions: file.deletions,
            old_path,
            new_path,
            status: file.status,
            mode: file.mode,
            line_bounds: file.line_bounds,
        })
    }
}

impl FixedRecord for FileRecord {
    const ENCODED_LEN: usize = FILE_RECORD_BYTES;

    fn encode(&self, output: &mut [u8]) -> Result<(), RecordCodecError> {
        require_width(
            output,
            Self::ENCODED_LEN,
            "invalid file record output width",
        )?;
        validate_id_codec(self.id, "file ID uses reserved sentinel")?;
        validate_id_codec(self.commit_id, "commit ID uses reserved sentinel")?;
        validate_closed_span_codec(self.span)?;
        validate_hunk_range_codec(self.hunks)?;
        validate_optional_codec(self.additions, "additions uses reserved sentinel")?;
        validate_optional_codec(self.deletions, "deletions uses reserved sentinel")?;
        validate_file_counts_codec(self.mode, self.additions, self.deletions)?;
        validate_optional_codec(
            self.line_bounds.max_old_line_number,
            "maximum old line uses reserved sentinel",
        )?;
        validate_optional_codec(
            self.line_bounds.max_new_line_number,
            "maximum new line uses reserved sentinel",
        )?;
        output.fill(0);
        put_u64(output, 0, self.id)?;
        put_u64(output, 8, self.commit_id)?;
        put_u64(output, 16, self.span.start)?;
        put_u64(output, 24, self.span.end_exclusive)?;
        encode_hunk_range(self.hunks, output, 32)?;
        put_u64(output, 48, encode_optional_u64(self.additions))?;
        put_u64(output, 56, encode_optional_u64(self.deletions))?;
        self.old_path.encode(&mut output[64..88])?;
        self.new_path.encode(&mut output[88..112])?;
        output[112] = encode_change_status(self.status);
        output[113] = encode_file_mode(self.mode);
        put_u64(
            output,
            120,
            encode_optional_u64(self.line_bounds.max_old_line_number),
        )?;
        put_u64(
            output,
            128,
            encode_optional_u64(self.line_bounds.max_new_line_number),
        )
    }

    fn decode(input: &[u8]) -> Result<Self, RecordCodecError> {
        require_width(input, Self::ENCODED_LEN, "invalid file record input width")?;
        require_zero(&input[114..120], "non-zero file reserved bytes")?;
        require_zero(&input[136..144], "non-zero file trailing reserved bytes")?;
        let value = Self {
            id: get_u64(input, 0)?,
            commit_id: get_u64(input, 8)?,
            span: ClosedLineSpan {
                start: get_u64(input, 16)?,
                end_exclusive: get_u64(input, 24)?,
            },
            hunks: decode_hunk_range(input, 32)?,
            additions: decode_optional_u64(get_u64(input, 48)?),
            deletions: decode_optional_u64(get_u64(input, 56)?),
            old_path: PackedBlobRef::decode(&input[64..88])?,
            new_path: PackedBlobRef::decode(&input[88..112])?,
            status: decode_change_status(input[112])?,
            mode: decode_file_mode(input[113])?,
            line_bounds: StructureFileLineBounds {
                max_old_line_number: decode_optional_u64(get_u64(input, 120)?),
                max_new_line_number: decode_optional_u64(get_u64(input, 128)?),
            },
        };
        validate_id_codec(value.id, "file ID uses reserved sentinel")?;
        validate_id_codec(value.commit_id, "commit ID uses reserved sentinel")?;
        validate_closed_span_codec(value.span)?;
        validate_file_counts_codec(value.mode, value.additions, value.deletions)?;
        validate_optional_codec(
            value.line_bounds.max_old_line_number,
            "maximum old line uses reserved sentinel",
        )?;
        validate_optional_codec(
            value.line_bounds.max_new_line_number,
            "maximum new line uses reserved sentinel",
        )?;
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HunkRecord {
    id: u64,
    file_id: u64,
    header_line: u64,
    span: ClosedLineSpan,
    kind: HunkKind,
    old_range: Option<SourceRange>,
    new_range: Option<SourceRange>,
}

impl HunkRecord {
    fn from_open(hunk: &OpenHunk, end_exclusive: u64) -> Result<Self, StructureStoreError> {
        validate_id("hunk", hunk.id.get())?;
        validate_id("file", hunk.file_id.get())?;
        validate_id("line", hunk.header_line.get())?;
        validate_hunk_ranges(hunk.id.get(), hunk.kind, hunk.old_range, hunk.new_range)?;
        Ok(Self {
            id: hunk.id.get(),
            file_id: hunk.file_id.get(),
            header_line: hunk.header_line.get(),
            span: ClosedLineSpan::close(hunk.span, end_exclusive)?,
            kind: hunk.kind,
            old_range: hunk.old_range,
            new_range: hunk.new_range,
        })
    }
}

impl FixedRecord for HunkRecord {
    const ENCODED_LEN: usize = HUNK_RECORD_BYTES;

    fn encode(&self, output: &mut [u8]) -> Result<(), RecordCodecError> {
        require_width(
            output,
            Self::ENCODED_LEN,
            "invalid hunk record output width",
        )?;
        validate_id_codec(self.id, "hunk ID uses reserved sentinel")?;
        validate_id_codec(self.file_id, "file ID uses reserved sentinel")?;
        validate_id_codec(self.header_line, "header line ID uses reserved sentinel")?;
        validate_closed_span_codec(self.span)?;
        validate_hunk_ranges_codec(self.kind, self.old_range, self.new_range)?;
        if self.header_line != self.span.start {
            return Err(RecordCodecError::new(
                "hunk header line does not match span start",
            ));
        }
        output.fill(0);
        put_u64(output, 0, self.id)?;
        put_u64(output, 8, self.file_id)?;
        put_u64(output, 16, self.header_line)?;
        put_u64(output, 24, self.span.start)?;
        put_u64(output, 32, self.span.end_exclusive)?;
        output[40] = encode_hunk_kind(self.kind);
        output[41] = u8::from(self.old_range.is_some()) | (u8::from(self.new_range.is_some()) << 1);
        if let Some(range) = self.old_range {
            put_u64(output, 48, range.start)?;
            put_u64(output, 56, range.count)?;
        }
        if let Some(range) = self.new_range {
            put_u64(output, 64, range.start)?;
            put_u64(output, 72, range.count)?;
        }
        Ok(())
    }

    fn decode(input: &[u8]) -> Result<Self, RecordCodecError> {
        require_width(input, Self::ENCODED_LEN, "invalid hunk record input width")?;
        require_zero(&input[42..48], "non-zero hunk reserved bytes")?;
        let flags = input[41];
        if flags & !0b11 != 0 {
            return Err(RecordCodecError::new("unknown hunk range flags"));
        }
        let old_values = (get_u64(input, 48)?, get_u64(input, 56)?);
        let new_values = (get_u64(input, 64)?, get_u64(input, 72)?);
        let old_range = decode_source_range(flags & 1 != 0, old_values)?;
        let new_range = decode_source_range(flags & 2 != 0, new_values)?;
        let value = Self {
            id: get_u64(input, 0)?,
            file_id: get_u64(input, 8)?,
            header_line: get_u64(input, 16)?,
            span: ClosedLineSpan {
                start: get_u64(input, 24)?,
                end_exclusive: get_u64(input, 32)?,
            },
            kind: decode_hunk_kind(input[40])?,
            old_range,
            new_range,
        };
        validate_id_codec(value.id, "hunk ID uses reserved sentinel")?;
        validate_id_codec(value.file_id, "file ID uses reserved sentinel")?;
        validate_id_codec(value.header_line, "header line ID uses reserved sentinel")?;
        validate_closed_span_codec(value.span)?;
        validate_hunk_ranges_codec(value.kind, value.old_range, value.new_range)?;
        if value.header_line != value.span.start {
            return Err(RecordCodecError::new(
                "hunk header line does not match span start",
            ));
        }
        Ok(value)
    }
}

fn validate_open_overlay(
    commit_high_water: u64,
    file_high_water: u64,
    hunk_high_water: u64,
    commit: Option<&OpenCommit>,
    file: Option<&OpenFile>,
    hunk: Option<&OpenHunk>,
) -> Result<(), StructureStoreError> {
    let visible_file_count = file_high_water
        .checked_add(u64::from(file.is_some()))
        .ok_or(StructureStoreError::IdRangeOverflow {
            kind: "file high-water",
            first: file_high_water,
            count: u64::from(file.is_some()),
        })?;
    let visible_hunk_count = hunk_high_water
        .checked_add(u64::from(hunk.is_some()))
        .ok_or(StructureStoreError::IdRangeOverflow {
            kind: "hunk high-water",
            first: hunk_high_water,
            count: u64::from(hunk.is_some()),
        })?;
    if let Some(commit) = commit {
        validate_identity("open commit", commit_high_water, commit.id.get())?;
        validate_id("line", commit.span.start.get())?;
        validate_file_range(commit.files)?;
        validate_child_high_water(
            "open commit",
            commit.id.get(),
            commit.files.end_exclusive(),
            visible_file_count,
        )?;
    } else if let Some(file) = file {
        return Err(StructureStoreError::InvalidParent {
            child_kind: "open file",
            child_id: file.id.get(),
            parent_kind: "open commit",
            parent_id: file.commit_id.get(),
            parent_count: commit_high_water,
        });
    }
    if let Some(file) = file {
        validate_identity("open file", file_high_water, file.id.get())?;
        let parent = commit.ok_or(StructureStoreError::MissingOpenParent {
            operation: "snapshot open file",
            required_kind: "commit",
        })?;
        if file.commit_id != parent.id || !parent.files.contains(file.id) {
            return Err(StructureStoreError::ParentRangeMismatch {
                parent_kind: "commit",
                parent_id: file.commit_id.get(),
                child_kind: "file",
                child_id: file.id.get(),
            });
        }
        validate_hunk_range(file.hunks)?;
        validate_optional_u64("additions", file.additions)?;
        validate_optional_u64("deletions", file.deletions)?;
        validate_file_counts(file.id.get(), file.mode, file.additions, file.deletions)?;
        validate_optional_u64(
            "maximum old line number",
            file.line_bounds.max_old_line_number,
        )?;
        validate_optional_u64(
            "maximum new line number",
            file.line_bounds.max_new_line_number,
        )?;
        validate_child_high_water(
            "open file",
            file.id.get(),
            file.hunks.end_exclusive(),
            visible_hunk_count,
        )?;
    } else if let Some(hunk) = hunk {
        return Err(StructureStoreError::InvalidParent {
            child_kind: "open hunk",
            child_id: hunk.id.get(),
            parent_kind: "open file",
            parent_id: hunk.file_id.get(),
            parent_count: file_high_water,
        });
    }
    if let Some(hunk) = hunk {
        validate_identity("open hunk", hunk_high_water, hunk.id.get())?;
        let parent = file.ok_or(StructureStoreError::MissingOpenParent {
            operation: "snapshot open hunk",
            required_kind: "file",
        })?;
        if hunk.file_id != parent.id || !parent.hunks.contains(hunk.id) {
            return Err(StructureStoreError::ParentRangeMismatch {
                parent_kind: "file",
                parent_id: hunk.file_id.get(),
                child_kind: "hunk",
                child_id: hunk.id.get(),
            });
        }
        validate_hunk_ranges(hunk.id.get(), hunk.kind, hunk.old_range, hunk.new_range)?;
    }
    Ok(())
}

fn validate_file_parent(
    snapshot: &StructureSnapshot,
    file_id: FileId,
    commit_id: CommitId,
    buffer: &mut Vec<u8>,
) -> Result<(), StructureStoreError> {
    if commit_id.get() >= snapshot.commit_count() {
        return Err(StructureStoreError::InvalidParent {
            child_kind: "file",
            child_id: file_id.get(),
            parent_kind: "commit",
            parent_id: commit_id.get(),
            parent_count: snapshot.commit_count(),
        });
    }
    let range = if commit_id.get() == snapshot.commit_high_water {
        snapshot
            .open_commit
            .as_ref()
            .filter(|commit| commit.id == commit_id)
            .map(|commit| commit.files)
            .ok_or(StructureStoreError::InvalidParent {
                child_kind: "file",
                child_id: file_id.get(),
                parent_kind: "commit",
                parent_id: commit_id.get(),
                parent_count: snapshot.commit_count(),
            })?
    } else {
        let record = snapshot
            .commits
            .get_with_buffer(commit_id.get(), buffer)?
            .ok_or(StructureStoreError::OutsideSnapshot {
                kind: "commit",
                id: commit_id.get(),
                high_water: snapshot.commit_high_water,
            })?;
        validate_identity("commit", commit_id.get(), record.id)?;
        record.files
    };
    validate_child_high_water(
        "commit",
        commit_id.get(),
        range.end_exclusive(),
        snapshot.file_count(),
    )?;
    if !range.contains(file_id) {
        return Err(StructureStoreError::ParentRangeMismatch {
            parent_kind: "commit",
            parent_id: commit_id.get(),
            child_kind: "file",
            child_id: file_id.get(),
        });
    }
    Ok(())
}

fn validate_hunk_parent(
    snapshot: &StructureSnapshot,
    hunk_id: HunkId,
    file_id: FileId,
    buffer: &mut Vec<u8>,
) -> Result<(), StructureStoreError> {
    if file_id.get() >= snapshot.file_count() {
        return Err(StructureStoreError::InvalidParent {
            child_kind: "hunk",
            child_id: hunk_id.get(),
            parent_kind: "file",
            parent_id: file_id.get(),
            parent_count: snapshot.file_count(),
        });
    }
    let range = if file_id.get() == snapshot.file_high_water {
        snapshot
            .open_file
            .as_ref()
            .filter(|file| file.id == file_id)
            .map(|file| file.hunks)
            .ok_or(StructureStoreError::InvalidParent {
                child_kind: "hunk",
                child_id: hunk_id.get(),
                parent_kind: "file",
                parent_id: file_id.get(),
                parent_count: snapshot.file_count(),
            })?
    } else {
        let record = snapshot
            .files
            .get_with_buffer(file_id.get(), buffer)?
            .ok_or(StructureStoreError::OutsideSnapshot {
                kind: "file",
                id: file_id.get(),
                high_water: snapshot.file_high_water,
            })?;
        validate_identity("file", file_id.get(), record.id)?;
        record.hunks
    };
    validate_child_high_water(
        "file",
        file_id.get(),
        range.end_exclusive(),
        snapshot.hunk_count(),
    )?;
    if !range.contains(hunk_id) {
        return Err(StructureStoreError::ParentRangeMismatch {
            parent_kind: "file",
            parent_id: file_id.get(),
            child_kind: "hunk",
            child_id: hunk_id.get(),
        });
    }
    Ok(())
}

fn validate_supplied_commit_parent(
    snapshot: &StructureSnapshot,
    file_id: FileId,
    commit_id: CommitId,
    parent: StoredCommitMetadata,
) -> Result<(), StructureStoreError> {
    let parent_is_visible = if parent.id.get() < snapshot.commit_high_water {
        parent.finalized
    } else {
        parent.id.get() == snapshot.commit_high_water
            && !parent.finalized
            && snapshot
                .open_commit
                .as_ref()
                .is_some_and(|commit| commit.id == parent.id)
    };
    if parent.id != commit_id || !parent_is_visible {
        return Err(StructureStoreError::InvalidParent {
            child_kind: "file",
            child_id: file_id.get(),
            parent_kind: "commit",
            parent_id: commit_id.get(),
            parent_count: snapshot.commit_count(),
        });
    }
    validate_child_high_water(
        "commit",
        parent.id.get(),
        parent.files.end_exclusive(),
        snapshot.file_count(),
    )?;
    if !parent.files.contains(file_id) {
        return Err(StructureStoreError::ParentRangeMismatch {
            parent_kind: "commit",
            parent_id: parent.id.get(),
            child_kind: "file",
            child_id: file_id.get(),
        });
    }
    Ok(())
}

fn validate_supplied_file_parent(
    snapshot: &StructureSnapshot,
    hunk_id: HunkId,
    file_id: FileId,
    parent: StoredFileMetadata,
) -> Result<(), StructureStoreError> {
    let parent_is_visible = if parent.id.get() < snapshot.file_high_water {
        parent.finalized
    } else {
        parent.id.get() == snapshot.file_high_water
            && !parent.finalized
            && snapshot
                .open_file
                .as_ref()
                .is_some_and(|file| file.id == parent.id)
    };
    if parent.id != file_id || !parent_is_visible {
        return Err(StructureStoreError::InvalidParent {
            child_kind: "hunk",
            child_id: hunk_id.get(),
            parent_kind: "file",
            parent_id: file_id.get(),
            parent_count: snapshot.file_count(),
        });
    }
    validate_child_high_water(
        "file",
        parent.id.get(),
        parent.hunks.end_exclusive(),
        snapshot.hunk_count(),
    )?;
    if !parent.hunks.contains(hunk_id) {
        return Err(StructureStoreError::ParentRangeMismatch {
            parent_kind: "file",
            parent_id: parent.id.get(),
            child_kind: "hunk",
            child_id: hunk_id.get(),
        });
    }
    Ok(())
}

fn append_optional_blob(
    arena: &BlobArena,
    value: Option<&str>,
) -> Result<PackedBlobRef, StructureStoreError> {
    let reference = value
        .map(|text| arena.append(text.as_bytes()))
        .transpose()?;
    PackedBlobRef::from_blob(reference)
        .map_err(StorageError::from)
        .map_err(Into::into)
}

fn read_optional_blob(
    arena: &BlobArena,
    packed: PackedBlobRef,
    output: &mut Vec<u8>,
) -> Result<(), StructureStoreError> {
    output.clear();
    if let Some(reference) = packed.bind(arena)? {
        arena.read_into(reference, output)?;
    }
    Ok(())
}

fn optional_utf8<'a>(
    packed: PackedBlobRef,
    bytes: &'a [u8],
    field: &'static str,
) -> Result<Option<&'a str>, StructureStoreError> {
    if !packed.is_some() {
        return Ok(None);
    }
    str::from_utf8(bytes)
        .map(Some)
        .map_err(|source| StructureStoreError::InvalidUtf8 { field, source })
}

fn open_child(
    operation: &'static str,
    open_kind: &'static str,
    open_id: u64,
) -> StructureStoreError {
    StructureStoreError::OpenChild {
        operation,
        open_kind,
        open_id,
    }
}

fn validate_child_start(
    parent_kind: &'static str,
    parent_id: u64,
    parent_start: u64,
    child_kind: &'static str,
    child_id: u64,
    child_start: u64,
) -> Result<(), StructureStoreError> {
    if child_start >= parent_start {
        Ok(())
    } else {
        Err(StructureStoreError::ChildStartsBeforeParent {
            child_kind,
            child_id,
            child_start,
            parent_kind,
            parent_id,
            parent_start,
        })
    }
}

fn validate_sibling_start(
    kind: &'static str,
    id: u64,
    start: u64,
    previous: Option<FinalizedSpanEnd>,
) -> Result<(), StructureStoreError> {
    let Some(previous) = previous else {
        return Ok(());
    };
    if start >= previous.end_exclusive {
        Ok(())
    } else {
        Err(StructureStoreError::SiblingSpanOverlap {
            kind,
            id,
            start,
            previous_id: previous.id,
            previous_end: previous.end_exclusive,
        })
    }
}

fn validate_last_child_end(
    parent_kind: &'static str,
    parent_id: u64,
    parent_end: u64,
    child_kind: &'static str,
    expected_child: Option<u64>,
    last_child: Option<FinalizedSpanEnd>,
) -> Result<(), StructureStoreError> {
    let Some(expected_child) = expected_child else {
        return Ok(());
    };
    let last_child = last_child
        .filter(|child| child.id == expected_child)
        .ok_or(StructureStoreError::ParentRangeMismatch {
            parent_kind,
            parent_id,
            child_kind,
            child_id: expected_child,
        })?;
    if last_child.end_exclusive <= parent_end {
        Ok(())
    } else {
        Err(StructureStoreError::ChildEndsAfterParent {
            parent_kind,
            parent_id,
            parent_end,
            child_kind,
            child_id: last_child.id,
            child_end: last_child.end_exclusive,
        })
    }
}

fn validate_id(kind: &'static str, id: u64) -> Result<(), StructureStoreError> {
    if id == OPTIONAL_U64_NONE {
        Err(StructureStoreError::ReservedId { kind, id })
    } else {
        Ok(())
    }
}

fn validate_optional_u64(
    field: &'static str,
    value: Option<u64>,
) -> Result<(), StructureStoreError> {
    if value == Some(OPTIONAL_U64_NONE) {
        Err(StructureStoreError::ReservedOptionalValue {
            field,
            value: OPTIONAL_U64_NONE,
        })
    } else {
        Ok(())
    }
}

fn validate_file_counts(
    file_id: u64,
    mode: FileMode,
    additions: Option<u64>,
    deletions: Option<u64>,
) -> Result<(), StructureStoreError> {
    let valid = if mode.is_rich() {
        additions.is_some() && deletions.is_some()
    } else {
        additions.is_none() && deletions.is_none()
    };
    if valid {
        Ok(())
    } else {
        Err(StructureStoreError::FileCountModeMismatch { file_id, mode })
    }
}

fn validate_hunk_ranges(
    hunk_id: u64,
    kind: HunkKind,
    old_range: Option<SourceRange>,
    new_range: Option<SourceRange>,
) -> Result<(), StructureStoreError> {
    let valid = if kind == HunkKind::Unified {
        old_range.is_some() && new_range.is_some()
    } else {
        old_range.is_none() && new_range.is_none()
    };
    if valid {
        Ok(())
    } else {
        Err(StructureStoreError::HunkRangeKindMismatch { hunk_id, kind })
    }
}

fn checked_range_end(
    kind: &'static str,
    first: u64,
    count: u64,
) -> Result<u64, StructureStoreError> {
    first
        .checked_add(count)
        .ok_or(StructureStoreError::IdRangeOverflow { kind, first, count })
}

fn visible_count(
    finalized: u64,
    has_open: bool,
    kind: &'static str,
) -> Result<u64, StructureStoreError> {
    finalized
        .checked_add(u64::from(has_open))
        .ok_or(StructureStoreError::IdRangeOverflow {
            kind,
            first: finalized,
            count: u64::from(has_open),
        })
}

fn validate_file_range(range: FileIdRange) -> Result<(), StructureStoreError> {
    validate_id("file", range.start)?;
    let _ = checked_range_end("file", range.start, range.count)?;
    Ok(())
}

fn validate_hunk_range(range: HunkIdRange) -> Result<(), StructureStoreError> {
    validate_id("hunk", range.start)?;
    let _ = checked_range_end("hunk", range.start, range.count)?;
    Ok(())
}

fn validate_identity(
    kind: &'static str,
    expected: u64,
    actual: u64,
) -> Result<(), StructureStoreError> {
    if expected == actual {
        Ok(())
    } else {
        Err(StructureStoreError::IdentityMismatch {
            kind,
            expected,
            actual,
        })
    }
}

fn validate_append_identity(
    kind: &'static str,
    expected: u64,
    actual: u64,
) -> Result<(), StructureStoreError> {
    if expected == actual {
        Ok(())
    } else {
        Err(StructureStoreError::AppendIdentityMismatch {
            kind,
            expected,
            actual,
        })
    }
}

fn validate_child_high_water(
    kind: &'static str,
    id: u64,
    end: u64,
    high_water: u64,
) -> Result<(), StructureStoreError> {
    if end <= high_water {
        Ok(())
    } else {
        Err(StructureStoreError::ChildRangeOutsideSnapshot {
            kind,
            id,
            end,
            high_water,
        })
    }
}

fn validate_packed_blob(value: PackedBlobRef) -> Result<(), RecordCodecError> {
    match value.tag {
        BLOB_NONE | BLOB_EMPTY if value.location == 0 && value.len == 0 => Ok(()),
        BLOB_MEMORY | BLOB_DISK if value.len > 0 => Ok(()),
        BLOB_NONE | BLOB_EMPTY | BLOB_MEMORY | BLOB_DISK => {
            Err(RecordCodecError::new("inconsistent packed blob sentinel"))
        }
        _ => Err(RecordCodecError::new("unknown blob tag")),
    }
}

fn validate_id_codec(value: u64, message: &'static str) -> Result<(), RecordCodecError> {
    if value == OPTIONAL_U64_NONE {
        Err(RecordCodecError::new(message))
    } else {
        Ok(())
    }
}

fn validate_optional_codec(
    value: Option<u64>,
    message: &'static str,
) -> Result<(), RecordCodecError> {
    if value == Some(OPTIONAL_U64_NONE) {
        Err(RecordCodecError::new(message))
    } else {
        Ok(())
    }
}

fn validate_file_counts_codec(
    mode: FileMode,
    additions: Option<u64>,
    deletions: Option<u64>,
) -> Result<(), RecordCodecError> {
    let valid = if mode.is_rich() {
        additions.is_some() && deletions.is_some()
    } else {
        additions.is_none() && deletions.is_none()
    };
    if valid {
        Ok(())
    } else {
        Err(RecordCodecError::new(
            "file mode has incompatible addition/deletion counts",
        ))
    }
}

fn validate_hunk_ranges_codec(
    kind: HunkKind,
    old_range: Option<SourceRange>,
    new_range: Option<SourceRange>,
) -> Result<(), RecordCodecError> {
    let valid = if kind == HunkKind::Unified {
        old_range.is_some() && new_range.is_some()
    } else {
        old_range.is_none() && new_range.is_none()
    };
    if valid {
        Ok(())
    } else {
        Err(RecordCodecError::new(
            "hunk kind has incompatible source ranges",
        ))
    }
}

fn validate_closed_span_codec(span: ClosedLineSpan) -> Result<(), RecordCodecError> {
    validate_id_codec(span.start, "line start uses reserved sentinel")?;
    if span.end_exclusive <= span.start {
        return Err(RecordCodecError::new(
            "closed line span must contain at least one line",
        ));
    }
    Ok(())
}

fn validate_file_range_codec(range: FileIdRange) -> Result<(), RecordCodecError> {
    validate_encoded_range(range.start, range.count, "invalid file child range")
}

fn validate_hunk_range_codec(range: HunkIdRange) -> Result<(), RecordCodecError> {
    validate_encoded_range(range.start, range.count, "invalid hunk child range")
}

fn validate_encoded_range(
    start: u64,
    count: u64,
    message: &'static str,
) -> Result<(), RecordCodecError> {
    if start != OPTIONAL_U64_NONE && start.checked_add(count).is_some() {
        Ok(())
    } else {
        Err(RecordCodecError::new(message))
    }
}

fn encode_file_range(
    range: FileIdRange,
    output: &mut [u8],
    offset: usize,
) -> Result<(), RecordCodecError> {
    validate_file_range_codec(range)?;
    put_u64(output, offset, range.start)?;
    put_u64(output, offset + 8, range.count)
}

fn decode_file_range(input: &[u8], offset: usize) -> Result<FileIdRange, RecordCodecError> {
    let range = FileIdRange {
        start: get_u64(input, offset)?,
        count: get_u64(input, offset + 8)?,
    };
    validate_file_range_codec(range)?;
    Ok(range)
}

fn encode_hunk_range(
    range: HunkIdRange,
    output: &mut [u8],
    offset: usize,
) -> Result<(), RecordCodecError> {
    validate_hunk_range_codec(range)?;
    put_u64(output, offset, range.start)?;
    put_u64(output, offset + 8, range.count)
}

fn decode_hunk_range(input: &[u8], offset: usize) -> Result<HunkIdRange, RecordCodecError> {
    let range = HunkIdRange {
        start: get_u64(input, offset)?,
        count: get_u64(input, offset + 8)?,
    };
    validate_hunk_range_codec(range)?;
    Ok(range)
}

const fn encode_optional_u64(value: Option<u64>) -> u64 {
    match value {
        Some(value) => value,
        None => OPTIONAL_U64_NONE,
    }
}

const fn decode_optional_u64(value: u64) -> Option<u64> {
    if value == OPTIONAL_U64_NONE {
        None
    } else {
        Some(value)
    }
}

fn decode_source_range(
    present: bool,
    values: (u64, u64),
) -> Result<Option<SourceRange>, RecordCodecError> {
    if present {
        Ok(Some(SourceRange {
            start: values.0,
            count: values.1,
        }))
    } else if values == (0, 0) {
        Ok(None)
    } else {
        Err(RecordCodecError::new(
            "absent source range has non-zero payload",
        ))
    }
}

fn require_width(
    bytes: &[u8],
    expected: usize,
    message: &'static str,
) -> Result<(), RecordCodecError> {
    if bytes.len() == expected {
        Ok(())
    } else {
        Err(RecordCodecError::new(message))
    }
}

fn require_zero(bytes: &[u8], message: &'static str) -> Result<(), RecordCodecError> {
    if bytes.iter().all(|byte| *byte == 0) {
        Ok(())
    } else {
        Err(RecordCodecError::new(message))
    }
}

fn put_u64(output: &mut [u8], offset: usize, value: u64) -> Result<(), RecordCodecError> {
    let destination = output
        .get_mut(offset..offset + 8)
        .ok_or(RecordCodecError::new("record field offset exceeds width"))?;
    destination.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn get_u64(input: &[u8], offset: usize) -> Result<u64, RecordCodecError> {
    let source = input
        .get(offset..offset + 8)
        .ok_or(RecordCodecError::new("record field offset exceeds width"))?;
    let bytes: [u8; 8] = source
        .try_into()
        .map_err(|_| RecordCodecError::new("invalid u64 field width"))?;
    Ok(u64::from_le_bytes(bytes))
}

const fn encode_commit_kind(value: CommitKind) -> u8 {
    match value {
        CommitKind::Commit => 0,
        CommitKind::WorkingTree => 1,
    }
}

fn decode_commit_kind(value: u8) -> Result<CommitKind, RecordCodecError> {
    match value {
        0 => Ok(CommitKind::Commit),
        1 => Ok(CommitKind::WorkingTree),
        _ => Err(RecordCodecError::new("unknown commit kind")),
    }
}

const fn encode_change_status(value: ChangeStatus) -> u8 {
    match value {
        ChangeStatus::Modified => 0,
        ChangeStatus::Added => 1,
        ChangeStatus::Deleted => 2,
        ChangeStatus::Renamed => 3,
        ChangeStatus::Copied => 4,
        ChangeStatus::Binary => 5,
        ChangeStatus::Submodule => 6,
        ChangeStatus::Unknown => 7,
    }
}

fn decode_change_status(value: u8) -> Result<ChangeStatus, RecordCodecError> {
    match value {
        0 => Ok(ChangeStatus::Modified),
        1 => Ok(ChangeStatus::Added),
        2 => Ok(ChangeStatus::Deleted),
        3 => Ok(ChangeStatus::Renamed),
        4 => Ok(ChangeStatus::Copied),
        5 => Ok(ChangeStatus::Binary),
        6 => Ok(ChangeStatus::Submodule),
        7 => Ok(ChangeStatus::Unknown),
        _ => Err(RecordCodecError::new("unknown change status")),
    }
}

const fn encode_file_mode(value: FileMode) -> u8 {
    match value {
        FileMode::Rich => 0,
        FileMode::Combined => 1,
        FileMode::WordDiff => 2,
        FileMode::Submodule => 3,
        FileMode::Binary => 4,
        FileMode::Malformed => 5,
    }
}

fn decode_file_mode(value: u8) -> Result<FileMode, RecordCodecError> {
    match value {
        0 => Ok(FileMode::Rich),
        1 => Ok(FileMode::Combined),
        2 => Ok(FileMode::WordDiff),
        3 => Ok(FileMode::Submodule),
        4 => Ok(FileMode::Binary),
        5 => Ok(FileMode::Malformed),
        _ => Err(RecordCodecError::new("unknown file mode")),
    }
}

const fn encode_hunk_kind(value: HunkKind) -> u8 {
    match value {
        HunkKind::Unified => 0,
        HunkKind::Combined => 1,
        HunkKind::WordDiff => 2,
        HunkKind::Submodule => 3,
        HunkKind::Malformed => 4,
    }
}

fn decode_hunk_kind(value: u8) -> Result<HunkKind, RecordCodecError> {
    match value {
        0 => Ok(HunkKind::Unified),
        1 => Ok(HunkKind::Combined),
        2 => Ok(HunkKind::WordDiff),
        3 => Ok(HunkKind::Submodule),
        4 => Ok(HunkKind::Malformed),
        _ => Err(RecordCodecError::new("unknown hunk kind")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{STRUCTURAL_RESIDENT_BYTES, StorageConfig};

    fn line(value: u64) -> LineId {
        LineId::new(value)
    }

    fn begin_sample_tree(
        builder: &mut StructureBuilder,
    ) -> Result<(CommitId, FileId, HunkId), StructureStoreError> {
        let commit = builder.begin_commit(
            CommitKind::Commit,
            Some("0123456789abcdef".to_owned()),
            line(0),
        )?;
        builder.open_commit_mut().unwrap().subject = Some("subject".to_owned());
        let file = builder.begin_file(
            commit,
            Some("src/old.rs".to_owned()),
            Some("src/new.rs".to_owned()),
            ChangeStatus::Renamed,
            FileMode::Rich,
            line(1),
        )?;
        builder
            .open_file_mut()
            .unwrap()
            .line_bounds
            .observe(Some(12), Some(14));
        let hunk = builder.begin_hunk(
            file,
            HunkKind::Unified,
            line(2),
            Some(SourceRange {
                start: 10,
                count: 3,
            }),
            Some(SourceRange {
                start: 10,
                count: 5,
            }),
        )?;
        Ok((commit, file, hunk))
    }

    #[test]
    fn open_overlays_and_finalized_records_share_typed_views() {
        let storage = StructuralStorage::new().unwrap();
        let mut builder = StructureBuilder::new(&storage).unwrap();
        let (commit_id, file_id, hunk_id) = begin_sample_tree(&mut builder).unwrap();

        let open = builder.snapshot().unwrap();
        assert_eq!(open.finalized_commit_count(), 0);
        assert_eq!(open.commit_count(), 1);
        assert_eq!(open.file_count(), 1);
        assert_eq!(open.hunk_count(), 1);

        let mut reader = StructureReader::new();
        let commit = reader.read_commit(&open, commit_id).unwrap().unwrap();
        assert_eq!(commit.object_id, Some("0123456789abcdef"));
        assert_eq!(commit.subject, Some("subject"));
        assert!(!commit.finalized);
        assert_eq!(commit.files.first(), Some(file_id));
        assert_eq!(commit.files.count(), 1);

        let file = reader.read_file(&open, file_id).unwrap().unwrap();
        assert_eq!(file.primary_path(), Some("src/new.rs"));
        assert_eq!(file.hunks.first(), Some(hunk_id));
        assert!(!file.finalized);
        assert_eq!(
            file.line_bounds,
            StructureFileLineBounds {
                max_old_line_number: Some(12),
                max_new_line_number: Some(14),
            }
        );
        let hunk = reader.read_hunk(&open, hunk_id).unwrap().unwrap();
        assert_eq!(
            hunk.old_range,
            Some(SourceRange {
                start: 10,
                count: 3
            })
        );
        assert!(!hunk.finalized);

        builder.finish_hunk(6).unwrap();
        builder.finish_file(7).unwrap();
        builder.finish_commit(8).unwrap();
        let finalized = builder.snapshot().unwrap();
        assert_eq!(finalized.finalized_commit_count(), 1);
        assert_eq!(finalized.finalized_file_count(), 1);
        assert_eq!(finalized.finalized_hunk_count(), 1);
        assert!(finalized.open_commit().is_none());

        let commit = reader.read_commit(&finalized, commit_id).unwrap().unwrap();
        assert_eq!(commit.span.end_exclusive, Some(8));
        assert!(commit.finalized);
        let file = reader.read_file(&finalized, file_id).unwrap().unwrap();
        assert_eq!(file.old_path, Some("src/old.rs"));
        assert_eq!(file.new_path, Some("src/new.rs"));
        assert_eq!(file.status, ChangeStatus::Renamed);
        assert_eq!(
            file.line_bounds,
            StructureFileLineBounds {
                max_old_line_number: Some(12),
                max_new_line_number: Some(14),
            }
        );
        assert!(file.finalized);
        let hunk = reader.read_hunk(&finalized, hunk_id).unwrap().unwrap();
        assert_eq!(hunk.span.end_exclusive, Some(6));
        assert!(hunk.finalized);
    }

    #[test]
    fn metadata_reader_tracks_open_and_finalized_records_without_strings() {
        let storage = StructuralStorage::new().unwrap();
        let mut builder = StructureBuilder::new(&storage).unwrap();
        let (commit_id, file_id, hunk_id) = begin_sample_tree(&mut builder).unwrap();

        let open = builder.snapshot().unwrap();
        let mut reader = StructureMetadataReader::new();
        let commit = reader.read_commit(&open, commit_id).unwrap().unwrap();
        assert_eq!(commit.files.first(), Some(file_id));
        assert!(!commit.finalized);
        let file = reader.read_file(&open, file_id).unwrap().unwrap();
        assert_eq!(file.commit_id, commit_id);
        assert_eq!(file.hunks.first(), Some(hunk_id));
        assert_eq!(file.mode, FileMode::Rich);
        assert!(!file.finalized);
        assert_eq!(
            file.line_bounds,
            StructureFileLineBounds {
                max_old_line_number: Some(12),
                max_new_line_number: Some(14),
            }
        );
        let hunk = reader.read_hunk(&open, hunk_id).unwrap().unwrap();
        assert_eq!(hunk.file_id, file_id);
        assert!(!hunk.finalized);

        builder.finish_hunk(6).unwrap();
        builder.finish_file(6).unwrap();
        builder.finish_commit(6).unwrap();
        let finalized = builder.snapshot().unwrap();
        assert!(
            reader
                .read_commit(&finalized, commit_id)
                .unwrap()
                .unwrap()
                .finalized
        );
        assert!(
            reader
                .read_file(&finalized, file_id)
                .unwrap()
                .unwrap()
                .finalized
        );
        assert!(
            reader
                .read_hunk(&finalized, hunk_id)
                .unwrap()
                .unwrap()
                .finalized
        );
    }

    #[test]
    fn metadata_reader_does_not_bind_or_read_path_blobs() {
        let storage = StructuralStorage::new().unwrap();
        let commits = storage.table::<CommitRecord>().unwrap();
        let files = storage.table::<FileRecord>().unwrap();
        let hunks = storage.table::<HunkRecord>().unwrap();
        commits
            .append(&CommitRecord {
                id: 0,
                kind: CommitKind::WorkingTree,
                span: ClosedLineSpan {
                    start: 0,
                    end_exclusive: 1,
                },
                files: FileIdRange { start: 0, count: 1 },
                object_id: PackedBlobRef::NONE,
                subject: PackedBlobRef::NONE,
            })
            .unwrap();
        files
            .append(&FileRecord {
                id: 0,
                commit_id: 0,
                span: ClosedLineSpan {
                    start: 0,
                    end_exclusive: 1,
                },
                hunks: HunkIdRange::empty_at(0),
                additions: None,
                deletions: None,
                // Arenaにはpage 999がない。Metadata readerがこのfieldへ触れればreadは失敗する。
                old_path: PackedBlobRef {
                    tag: BLOB_MEMORY,
                    location: u64::from(999_u32) << 32,
                    len: 1,
                },
                new_path: PackedBlobRef::NONE,
                status: ChangeStatus::Unknown,
                mode: FileMode::Malformed,
                line_bounds: StructureFileLineBounds::default(),
            })
            .unwrap();
        let snapshot = StructureSnapshot {
            commits,
            files,
            hunks,
            blobs: storage.blob_arena(),
            commit_high_water: 1,
            file_high_water: 1,
            hunk_high_water: 0,
            open_commit: None,
            open_file: None,
            open_hunk: None,
        };

        let metadata = StructureMetadataReader::new()
            .read_file(&snapshot, FileId::new(0))
            .unwrap()
            .unwrap();
        assert_eq!(metadata.mode, FileMode::Malformed);
        assert!(
            StructureReader::new()
                .read_file(&snapshot, FileId::new(0))
                .is_err()
        );
    }

    #[test]
    fn snapshot_high_water_does_not_expose_later_records() {
        let storage = StructuralStorage::new().unwrap();
        let mut builder = StructureBuilder::new(&storage).unwrap();
        let first = builder
            .begin_commit(CommitKind::WorkingTree, None, line(0))
            .unwrap();
        builder.finish_commit(1).unwrap();
        let frozen = builder.snapshot().unwrap();

        let second = builder
            .begin_commit(CommitKind::WorkingTree, None, line(1))
            .unwrap();
        builder.finish_commit(2).unwrap();
        let current = builder.snapshot().unwrap();

        assert_eq!(first.get(), 0);
        assert_eq!(second.get(), 1);
        assert_eq!(frozen.commit_count(), 1);
        assert_eq!(current.commit_count(), 2);
        let mut reader = StructureReader::new();
        assert!(reader.read_commit(&frozen, second).unwrap().is_none());
        assert!(reader.read_commit(&current, second).unwrap().is_some());
    }

    #[test]
    fn begin_operations_require_exact_open_parent_ids() {
        let storage = StructuralStorage::new().unwrap();
        let mut builder = StructureBuilder::new(&storage).unwrap();
        let commit = builder
            .begin_commit(CommitKind::WorkingTree, None, line(0))
            .unwrap();
        let error = builder
            .begin_file(
                CommitId::new(commit.get() + 1),
                None,
                None,
                ChangeStatus::Unknown,
                FileMode::Malformed,
                line(0),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            StructureStoreError::OpenParentMismatch { kind: "commit", .. }
        ));
        let file = builder
            .begin_file(
                commit,
                None,
                None,
                ChangeStatus::Unknown,
                FileMode::Malformed,
                line(0),
            )
            .unwrap();
        let error = builder
            .begin_hunk(
                FileId::new(file.get() + 1),
                HunkKind::Malformed,
                line(0),
                None,
                None,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            StructureStoreError::OpenParentMismatch { kind: "file", .. }
        ));
        let error = builder
            .begin_hunk(file, HunkKind::Unified, line(0), None, None)
            .unwrap_err();
        assert!(matches!(
            error,
            StructureStoreError::HunkRangeKindMismatch { hunk_id: 0, .. }
        ));
        assert_eq!(builder.snapshot().unwrap().hunk_count(), 0);
    }

    #[test]
    fn file_cannot_start_before_its_open_commit() {
        let storage = StructuralStorage::new().unwrap();
        let mut builder = StructureBuilder::new(&storage).unwrap();
        let commit = builder
            .begin_commit(CommitKind::WorkingTree, None, line(10))
            .unwrap();

        let error = builder
            .begin_file(
                commit,
                None,
                None,
                ChangeStatus::Unknown,
                FileMode::Malformed,
                line(9),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            StructureStoreError::ChildStartsBeforeParent {
                parent_kind: "commit",
                child_kind: "file",
                parent_start: 10,
                child_start: 9,
                ..
            }
        ));
        assert_eq!(builder.open_commit_mut().unwrap().files.count(), 0);
    }

    #[test]
    fn hunk_cannot_start_before_its_open_file() {
        let storage = StructuralStorage::new().unwrap();
        let mut builder = StructureBuilder::new(&storage).unwrap();
        let commit = builder
            .begin_commit(CommitKind::WorkingTree, None, line(0))
            .unwrap();
        let file = builder
            .begin_file(
                commit,
                None,
                None,
                ChangeStatus::Unknown,
                FileMode::Malformed,
                line(10),
            )
            .unwrap();

        let error = builder
            .begin_hunk(file, HunkKind::Malformed, line(9), None, None)
            .unwrap_err();

        assert!(matches!(
            error,
            StructureStoreError::ChildStartsBeforeParent {
                parent_kind: "file",
                child_kind: "hunk",
                parent_start: 10,
                child_start: 9,
                ..
            }
        ));
        assert_eq!(builder.open_file_mut().unwrap().hunks.count(), 0);
    }

    #[test]
    fn parent_close_rejects_a_finalized_child_that_ends_later() {
        let storage = StructuralStorage::new().unwrap();
        let mut builder = StructureBuilder::new(&storage).unwrap();
        let commit = builder
            .begin_commit(CommitKind::WorkingTree, None, line(0))
            .unwrap();
        let file = builder
            .begin_file(
                commit,
                None,
                None,
                ChangeStatus::Unknown,
                FileMode::Malformed,
                line(1),
            )
            .unwrap();
        builder
            .begin_hunk(file, HunkKind::Malformed, line(2), None, None)
            .unwrap();
        builder.finish_hunk(6).unwrap();

        let file_error = builder.finish_file(5).unwrap_err();
        assert!(matches!(
            file_error,
            StructureStoreError::ChildEndsAfterParent {
                parent_kind: "file",
                parent_end: 5,
                child_kind: "hunk",
                child_end: 6,
                ..
            }
        ));
        builder.finish_file(6).unwrap();

        let commit_error = builder.finish_commit(5).unwrap_err();
        assert!(matches!(
            commit_error,
            StructureStoreError::ChildEndsAfterParent {
                parent_kind: "commit",
                parent_end: 5,
                child_kind: "file",
                child_end: 6,
                ..
            }
        ));
        builder.finish_commit(6).unwrap();
    }

    #[test]
    fn sibling_spans_are_monotonic_and_non_overlapping() {
        let storage = StructuralStorage::new().unwrap();
        let mut commits = StructureBuilder::new(&storage).unwrap();
        commits
            .begin_commit(CommitKind::WorkingTree, None, line(0))
            .unwrap();
        commits.finish_commit(5).unwrap();
        let error = commits
            .begin_commit(CommitKind::WorkingTree, None, line(4))
            .unwrap_err();
        assert!(matches!(
            error,
            StructureStoreError::SiblingSpanOverlap {
                kind: "commit",
                previous_end: 5,
                start: 4,
                ..
            }
        ));
        commits
            .begin_commit(CommitKind::WorkingTree, None, line(5))
            .unwrap();

        let storage = StructuralStorage::new().unwrap();
        let mut files = StructureBuilder::new(&storage).unwrap();
        let commit = files
            .begin_commit(CommitKind::WorkingTree, None, line(0))
            .unwrap();
        files
            .begin_file(
                commit,
                None,
                None,
                ChangeStatus::Unknown,
                FileMode::Malformed,
                line(1),
            )
            .unwrap();
        files.finish_file(5).unwrap();
        let error = files
            .begin_file(
                commit,
                None,
                None,
                ChangeStatus::Unknown,
                FileMode::Malformed,
                line(4),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            StructureStoreError::SiblingSpanOverlap {
                kind: "file",
                previous_end: 5,
                start: 4,
                ..
            }
        ));
        files
            .begin_file(
                commit,
                None,
                None,
                ChangeStatus::Unknown,
                FileMode::Malformed,
                line(5),
            )
            .unwrap();

        let storage = StructuralStorage::new().unwrap();
        let mut hunks = StructureBuilder::new(&storage).unwrap();
        let commit = hunks
            .begin_commit(CommitKind::WorkingTree, None, line(0))
            .unwrap();
        let file = hunks
            .begin_file(
                commit,
                None,
                None,
                ChangeStatus::Unknown,
                FileMode::Malformed,
                line(1),
            )
            .unwrap();
        hunks
            .begin_hunk(file, HunkKind::Malformed, line(2), None, None)
            .unwrap();
        hunks.finish_hunk(5).unwrap();
        let error = hunks
            .begin_hunk(file, HunkKind::Malformed, line(4), None, None)
            .unwrap_err();
        assert!(matches!(
            error,
            StructureStoreError::SiblingSpanOverlap {
                kind: "hunk",
                previous_end: 5,
                start: 4,
                ..
            }
        ));
        hunks
            .begin_hunk(file, HunkKind::Malformed, line(5), None, None)
            .unwrap();
    }

    #[test]
    fn empty_string_and_absent_string_remain_distinct() {
        let storage = StructuralStorage::new().unwrap();
        let mut builder = StructureBuilder::new(&storage).unwrap();
        let commit = builder
            .begin_commit(CommitKind::Commit, Some(String::new()), line(0))
            .unwrap();
        builder.open_commit_mut().unwrap().subject = None;
        builder.finish_commit(1).unwrap();
        let snapshot = builder.snapshot().unwrap();
        let mut reader = StructureReader::new();
        let commit = reader.read_commit(&snapshot, commit).unwrap().unwrap();
        assert_eq!(commit.object_id, Some(""));
        assert_eq!(commit.subject, None);
    }

    #[test]
    fn invalid_overlay_value_can_be_fixed_before_any_append() {
        let storage = StructuralStorage::new().unwrap();
        let mut builder = StructureBuilder::new(&storage).unwrap();
        let commit = builder
            .begin_commit(CommitKind::WorkingTree, None, line(0))
            .unwrap();
        builder
            .begin_file(
                commit,
                Some("old.rs".to_owned()),
                Some("new.rs".to_owned()),
                ChangeStatus::Modified,
                FileMode::Rich,
                line(0),
            )
            .unwrap();
        builder.open_file_mut().unwrap().additions = Some(OPTIONAL_U64_NONE);
        let error = builder.finish_file(1).unwrap_err();
        assert!(matches!(
            error,
            StructureStoreError::ReservedOptionalValue {
                field: "additions",
                ..
            }
        ));

        builder.open_file_mut().unwrap().additions = Some(1);
        builder.finish_file(1).unwrap();
        builder.finish_commit(1).unwrap();
        assert_eq!(builder.snapshot().unwrap().file_count(), 1);
    }

    #[test]
    fn zero_length_close_is_retryable_for_every_structure_kind() {
        let storage = StructuralStorage::new().unwrap();
        let mut commit_builder = StructureBuilder::new(&storage).unwrap();
        let commit = commit_builder
            .begin_commit(CommitKind::WorkingTree, None, line(5))
            .unwrap();
        let error = commit_builder.finish_commit(5).unwrap_err();
        assert!(matches!(
            error,
            StructureStoreError::InvalidLineSpan { start: 5, end: 5 }
        ));
        assert_eq!(
            commit_builder.open_commit().map(OpenCommit::id),
            Some(commit)
        );
        assert!(commit_builder.snapshot().is_ok());
        assert_eq!(commit_builder.finish_commit(6).unwrap(), commit);

        let storage = StructuralStorage::new().unwrap();
        let mut file_builder = StructureBuilder::new(&storage).unwrap();
        let commit = file_builder
            .begin_commit(CommitKind::WorkingTree, None, line(0))
            .unwrap();
        let file = file_builder
            .begin_file(
                commit,
                None,
                None,
                ChangeStatus::Unknown,
                FileMode::Malformed,
                line(2),
            )
            .unwrap();
        let error = file_builder.finish_file(2).unwrap_err();
        assert!(matches!(
            error,
            StructureStoreError::InvalidLineSpan { start: 2, end: 2 }
        ));
        assert_eq!(file_builder.open_file().map(OpenFile::id), Some(file));
        assert!(file_builder.snapshot().is_ok());
        assert_eq!(file_builder.finish_file(3).unwrap(), file);
        file_builder.finish_commit(4).unwrap();

        let storage = StructuralStorage::new().unwrap();
        let mut hunk_builder = StructureBuilder::new(&storage).unwrap();
        let commit = hunk_builder
            .begin_commit(CommitKind::WorkingTree, None, line(0))
            .unwrap();
        let file = hunk_builder
            .begin_file(
                commit,
                None,
                None,
                ChangeStatus::Unknown,
                FileMode::Malformed,
                line(1),
            )
            .unwrap();
        let hunk = hunk_builder
            .begin_hunk(file, HunkKind::Malformed, line(2), None, None)
            .unwrap();
        let error = hunk_builder.finish_hunk(2).unwrap_err();
        assert!(matches!(
            error,
            StructureStoreError::InvalidLineSpan { start: 2, end: 2 }
        ));
        assert_eq!(hunk_builder.open_hunk().map(OpenHunk::id), Some(hunk));
        assert!(hunk_builder.snapshot().is_ok());
        assert_eq!(hunk_builder.finish_hunk(3).unwrap(), hunk);
        hunk_builder.finish_file(4).unwrap();
        hunk_builder.finish_commit(5).unwrap();
    }

    #[test]
    fn snapshot_ordinal_mapping_obeys_generation_high_water_marks() {
        let storage = StructuralStorage::new().unwrap();
        let mut builder = StructureBuilder::new(&storage).unwrap();
        let empty = builder.snapshot().unwrap();
        assert_eq!(empty.commit_id_at(0), None);
        assert_eq!(empty.file_id_at(0), None);
        assert_eq!(empty.hunk_id_at(0), None);
        assert_eq!(empty.commit_id_at(u64::MAX), None);
        assert_eq!(empty.file_id_at(u64::MAX), None);
        assert_eq!(empty.hunk_id_at(u64::MAX), None);
        assert!(builder.open_commit().is_none());
        assert!(builder.open_file().is_none());
        assert!(builder.open_hunk().is_none());

        let commit0 = builder
            .begin_commit(CommitKind::WorkingTree, None, line(0))
            .unwrap();
        let file0 = builder
            .begin_file(
                commit0,
                None,
                None,
                ChangeStatus::Unknown,
                FileMode::Malformed,
                line(0),
            )
            .unwrap();
        let hunk0 = builder
            .begin_hunk(file0, HunkKind::Malformed, line(0), None, None)
            .unwrap();
        assert_eq!(builder.open_commit().map(OpenCommit::id), Some(commit0));
        assert_eq!(builder.open_file().map(OpenFile::id), Some(file0));
        assert_eq!(builder.open_hunk().map(OpenHunk::id), Some(hunk0));
        let first_open = builder.snapshot().unwrap();
        assert_eq!(first_open.commit_id_at(0), Some(commit0));
        assert_eq!(first_open.file_id_at(0), Some(file0));
        assert_eq!(first_open.hunk_id_at(0), Some(hunk0));
        assert_eq!(first_open.commit_id_at(1), None);
        assert_eq!(first_open.file_id_at(1), None);
        assert_eq!(first_open.hunk_id_at(1), None);

        builder.finish_hunk(1).unwrap();
        builder.finish_file(2).unwrap();
        builder.finish_commit(3).unwrap();
        let older = builder.snapshot().unwrap();
        assert_eq!(older.commit_id_at(0), Some(commit0));
        assert_eq!(older.file_id_at(0), Some(file0));
        assert_eq!(older.hunk_id_at(0), Some(hunk0));

        let commit1 = builder
            .begin_commit(CommitKind::WorkingTree, None, line(3))
            .unwrap();
        let file1 = builder
            .begin_file(
                commit1,
                None,
                None,
                ChangeStatus::Unknown,
                FileMode::Malformed,
                line(3),
            )
            .unwrap();
        let hunk1 = builder
            .begin_hunk(file1, HunkKind::Malformed, line(3), None, None)
            .unwrap();
        let current = builder.snapshot().unwrap();
        assert_eq!(current.commit_id_at(0), Some(commit0));
        assert_eq!(current.commit_id_at(1), Some(commit1));
        assert_eq!(current.file_id_at(1), Some(file1));
        assert_eq!(current.hunk_id_at(1), Some(hunk1));
        assert_eq!(current.commit_id_at(2), None);
        assert_eq!(current.file_id_at(2), None);
        assert_eq!(current.hunk_id_at(2), None);
        assert_eq!(current.commit_id_at(u64::MAX), None);
        assert_eq!(current.file_id_at(u64::MAX), None);
        assert_eq!(current.hunk_id_at(u64::MAX), None);
        assert_eq!(older.commit_id_at(1), None);
        assert_eq!(older.file_id_at(1), None);
        assert_eq!(older.hunk_id_at(1), None);
    }

    #[test]
    fn tiny_shared_budget_spills_records_and_path_blobs() {
        let limit = 512 * 1024;
        let config = StorageConfig::new(limit, 480 * 1024, 256, 4 * 1024).unwrap();
        let storage = StructuralStorage::with_config(config).unwrap();
        let mut builder = StructureBuilder::new(&storage).unwrap();
        let commit = builder
            .begin_commit(CommitKind::WorkingTree, None, line(0))
            .unwrap();
        for index in 0..800_u64 {
            let file = builder
                .begin_file(
                    commit,
                    Some(format!("old/{index:04}/a-long-path-name.rs")),
                    Some(format!("new/{index:04}/a-long-path-name.rs")),
                    ChangeStatus::Modified,
                    FileMode::Rich,
                    line(index),
                )
                .unwrap();
            let hunk = builder
                .begin_hunk(
                    file,
                    HunkKind::Unified,
                    line(index),
                    Some(SourceRange {
                        start: index,
                        count: 1,
                    }),
                    Some(SourceRange {
                        start: index,
                        count: 1,
                    }),
                )
                .unwrap();
            assert_eq!(hunk.get(), index);
            builder.finish_hunk(index + 1).unwrap();
            builder.finish_file(index + 1).unwrap();
        }
        builder.finish_commit(801).unwrap();

        let spill = builder.spill_status().unwrap();
        assert!(spill.files || spill.hunks || spill.commits);
        let snapshot = builder.snapshot().unwrap();
        let last = snapshot.files.get(799).unwrap().unwrap();
        assert_eq!(last.old_path.tag, BLOB_DISK);
        let resident = storage.resident_bytes().unwrap();
        assert!(resident.total <= limit);
        assert_eq!(resident.limit, limit);
    }

    #[test]
    fn shared_ledger_stays_bounded_for_many_files_and_paths() {
        let limit = 2 * 1024 * 1024;
        let config = StorageConfig::new(limit, 1_900 * 1024, 512, 64 * 1024).unwrap();
        let storage = StructuralStorage::with_config(config).unwrap();
        let mut builder = StructureBuilder::new(&storage).unwrap();
        let commit = builder
            .begin_commit(CommitKind::WorkingTree, None, line(0))
            .unwrap();
        for index in 0..5_000_u64 {
            builder
                .begin_file(
                    commit,
                    Some(format!("before/{index:05}/component/source-file.rs")),
                    Some(format!("after/{index:05}/component/source-file.rs")),
                    ChangeStatus::Modified,
                    FileMode::Rich,
                    line(index),
                )
                .unwrap();
            builder.finish_file(index + 1).unwrap();
        }
        builder.finish_commit(5_001).unwrap();
        let snapshot = builder.snapshot().unwrap();
        assert_eq!(snapshot.file_count(), 5_000);

        let resident = storage.resident_bytes().unwrap();
        assert!(resident.total <= limit);
        assert!(resident.total < STRUCTURAL_RESIDENT_BYTES);
        let mut reader = StructureReader::new();
        let last = reader
            .read_file(&snapshot, FileId::new(4_999))
            .unwrap()
            .unwrap();
        assert_eq!(last.new_path, Some("after/04999/component/source-file.rs"));
    }

    #[test]
    fn codecs_reject_unknown_discriminants_reserved_bytes_and_sentinels() {
        let record = CommitRecord {
            id: 0,
            kind: CommitKind::Commit,
            span: ClosedLineSpan {
                start: 0,
                end_exclusive: 1,
            },
            files: FileIdRange::empty_at(0),
            object_id: PackedBlobRef::NONE,
            subject: PackedBlobRef::NONE,
        };
        let mut encoded = vec![0; CommitRecord::ENCODED_LEN];
        record.encode(&mut encoded).unwrap();

        let mut unknown_kind = encoded.clone();
        unknown_kind[8] = 9;
        assert!(CommitRecord::decode(&unknown_kind).is_err());
        let mut reserved = encoded.clone();
        reserved[9] = 1;
        assert!(CommitRecord::decode(&reserved).is_err());
        let mut sentinel_id = encoded.clone();
        sentinel_id[0..8].copy_from_slice(&OPTIONAL_U64_NONE.to_le_bytes());
        assert!(CommitRecord::decode(&sentinel_id).is_err());
        let mut invalid_range = encoded.clone();
        invalid_range[32..40].copy_from_slice(&OPTIONAL_U64_NONE.to_le_bytes());
        invalid_range[40..48].copy_from_slice(&1_u64.to_le_bytes());
        assert!(CommitRecord::decode(&invalid_range).is_err());
        let mut zero_length = encoded.clone();
        zero_length[24..32].copy_from_slice(&0_u64.to_le_bytes());
        assert!(CommitRecord::decode(&zero_length).is_err());
        let zero_length = CommitRecord {
            span: ClosedLineSpan {
                start: 7,
                end_exclusive: 7,
            },
            ..record
        };
        let mut zero_length_bytes = vec![0; CommitRecord::ENCODED_LEN];
        assert!(zero_length.encode(&mut zero_length_bytes).is_err());
        let mut invalid_blob = encoded;
        invalid_blob[48] = BLOB_NONE;
        invalid_blob[64..72].copy_from_slice(&1_u64.to_le_bytes());
        assert!(CommitRecord::decode(&invalid_blob).is_err());
    }

    #[test]
    fn hunk_codec_rejects_payload_for_absent_source_range() {
        let record = HunkRecord {
            id: 0,
            file_id: 0,
            header_line: 0,
            span: ClosedLineSpan {
                start: 0,
                end_exclusive: 1,
            },
            kind: HunkKind::Malformed,
            old_range: None,
            new_range: None,
        };
        let mut encoded = vec![0; HunkRecord::ENCODED_LEN];
        record.encode(&mut encoded).unwrap();
        encoded[48..56].copy_from_slice(&1_u64.to_le_bytes());
        assert!(HunkRecord::decode(&encoded).is_err());
    }

    #[test]
    fn file_codec_rejects_reserved_line_bound() {
        let record = FileRecord {
            id: 0,
            commit_id: 0,
            span: ClosedLineSpan {
                start: 0,
                end_exclusive: 1,
            },
            hunks: HunkIdRange::empty_at(0),
            additions: None,
            deletions: None,
            old_path: PackedBlobRef::NONE,
            new_path: PackedBlobRef::NONE,
            status: ChangeStatus::Unknown,
            mode: FileMode::Malformed,
            line_bounds: StructureFileLineBounds {
                max_old_line_number: Some(OPTIONAL_U64_NONE),
                max_new_line_number: None,
            },
        };
        let mut encoded = vec![0; FileRecord::ENCODED_LEN];
        assert!(record.encode(&mut encoded).is_err());
    }

    #[test]
    fn reader_rejects_file_parent_outside_snapshot() {
        let storage = StructuralStorage::new().unwrap();
        let commits = storage.table::<CommitRecord>().unwrap();
        let files = storage.table::<FileRecord>().unwrap();
        let hunks = storage.table::<HunkRecord>().unwrap();
        let file = FileRecord {
            id: 0,
            commit_id: 9,
            span: ClosedLineSpan {
                start: 0,
                end_exclusive: 1,
            },
            hunks: HunkIdRange::empty_at(0),
            additions: None,
            deletions: None,
            old_path: PackedBlobRef::NONE,
            new_path: PackedBlobRef::NONE,
            status: ChangeStatus::Unknown,
            mode: FileMode::Malformed,
            line_bounds: StructureFileLineBounds::default(),
        };
        files.append(&file).unwrap();
        let snapshot = StructureSnapshot {
            commits,
            files,
            hunks,
            blobs: storage.blob_arena(),
            commit_high_water: 0,
            file_high_water: 1,
            hunk_high_water: 0,
            open_commit: None,
            open_file: None,
            open_hunk: None,
        };
        let mut reader = StructureReader::new();
        let error = reader.read_file(&snapshot, FileId::new(0)).unwrap_err();
        assert!(matches!(
            error,
            StructureStoreError::InvalidParent {
                parent_kind: "commit",
                parent_id: 9,
                ..
            }
        ));
    }

    #[test]
    fn child_file_read_rejects_parent_range_past_snapshot_high_water() {
        let storage = StructuralStorage::new().unwrap();
        let commits = storage.table::<CommitRecord>().unwrap();
        let files = storage.table::<FileRecord>().unwrap();
        let hunks = storage.table::<HunkRecord>().unwrap();
        commits
            .append(&CommitRecord {
                id: 0,
                kind: CommitKind::WorkingTree,
                span: ClosedLineSpan {
                    start: 0,
                    end_exclusive: 3,
                },
                files: FileIdRange { start: 0, count: 2 },
                object_id: PackedBlobRef::NONE,
                subject: PackedBlobRef::NONE,
            })
            .unwrap();
        files
            .append(&FileRecord {
                id: 0,
                commit_id: 0,
                span: ClosedLineSpan {
                    start: 1,
                    end_exclusive: 2,
                },
                hunks: HunkIdRange::empty_at(0),
                additions: None,
                deletions: None,
                old_path: PackedBlobRef::NONE,
                new_path: PackedBlobRef::NONE,
                status: ChangeStatus::Unknown,
                mode: FileMode::Malformed,
                line_bounds: StructureFileLineBounds::default(),
            })
            .unwrap();
        let snapshot = StructureSnapshot {
            commits,
            files,
            hunks,
            blobs: storage.blob_arena(),
            commit_high_water: 1,
            file_high_water: 1,
            hunk_high_water: 0,
            open_commit: None,
            open_file: None,
            open_hunk: None,
        };

        let error = StructureReader::new()
            .read_file(&snapshot, FileId::new(0))
            .unwrap_err();
        assert!(matches!(
            error,
            StructureStoreError::ChildRangeOutsideSnapshot {
                kind: "commit",
                id: 0,
                end: 2,
                high_water: 1,
            }
        ));
    }

    #[test]
    fn child_hunk_read_rejects_parent_range_past_snapshot_high_water() {
        let storage = StructuralStorage::new().unwrap();
        let commits = storage.table::<CommitRecord>().unwrap();
        let files = storage.table::<FileRecord>().unwrap();
        let hunks = storage.table::<HunkRecord>().unwrap();
        commits
            .append(&CommitRecord {
                id: 0,
                kind: CommitKind::WorkingTree,
                span: ClosedLineSpan {
                    start: 0,
                    end_exclusive: 4,
                },
                files: FileIdRange { start: 0, count: 1 },
                object_id: PackedBlobRef::NONE,
                subject: PackedBlobRef::NONE,
            })
            .unwrap();
        files
            .append(&FileRecord {
                id: 0,
                commit_id: 0,
                span: ClosedLineSpan {
                    start: 1,
                    end_exclusive: 4,
                },
                hunks: HunkIdRange { start: 0, count: 2 },
                additions: None,
                deletions: None,
                old_path: PackedBlobRef::NONE,
                new_path: PackedBlobRef::NONE,
                status: ChangeStatus::Unknown,
                mode: FileMode::Malformed,
                line_bounds: StructureFileLineBounds::default(),
            })
            .unwrap();
        hunks
            .append(&HunkRecord {
                id: 0,
                file_id: 0,
                header_line: 2,
                span: ClosedLineSpan {
                    start: 2,
                    end_exclusive: 3,
                },
                kind: HunkKind::Malformed,
                old_range: None,
                new_range: None,
            })
            .unwrap();
        let snapshot = StructureSnapshot {
            commits,
            files,
            hunks,
            blobs: storage.blob_arena(),
            commit_high_water: 1,
            file_high_water: 1,
            hunk_high_water: 1,
            open_commit: None,
            open_file: None,
            open_hunk: None,
        };

        let error = StructureReader::new()
            .read_hunk(&snapshot, HunkId::new(0))
            .unwrap_err();
        assert!(matches!(
            error,
            StructureStoreError::ChildRangeOutsideSnapshot {
                kind: "file",
                id: 0,
                end: 2,
                high_water: 1,
            }
        ));
    }

    #[test]
    fn snapshots_share_open_overlay_allocations_until_copy_on_write() {
        let storage = StructuralStorage::new().unwrap();
        let mut builder = StructureBuilder::new(&storage).unwrap();
        begin_sample_tree(&mut builder).unwrap();

        let first = builder.snapshot().unwrap();
        let second = builder.snapshot().unwrap();
        assert!(Arc::ptr_eq(
            builder.open_commit.as_ref().unwrap(),
            first.open_commit.as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(
            first.open_commit.as_ref().unwrap(),
            second.open_commit.as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(
            builder.open_file.as_ref().unwrap(),
            first.open_file.as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(
            builder.open_hunk.as_ref().unwrap(),
            first.open_hunk.as_ref().unwrap()
        ));
        assert_eq!(
            builder
                .open_commit
                .as_ref()
                .unwrap()
                .subject
                .as_ref()
                .unwrap()
                .as_ptr(),
            first
                .open_commit()
                .unwrap()
                .subject
                .as_ref()
                .unwrap()
                .as_ptr()
        );

        builder.open_commit_mut().unwrap().subject = Some("updated subject".to_owned());
        builder.open_file_mut().unwrap().new_path = Some("src/updated.rs".to_owned());
        builder.open_hunk_mut().unwrap().kind = HunkKind::Malformed;

        assert!(!Arc::ptr_eq(
            builder.open_commit.as_ref().unwrap(),
            first.open_commit.as_ref().unwrap()
        ));
        assert!(!Arc::ptr_eq(
            builder.open_file.as_ref().unwrap(),
            first.open_file.as_ref().unwrap()
        ));
        assert!(!Arc::ptr_eq(
            builder.open_hunk.as_ref().unwrap(),
            first.open_hunk.as_ref().unwrap()
        ));
        assert_eq!(
            first.open_commit().unwrap().subject.as_deref(),
            Some("subject")
        );
        assert_eq!(
            first.open_file().unwrap().new_path.as_deref(),
            Some("src/new.rs")
        );
        assert_eq!(first.open_hunk().unwrap().kind, HunkKind::Unified);
    }

    #[test]
    fn child_ranges_round_trip_empty_nonzero_cursors() {
        let files = FileIdRange::empty_at(37);
        let hunks = HunkIdRange::empty_at(91);
        assert_eq!(files.start(), 37);
        assert_eq!(files.end_exclusive(), 37);
        assert_eq!(files.first(), None);
        assert_eq!(files.last(), None);
        assert_eq!(files.get(0), None);
        assert_eq!(files.iter().count(), 0);
        assert_eq!(hunks.start(), 91);
        assert_eq!(hunks.end_exclusive(), 91);
        assert_eq!(hunks.first(), None);
        assert_eq!(hunks.last(), None);
        assert_eq!(hunks.get(0), None);
        assert_eq!(hunks.iter().count(), 0);

        let mut file_bytes = [0; 16];
        encode_file_range(files, &mut file_bytes, 0).unwrap();
        assert_eq!(u64::from_le_bytes(file_bytes[0..8].try_into().unwrap()), 37);
        assert_eq!(decode_file_range(&file_bytes, 0).unwrap(), files);
        let mut hunk_bytes = [0; 16];
        encode_hunk_range(hunks, &mut hunk_bytes, 0).unwrap();
        assert_eq!(u64::from_le_bytes(hunk_bytes[0..8].try_into().unwrap()), 91);
        assert_eq!(decode_hunk_range(&hunk_bytes, 0).unwrap(), hunks);

        let mut files = files;
        files.push(FileId::new(37)).unwrap();
        assert_eq!(files.first(), Some(FileId::new(37)));
        assert_eq!(files.last(), Some(FileId::new(37)));
        assert_eq!(files.get(0), Some(FileId::new(37)));
        assert_eq!(files.iter().collect::<Vec<_>>(), vec![FileId::new(37)]);
    }

    #[test]
    fn binary_search_seek_and_intersection_include_open_overlays() {
        const FINALIZED: u64 = 1_024;
        let storage = StructuralStorage::new().unwrap();
        let mut builder = StructureBuilder::new(&storage).unwrap();
        for index in 0..FINALIZED {
            let start = index * 4;
            let commit = builder
                .begin_commit(CommitKind::WorkingTree, None, line(start))
                .unwrap();
            let file = builder
                .begin_file(
                    commit,
                    None,
                    None,
                    ChangeStatus::Modified,
                    FileMode::Rich,
                    line(start),
                )
                .unwrap();
            builder
                .begin_hunk(
                    file,
                    HunkKind::Unified,
                    line(start),
                    Some(SourceRange { start, count: 1 }),
                    Some(SourceRange { start, count: 1 }),
                )
                .unwrap();
            builder.finish_hunk(start + 1).unwrap();
            builder.finish_file(start + 2).unwrap();
            builder.finish_commit(start + 3).unwrap();
        }
        let open_start = FINALIZED * 4;
        let open_commit = builder
            .begin_commit(CommitKind::WorkingTree, None, line(open_start))
            .unwrap();
        let open_file = builder
            .begin_file(
                open_commit,
                None,
                None,
                ChangeStatus::Modified,
                FileMode::Rich,
                line(open_start),
            )
            .unwrap();
        let open_hunk = builder
            .begin_hunk(
                open_file,
                HunkKind::Unified,
                line(open_start),
                Some(SourceRange {
                    start: open_start,
                    count: 1,
                }),
                Some(SourceRange {
                    start: open_start,
                    count: 1,
                }),
            )
            .unwrap();
        let snapshot = builder.snapshot().unwrap();
        assert_eq!(
            snapshot.visible_counts().unwrap(),
            StructureCounts {
                commits: FINALIZED + 1,
                files: FINALIZED + 1,
                hunks: FINALIZED + 1,
            }
        );

        let mut reader = StructureMetadataReader::new();
        let target = 500 * 4;
        reader.reset_statistics();
        assert_eq!(reader.file_lower_bound(&snapshot, target).unwrap(), 500);
        assert!(reader.statistics().file_records <= 11);
        assert_eq!(reader.file_upper_bound(&snapshot, target).unwrap(), 501);
        assert_eq!(
            reader
                .nth_file_before(&snapshot, target, NonZeroU64::new(1).unwrap())
                .unwrap(),
            Some(FileId::new(499))
        );
        assert_eq!(
            reader
                .nth_file_after(&snapshot, target, NonZeroU64::new(1).unwrap())
                .unwrap(),
            Some(FileId::new(501))
        );
        assert_eq!(
            reader
                .files_intersecting(&snapshot, target..target + 1)
                .unwrap(),
            FileIdRange {
                start: 500,
                count: 1,
            }
        );
        assert_eq!(
            reader
                .files_intersecting(&snapshot, target + 2..target + 4)
                .unwrap(),
            FileIdRange::empty_at(501)
        );

        assert_eq!(
            reader.commit_lower_bound(&snapshot, open_start).unwrap(),
            open_commit.get()
        );
        assert_eq!(
            reader.commit_upper_bound(&snapshot, open_start).unwrap(),
            FINALIZED + 1
        );
        assert_eq!(
            reader.file_lower_bound(&snapshot, open_start).unwrap(),
            open_file.get()
        );
        assert_eq!(
            reader.hunk_lower_bound(&snapshot, open_start).unwrap(),
            open_hunk.get()
        );
        assert_eq!(
            reader
                .commits_intersecting(&snapshot, open_start + 1..open_start + 2)
                .unwrap(),
            FINALIZED..FINALIZED + 1
        );
        assert_eq!(
            reader
                .files_intersecting(&snapshot, open_start + 1..open_start + 2)
                .unwrap(),
            FileIdRange {
                start: FINALIZED,
                count: 1,
            }
        );
        assert_eq!(
            reader
                .hunks_intersecting(&snapshot, open_start + 1..open_start + 2)
                .unwrap(),
            HunkIdRange {
                start: FINALIZED,
                count: 1,
            }
        );
    }

    #[test]
    fn non_empty_span_invariant_keeps_intersection_ranges_exact() {
        let storage = StructuralStorage::new().unwrap();
        let mut builder = StructureBuilder::new(&storage).unwrap();
        for start in 0..3 {
            builder
                .begin_commit(CommitKind::WorkingTree, None, line(start))
                .unwrap();
            builder.finish_commit(start + 1).unwrap();
        }
        let snapshot = builder.snapshot().unwrap();
        let mut reader = StructureMetadataReader::new();

        assert_eq!(reader.commits_intersecting(&snapshot, 0..1).unwrap(), 0..1);
        assert_eq!(reader.commits_intersecting(&snapshot, 1..2).unwrap(), 1..2);
        assert_eq!(reader.commits_intersecting(&snapshot, 0..3).unwrap(), 0..3);
        assert_eq!(reader.commits_intersecting(&snapshot, 1..1).unwrap(), 1..1);
        for id in 0..snapshot.commit_count() {
            let span = reader
                .read_commit(&snapshot, CommitId::new(id))
                .unwrap()
                .unwrap()
                .span;
            assert!(span.end_exclusive.unwrap() > span.start.get());
        }
    }

    #[test]
    fn parent_aware_sequential_reads_do_not_decode_parent_records() {
        const CHILDREN: u64 = 64;
        let storage = StructuralStorage::new().unwrap();
        let mut builder = StructureBuilder::new(&storage).unwrap();
        let commit_id = builder
            .begin_commit(CommitKind::WorkingTree, None, line(0))
            .unwrap();
        for index in 0..CHILDREN {
            let start = index * 3;
            let file = builder
                .begin_file(
                    commit_id,
                    None,
                    None,
                    ChangeStatus::Modified,
                    FileMode::Rich,
                    line(start),
                )
                .unwrap();
            builder
                .begin_hunk(
                    file,
                    HunkKind::Unified,
                    line(start),
                    Some(SourceRange { start, count: 1 }),
                    Some(SourceRange { start, count: 1 }),
                )
                .unwrap();
            builder.finish_hunk(start + 1).unwrap();
            builder.finish_file(start + 2).unwrap();
        }
        builder.finish_commit(CHILDREN * 3).unwrap();
        let snapshot = builder.snapshot().unwrap();

        let mut reader = StructureMetadataReader::new();
        let commit = reader.read_commit(&snapshot, commit_id).unwrap().unwrap();
        reader.reset_statistics();
        for file_id in commit.files.iter() {
            let file = reader
                .read_file_in_commit(&snapshot, file_id, commit)
                .unwrap()
                .unwrap();
            let hunk_id = file.hunks.first().unwrap();
            let hunk = reader
                .read_hunk_in_file(&snapshot, hunk_id, file)
                .unwrap()
                .unwrap();
            assert_eq!(hunk.file_id, file_id);
        }
        assert_eq!(
            reader.statistics(),
            StructureReadStats {
                commit_records: 0,
                file_records: CHILDREN,
                hunk_records: CHILDREN,
            }
        );

        let mut reader = StructureReader::new();
        let commit = reader
            .read_commit(&snapshot, commit_id)
            .unwrap()
            .unwrap()
            .metadata();
        let file = reader
            .read_file_in_commit(&snapshot, FileId::new(0), commit)
            .unwrap()
            .unwrap()
            .metadata();
        assert!(
            reader
                .read_hunk_in_file(&snapshot, HunkId::new(0), file)
                .unwrap()
                .is_some()
        );
    }
}
