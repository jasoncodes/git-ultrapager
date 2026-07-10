//! Raw logical-line table と Git structure を同じ high-water mark で公開する document layer。
//!
//! [`DocumentBuilder`] は parser thread だけが所有し、line text を保持せず
//! [`crate::document_store::DocumentLineStore`] へ固定長 metadata を追記する。Commit / file / hunk は
//! [`crate::structure_store::StructureBuilder`] へ追記し、line table とstructure tableの開始・終了位置を
//! 同じlogical-line high-water markに制限する。[`DocumentSnapshot`] は両tableのimmutable
//! snapshotだけを持ち、可変長pathやsubjectを件数分materializeしない。
//!
//! Raw textの復元はsnapshot自身では行わない。各workerまたはframeが[`DocumentLineReader`]を1つ持ち、
//! 返された[`DocumentLineRef`]を次のreadまでだけ借用する。複数行を保持する処理は
//! [`OwnedDocumentLine`]へ明示的にcopyし、保持範囲の上限を呼び出し側で決める。

use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use thiserror::Error;

use crate::document_store::{
    DocumentLineStore, LineDecoder, LineSnapshot, LineStoreError, LogicalLineMetadata,
    LogicalLineRecord, RawLineSpan, RawLineTracker,
};
use crate::input::AppendStore;
use crate::model::{
    ChangeStatus, CommitId, CommitKind, FileId, FileMode, HunkId, HunkKind, LineId, LineKind,
    LineSpan, SourceRange,
};
use crate::storage::StructuralStorage;
use crate::structure_store::{
    OpenCommit, OpenFile, OpenHunk, StoredFileMetadata, StoredHunk, StructureBuilder,
    StructureCounts, StructureFileLineBounds, StructureMetadataReader, StructureSnapshot,
    StructureStoreError,
};

/// Document snapshotの構築、metadata検証、lazy decodeに失敗した理由。
#[derive(Debug, Error)]
pub enum DocumentError {
    #[error(transparent)]
    LineStore(#[from] LineStoreError),
    #[error(transparent)]
    StructureStore(#[from] StructureStoreError),
    #[error("logical line count {count} does not fit a stable u64 ID")]
    LineCountOverflow { count: usize },
    #[error("logical line display length {length} does not fit the on-disk u64 field")]
    DisplayLengthOverflow { length: usize },
    #[error(
        "document builder expects {expected} logical lines, but its line store contains {actual}"
    )]
    LineStoreOutOfSync { expected: u64, actual: u64 },
    #[error(
        "logical line {line_id} references {kind} {referenced_id}, but only {available} records exist"
    )]
    InvalidLineReference {
        line_id: LineId,
        kind: &'static str,
        referenced_id: u64,
        available: u64,
    },
    #[error("logical line {line_id} references file {child_id} without a commit owner")]
    FileWithoutCommit { line_id: LineId, child_id: FileId },
    #[error("logical line {line_id} references hunk {child_id} without a file owner")]
    HunkWithoutFile { line_id: LineId, child_id: HunkId },
    #[error(
        "logical line {line_id} names commit {actual}, but file {file_id} belongs to commit {expected}"
    )]
    FileCommitMismatch {
        line_id: LineId,
        file_id: FileId,
        expected: CommitId,
        actual: CommitId,
    },
    #[error(
        "logical line {line_id} names file {actual}, but hunk {hunk_id} belongs to file {expected}"
    )]
    HunkFileMismatch {
        line_id: LineId,
        hunk_id: HunkId,
        expected: FileId,
        actual: FileId,
    },
    #[error("logical line {line_id} has a source line number without a file owner")]
    LineNumberWithoutFile { line_id: LineId },
    #[error(
        "{operation} requires logical-line high-water mark {expected}, but received boundary {actual}"
    )]
    StructureBoundaryMismatch {
        operation: &'static str,
        expected: u64,
        actual: u64,
    },
    #[error("{operation} requires open {kind} {expected}, but no {kind} is open")]
    MissingOpenStructure {
        operation: &'static str,
        kind: &'static str,
        expected: u64,
    },
    #[error("{operation} requires open {kind} {expected}, but open {kind} is {actual}")]
    OpenStructureIdentityMismatch {
        operation: &'static str,
        kind: &'static str,
        expected: u64,
        actual: u64,
    },
    #[error(
        "{kind} {id} span {start}..{end:?} is invalid for document line high-water mark {line_count}"
    )]
    InvalidStructureSpan {
        kind: &'static str,
        id: u64,
        start: u64,
        end: Option<u64>,
        line_count: u64,
    },
    #[error("logical line {line_id} falls outside {kind} {id} span {start}..{end}")]
    LineOutsideStructureSpan {
        line_id: LineId,
        kind: &'static str,
        id: u64,
        start: u64,
        end: u64,
    },
    #[error("cannot mark document complete while {kind} {id} is open")]
    OpenStructureAtCompletion { kind: &'static str, id: u64 },
    #[error("could not allocate {requested} bytes for an owned logical line")]
    OwnedLineAllocation { requested: usize },
}

/// 1つの[`DocumentBuilder`]から派生したsnapshotを識別するopaque token。
///
/// Stable line IDはbuilder内でだけ一意なので、viewport/cacheが同じ数値IDを別documentへ流用しないために
/// 使う。Cloneは同じ`Arc` allocationを共有し、別builderは内容が同じでも一致しない。
#[derive(Clone)]
pub struct DocumentIdentity(Arc<DocumentIdentityToken>);

#[derive(Debug)]
struct DocumentIdentityToken;

impl PartialEq for DocumentIdentity {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for DocumentIdentity {}

impl Hash for DocumentIdentity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

impl fmt::Debug for DocumentIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("DocumentIdentity")
            .field(&Arc::as_ptr(&self.0))
            .finish()
    }
}

/// 1回の[`DocumentBuilder::snapshot`]で公開されたsnapshot revisionを識別するtoken。
///
/// [`DocumentIdentity`]は同じbuilderから得たappend-only snapshot間でcacheを再利用するために共有する。
/// 一方、open file/hunkのmodeやgutter boundsは後続snapshotで変わり得るため、その時点のmetadataから
/// 計算したderived valueにはこのtokenを結び付ける。`DocumentSnapshot::clone`は同じallocationを共有し、
/// builderが次にsnapshotを取得すると、内容が同じでも別tokenになる。
#[derive(Clone)]
pub(crate) struct DocumentRevision(Arc<DocumentRevisionToken>);

#[derive(Debug)]
struct DocumentRevisionToken;

impl DocumentRevision {
    fn new() -> Self {
        Self(Arc::new(DocumentRevisionToken))
    }
}

impl PartialEq for DocumentRevision {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for DocumentRevision {}

impl fmt::Debug for DocumentRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("DocumentRevision")
            .field(&Arc::as_ptr(&self.0))
            .finish()
    }
}

/// Fixed recordに保存された表示text長。
///
/// `Overflowed` は元の長さが `u32::MAX` を超えたことだけを表す。巨大行を検索やsyntaxから除外する
/// callerはdecode前に[`Self::exceeds`]を使い、saturated値を正確な長さとして扱わない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineDisplayLength {
    Exact(u32),
    Overflowed,
}

impl LineDisplayLength {
    /// 表示text長が指定byte数を超えるかを返す。Overflow recordは任意の`usize`上限を超える扱いにする。
    #[must_use]
    pub fn exceeds(self, limit: usize) -> bool {
        match self {
            Self::Exact(length) => usize::try_from(length).map_or(true, |value| value > limit),
            Self::Overflowed => true,
        }
    }

    /// Fixed recordに正確な長さが収まる場合だけ`usize`へ変換する。
    #[must_use]
    pub fn exact(self) -> Option<usize> {
        match self {
            Self::Exact(length) => usize::try_from(length).ok(),
            Self::Overflowed => None,
        }
    }
}

/// Text decodeを伴わず読める1 logical lineのtyped metadata。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocumentLineMetadata {
    /// Snapshot内で不変の0始まりlogical-line ID。
    pub id: LineId,
    /// Parserがrecord append時に確定した分類。Parent file/hunkが後からfallbackになったsource lineは
    /// snapshot読取時に[`LineKind::Fallback`]へ正規化される。
    pub kind: LineKind,
    /// Plain preambleでは`None`。Commit配下では同じsnapshotのcommit ID。
    pub commit_id: Option<CommitId>,
    /// File section外では`None`。同じsnapshotのfile occurrence ID。
    pub file_id: Option<FileId>,
    /// Hunk外では`None`。同じsnapshotのhunk ID。
    pub hunk_id: Option<HunkId>,
    /// Context/deletion lineが消費する旧側行番号。それ以外では`None`。
    pub old_line_number: Option<u64>,
    /// Context/addition lineが消費する新側行番号。それ以外では`None`。
    pub new_line_number: Option<u64>,
    /// 親fileがrich modeの場合にgutter幅を決めるsnapshot世代の最大行番号。
    /// File外では`None`、fallback fileでは両側`None`のboundsに正規化する。
    pub file_line_bounds: Option<StructureFileLineBounds>,
    /// Raw input上で表示対象のLFにより閉じたlineか。最終unterminated lineだけ`false`。
    pub terminated: bool,
    /// ANSI sequenceを含むraw store上のbyte数。Logical-line終端のLF自体は含まない。
    pub raw_len: u64,
    /// ANSI除去とcontrol-byte escape後のUTF-8 byte数。Textをdecodeせずcap判定できる。
    pub display_len: LineDisplayLength,
}

impl DocumentLineMetadata {
    /// Eager decoderへ渡せるraw/display payload上限をどちらかが越えるかを返す。
    ///
    /// OSC/DCS payloadは表示textから消えるため、`display_len`だけでは巨大raw rangeを検出できない。
    /// Viewport、search、decoration、direct writerはtextを読む前にこのpredicateを共有する。
    #[must_use]
    pub fn exceeds_eager_text_limit(self, limit: usize) -> bool {
        let raw_exceeds = u64::try_from(limit).map_or(true, |limit| self.raw_len > limit);
        raw_exceeds || self.display_len.exceeds(limit)
    }
}

/// Decoder bufferを借用する1 logical line。
///
/// `text`はこの値を返した[`DocumentLineReader`]の次回readまでだけ有効で、snapshotやraw storeを
/// 所有しない。Source lineのbyte offsetはdiff markerをoffset 0として数える。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocumentLineRef<'a> {
    /// Snapshot内で不変の0始まりlogical-line ID。
    pub id: LineId,
    /// Reader所有bufferへのview。次に同じreaderを使うまで有効。
    pub text: &'a str,
    /// Parser分類をsnapshot世代のparent modeへ正規化した表示上のline分類。
    pub kind: LineKind,
    /// Plain preambleでは`None`。Commit配下では同じsnapshotのcommit ID。
    pub commit_id: Option<CommitId>,
    /// File section外では`None`。同じsnapshotのfile occurrence ID。
    pub file_id: Option<FileId>,
    /// Hunk外では`None`。同じsnapshotのhunk ID。
    pub hunk_id: Option<HunkId>,
    /// Context/deletion lineが消費する旧側行番号。それ以外では`None`。
    pub old_line_number: Option<u64>,
    /// Context/addition lineが消費する新側行番号。それ以外では`None`。
    pub new_line_number: Option<u64>,
    /// Snapshot世代の親file modeを反映したeffective gutter bounds。
    pub file_line_bounds: Option<StructureFileLineBounds>,
    /// Raw input上で表示対象のLFにより閉じたlineか。
    pub terminated: bool,
}

impl<'line> DocumentLineRef<'line> {
    /// Source lineでは先頭のdiff markerを除いた本文、それ以外では行全体を返す。
    #[must_use]
    pub fn content(self) -> &'line str {
        if is_source_line(self.kind) {
            self.text.get(1..).unwrap_or_default()
        } else {
            self.text
        }
    }

    /// Readerの次回利用後も保持できるowned copyを作る。
    pub fn try_to_owned(self) -> Result<OwnedDocumentLine, DocumentError> {
        let mut text = String::new();
        text.try_reserve_exact(self.text.len()).map_err(|_| {
            DocumentError::OwnedLineAllocation {
                requested: self.text.len(),
            }
        })?;
        text.push_str(self.text);
        Ok(OwnedDocumentLine {
            id: self.id,
            text,
            kind: self.kind,
            commit_id: self.commit_id,
            file_id: self.file_id,
            hunk_id: self.hunk_id,
            old_line_number: self.old_line_number,
            new_line_number: self.new_line_number,
            file_line_bounds: self.file_line_bounds,
            terminated: self.terminated,
        })
    }
}

/// Decoderから独立して保持できる1 logical line。
///
/// Viewport frameやintraline blockのように、呼び出し側が保持件数・payload上限を持つ範囲で使う。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedDocumentLine {
    /// Snapshot内で不変の0始まりlogical-line ID。
    pub id: LineId,
    /// ANSI除去とcontrol-byte escapeを終えたowned UTF-8 text。
    pub text: String,
    /// Parser分類をsnapshot世代のparent modeへ正規化した表示上のline分類。
    pub kind: LineKind,
    /// Plain preambleでは`None`。Commit配下では同じsnapshotのcommit ID。
    pub commit_id: Option<CommitId>,
    /// File section外では`None`。同じsnapshotのfile occurrence ID。
    pub file_id: Option<FileId>,
    /// Hunk外では`None`。同じsnapshotのhunk ID。
    pub hunk_id: Option<HunkId>,
    /// Context/deletion lineが消費する旧側行番号。それ以外では`None`。
    pub old_line_number: Option<u64>,
    /// Context/addition lineが消費する新側行番号。それ以外では`None`。
    pub new_line_number: Option<u64>,
    /// Snapshot世代の親file modeを反映したeffective gutter bounds。
    pub file_line_bounds: Option<StructureFileLineBounds>,
    /// Raw input上で表示対象のLFにより閉じたlineか。
    pub terminated: bool,
}

impl OwnedDocumentLine {
    /// Allocationなしで、このowned valueを借用するviewを返す。
    #[must_use]
    pub fn as_borrowed(&self) -> DocumentLineRef<'_> {
        DocumentLineRef {
            id: self.id,
            text: &self.text,
            kind: self.kind,
            commit_id: self.commit_id,
            file_id: self.file_id,
            hunk_id: self.hunk_id,
            old_line_number: self.old_line_number,
            new_line_number: self.new_line_number,
            file_line_bounds: self.file_line_bounds,
            terminated: self.terminated,
        }
    }

    /// Source lineでは先頭のdiff markerを除いた本文、それ以外では行全体を返す。
    #[must_use]
    pub fn content(&self) -> &str {
        self.as_borrowed().content()
    }
}

/// Logical-line high-water markとimmutable structural generationを束ねるcheap snapshot。
///
/// Cloneはraw line tableとspill-backed structure tableのhandle、最大3件のopen overlayだけを
/// 増やす。Line textとpath/subjectのowned copyは含まず、後からbuilderへappendされた
/// recordは[`Self::line_count`]にも[`Self::structure`]にも現れない。
#[derive(Clone)]
pub struct DocumentSnapshot {
    identity: DocumentIdentity,
    revision: DocumentRevision,
    lines: LineSnapshot,
    line_count: usize,
    structure: StructureSnapshot,
    complete: bool,
    trailing_newline: bool,
}

impl DocumentSnapshot {
    /// このsnapshotを生成したbuilder lineageを返す。
    #[must_use]
    pub fn identity(&self) -> DocumentIdentity {
        self.identity.clone()
    }

    /// Derived valueがこのsnapshot取得時点のmetadataに結び付いているかを照合するtokenを返す。
    pub(crate) fn revision(&self) -> DocumentRevision {
        self.revision.clone()
    }

    /// Snapshot取得時点で確定していたlogical-line数を返す。
    #[must_use]
    pub const fn line_count(&self) -> usize {
        self.line_count
    }

    /// Snapshotにlogical lineがないかを返す。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.line_count == 0
    }

    /// 0始まりのsnapshot indexをstable IDへ変換する。範囲外なら`None`を返す。
    #[must_use]
    pub fn line_id_at(&self, index: usize) -> Option<LineId> {
        if index >= self.line_count {
            return None;
        }
        u64::try_from(index).ok().map(LineId::new)
    }

    /// Stable line IDがこのsnapshotのhigh-water markより前にあるかを返す。
    #[must_use]
    pub fn contains_line(&self, id: LineId) -> bool {
        usize::try_from(id.get()).is_ok_and(|index| index < self.line_count)
    }

    /// Commit / file / hunkのimmutable high-water markとopen overlayを返す。
    ///
    /// Pathやsubjectを読むcallerは[`crate::structure_store::StructureReader`]、固定長metadata
    /// だけを読むcallerは[`StructureMetadataReader`]を自身で再利用する。
    #[must_use]
    pub const fn structure(&self) -> &StructureSnapshot {
        &self.structure
    }

    /// Open overlayを含むstructural record件数をoverflow検査付きで返す。
    pub fn structure_counts(&self) -> Result<StructureCounts, DocumentError> {
        self.structure.visible_counts().map_err(DocumentError::from)
    }

    /// EOF処理とopen spanのcloseが終わったsnapshotかを返す。
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.complete
    }

    /// Snapshot末尾のlogical lineがraw LFで閉じていたかを返す。空snapshotでは`false`。
    #[must_use]
    pub const fn has_trailing_newline(&self) -> bool {
        self.trailing_newline
    }

    /// File grammarを1件も検出していないplain pager snapshotかを返す。
    #[must_use]
    pub const fn is_plain(&self) -> bool {
        self.structure.is_plain()
    }

    /// 巨大行decoderへ渡すline-table snapshot。Raw store自体は公開しない。
    pub(crate) const fn line_snapshot(&self) -> &LineSnapshot {
        &self.lines
    }
}

impl fmt::Debug for DocumentSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DocumentSnapshot")
            .field("line_count", &self.line_count)
            .field("commit_count", &self.structure.commit_count())
            .field("file_count", &self.structure.file_count())
            .field("hunk_count", &self.structure.hunk_count())
            .field("complete", &self.complete)
            .field("trailing_newline", &self.trailing_newline)
            .finish()
    }
}

/// Snapshotのraw rangeを1行ずつborrowed textへ復元する再利用可能reader。
///
/// Metadata、raw、ANSI除去後byte、escaped UTF-8の各bufferをreader lifetimeで保持する。`read`の
/// 返り値が`&mut self`を借用するため、同じreaderから得た2行を同時に保持することはできない。
#[derive(Debug, Default)]
pub struct DocumentLineReader {
    decoder: LineDecoder,
    structure: StructureMetadataReader,
}

impl DocumentLineReader {
    /// Allocation前の空bufferを持つreaderを作る。各bufferは最初に必要になったreadで確保する。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            decoder: LineDecoder::new(),
            structure: StructureMetadataReader::new(),
        }
    }

    /// Stable IDをsnapshotからdecodeする。Snapshot外のIDは`Ok(None)`となる。
    ///
    /// Recordのcommit/file/hunk IDが同じsnapshotのstructural high-water markを越える場合、または
    /// parent relationと一致しない場合は[`DocumentError`]を返し、dangling IDをtyped viewへ渡さない。
    pub fn read<'a>(
        &'a mut self,
        document: &DocumentSnapshot,
        line_id: LineId,
    ) -> Result<Option<DocumentLineRef<'a>>, DocumentError> {
        let Self { decoder, structure } = self;
        let Some(decoded) = decoder.decode(&document.lines, line_id.get())? else {
            return Ok(None);
        };
        let metadata = metadata_from_record(document, line_id, decoded.record, structure)?;
        Ok(Some(line_ref(metadata, decoded.text)))
    }

    /// Stable IDをdecodeし、readerの次回利用後も保持できるowned valueへcopyする。
    pub fn read_owned(
        &mut self,
        document: &DocumentSnapshot,
        line_id: LineId,
    ) -> Result<Option<OwnedDocumentLine>, DocumentError> {
        self.read(document, line_id)?
            .map(DocumentLineRef::try_to_owned)
            .transpose()
    }
}

/// Textを読まず、fixed record metadataだけを順次読む再利用可能reader。
#[derive(Debug, Default)]
pub struct DocumentLineMetadataReader {
    record_buffer: Vec<u8>,
    structure: StructureMetadataReader,
}

impl DocumentLineMetadataReader {
    /// Spill record用bufferをまだ確保していない空readerを作る。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            record_buffer: Vec::new(),
            structure: StructureMetadataReader::new(),
        }
    }

    /// Stable IDのtyped metadataを返す。Spill tierでは64 byte bufferを次回readへ再利用する。
    pub fn read(
        &mut self,
        document: &DocumentSnapshot,
        line_id: LineId,
    ) -> Result<Option<DocumentLineMetadata>, DocumentError> {
        let Some(record) = document
            .lines
            .record_with_buffer(line_id.get(), &mut self.record_buffer)?
        else {
            return Ok(None);
        };
        metadata_from_record(document, line_id, record, &mut self.structure).map(Some)
    }
}

/// Parserがboundary proofへ添えるtyped line metadata。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AppendDocumentLine {
    pub display_len: usize,
    pub kind: LineKind,
    pub commit_id: Option<CommitId>,
    pub file_id: Option<FileId>,
    pub hunk_id: Option<HunkId>,
    pub old_line_number: Option<u64>,
    pub new_line_number: Option<u64>,
}

/// Parser threadだけが更新するdocument writer。
pub(crate) struct DocumentBuilder {
    identity: DocumentIdentity,
    line_store: DocumentLineStore,
    line_count: usize,
    structure: StructureBuilder,
    structure_reader: StructureMetadataReader,
    complete: bool,
    trailing_newline: bool,
}

impl fmt::Debug for DocumentBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DocumentBuilder")
            .field("line_count", &self.line_count)
            .field("structure_counts", &self.structure.visible_counts().ok())
            .field("complete", &self.complete)
            .field("trailing_newline", &self.trailing_newline)
            .finish_non_exhaustive()
    }
}

impl DocumentBuilder {
    /// Raw input storeとstructural budgetを共有する空builderを作る。
    pub(crate) fn new(
        raw: AppendStore,
        storage: &StructuralStorage,
    ) -> Result<Self, DocumentError> {
        Ok(Self {
            identity: DocumentIdentity(Arc::new(DocumentIdentityToken)),
            line_store: DocumentLineStore::new(raw, storage)?,
            line_count: 0,
            structure: StructureBuilder::new(storage)?,
            structure_reader: StructureMetadataReader::new(),
            complete: false,
            trailing_newline: false,
        })
    }

    /// このbuilderのline storeに紐づく唯一のraw boundary trackerを開始する。
    pub(crate) fn start_line_tracker(&self) -> Result<RawLineTracker, DocumentError> {
        self.line_store.start_tracker().map_err(DocumentError::from)
    }

    #[must_use]
    pub(crate) const fn line_count(&self) -> usize {
        self.line_count
    }

    /// 次にappendするlogical lineのstable IDを返す。
    pub(crate) fn next_line_id(&self) -> Result<LineId, DocumentError> {
        Ok(LineId::new(self.line_high_water()?))
    }

    /// Open overlayを含むstructural record件数を返す。Parse deltaはこの値を
    /// 追記前後で差分化し、spill tableをmaterializeしない。
    pub(crate) fn structure_counts(&self) -> Result<StructureCounts, DocumentError> {
        self.structure.visible_counts().map_err(DocumentError::from)
    }

    /// 現在のline high-water markでcommitを開始する。
    pub(crate) fn begin_commit(
        &mut self,
        kind: CommitKind,
        object_id: Option<String>,
        start: LineId,
    ) -> Result<CommitId, DocumentError> {
        self.validate_boundary("begin commit", start.get())?;
        self.structure
            .begin_commit(kind, object_id, start)
            .map_err(DocumentError::from)
    }

    /// 指定IDが現在のopen commitと一致する場合だけimmutable viewを返す。
    pub(crate) fn open_commit(&self, id: CommitId) -> Result<&OpenCommit, DocumentError> {
        let Some(commit) = self.structure.open_commit() else {
            return Err(DocumentError::MissingOpenStructure {
                operation: "read commit",
                kind: "commit",
                expected: id.get(),
            });
        };
        if commit.id() != id {
            return Err(DocumentError::OpenStructureIdentityMismatch {
                operation: "read commit",
                kind: "commit",
                expected: id.get(),
                actual: commit.id().get(),
            });
        }
        Ok(commit)
    }

    /// 指定IDのopen commitをCOW分離して更新可能にする。
    pub(crate) fn open_commit_mut(
        &mut self,
        id: CommitId,
    ) -> Result<&mut OpenCommit, DocumentError> {
        let _ = self.open_commit(id)?;
        self.structure
            .open_commit_mut()
            .ok_or(DocumentError::MissingOpenStructure {
                operation: "mutate commit",
                kind: "commit",
                expected: id.get(),
            })
    }

    /// 現在のline high-water markでopen commit配下のfileを開始する。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn begin_file(
        &mut self,
        commit_id: CommitId,
        old_path: Option<String>,
        new_path: Option<String>,
        status: ChangeStatus,
        mode: FileMode,
        start: LineId,
    ) -> Result<FileId, DocumentError> {
        self.validate_boundary("begin file", start.get())?;
        let _ = self.open_commit(commit_id)?;
        self.structure
            .begin_file(commit_id, old_path, new_path, status, mode, start)
            .map_err(DocumentError::from)
    }

    /// 指定IDが現在のopen fileと一致する場合だけimmutable viewを返す。
    pub(crate) fn open_file(&self, id: FileId) -> Result<&OpenFile, DocumentError> {
        let Some(file) = self.structure.open_file() else {
            return Err(DocumentError::MissingOpenStructure {
                operation: "read file",
                kind: "file",
                expected: id.get(),
            });
        };
        if file.id() != id {
            return Err(DocumentError::OpenStructureIdentityMismatch {
                operation: "read file",
                kind: "file",
                expected: id.get(),
                actual: file.id().get(),
            });
        }
        Ok(file)
    }

    /// 指定IDのopen fileをCOW分離して更新可能にする。
    pub(crate) fn open_file_mut(&mut self, id: FileId) -> Result<&mut OpenFile, DocumentError> {
        let _ = self.open_file(id)?;
        self.structure
            .open_file_mut()
            .ok_or(DocumentError::MissingOpenStructure {
                operation: "mutate file",
                kind: "file",
                expected: id.get(),
            })
    }

    /// 現在のline high-water markでopen file配下のhunkを開始する。
    pub(crate) fn begin_hunk(
        &mut self,
        file_id: FileId,
        kind: HunkKind,
        header_line: LineId,
        old_range: Option<SourceRange>,
        new_range: Option<SourceRange>,
    ) -> Result<HunkId, DocumentError> {
        self.validate_boundary("begin hunk", header_line.get())?;
        let _ = self.open_file(file_id)?;
        self.structure
            .begin_hunk(file_id, kind, header_line, old_range, new_range)
            .map_err(DocumentError::from)
    }

    /// 指定IDが現在のopen hunkと一致する場合だけimmutable viewを返す。
    pub(crate) fn open_hunk(&self, id: HunkId) -> Result<&OpenHunk, DocumentError> {
        let Some(hunk) = self.structure.open_hunk() else {
            return Err(DocumentError::MissingOpenStructure {
                operation: "read hunk",
                kind: "hunk",
                expected: id.get(),
            });
        };
        if hunk.id() != id {
            return Err(DocumentError::OpenStructureIdentityMismatch {
                operation: "read hunk",
                kind: "hunk",
                expected: id.get(),
                actual: hunk.id().get(),
            });
        }
        Ok(hunk)
    }

    /// 指定IDのopen hunkをCOW分離して更新可能にする。
    pub(crate) fn open_hunk_mut(&mut self, id: HunkId) -> Result<&mut OpenHunk, DocumentError> {
        let _ = self.open_hunk(id)?;
        self.structure
            .open_hunk_mut()
            .ok_or(DocumentError::MissingOpenStructure {
                operation: "mutate hunk",
                kind: "hunk",
                expected: id.get(),
            })
    }

    /// 指定open hunkを現在のline high-water markで閉じる。
    pub(crate) fn finish_hunk(
        &mut self,
        id: HunkId,
        end_exclusive: u64,
    ) -> Result<(), DocumentError> {
        self.validate_boundary("finish hunk", end_exclusive)?;
        let _ = self.open_hunk(id)?;
        let actual = self.structure.finish_hunk(end_exclusive)?;
        if actual != id {
            return Err(DocumentError::OpenStructureIdentityMismatch {
                operation: "finish hunk",
                kind: "hunk",
                expected: id.get(),
                actual: actual.get(),
            });
        }
        Ok(())
    }

    /// 指定open fileを現在のline high-water markで閉じる。
    pub(crate) fn finish_file(
        &mut self,
        id: FileId,
        end_exclusive: u64,
    ) -> Result<(), DocumentError> {
        self.validate_boundary("finish file", end_exclusive)?;
        let _ = self.open_file(id)?;
        let actual = self.structure.finish_file(end_exclusive)?;
        if actual != id {
            return Err(DocumentError::OpenStructureIdentityMismatch {
                operation: "finish file",
                kind: "file",
                expected: id.get(),
                actual: actual.get(),
            });
        }
        Ok(())
    }

    /// 指定open commitを現在のline high-water markで閉じる。
    pub(crate) fn finish_commit(
        &mut self,
        id: CommitId,
        end_exclusive: u64,
    ) -> Result<(), DocumentError> {
        self.validate_boundary("finish commit", end_exclusive)?;
        let _ = self.open_commit(id)?;
        let actual = self.structure.finish_commit(end_exclusive)?;
        if actual != id {
            return Err(DocumentError::OpenStructureIdentityMismatch {
                operation: "finish commit",
                kind: "commit",
                expected: id.get(),
                actual: actual.get(),
            });
        }
        Ok(())
    }

    /// Boundary proofとtyped metadataを1つのfixed recordとしてcommitする。
    ///
    /// Stable referenceとstore/table countをappend前に検証する。File bounds、line count、末尾改行状態は
    /// record append成功後だけ更新するため、I/O failureでsnapshot側metadataだけが先行しない。
    pub(crate) fn append_line(
        &mut self,
        span: RawLineSpan,
        line: AppendDocumentLine,
    ) -> Result<LineId, DocumentError> {
        let next_line_count =
            self.line_count
                .checked_add(1)
                .ok_or(DocumentError::LineCountOverflow {
                    count: self.line_count,
                })?;
        let expected =
            u64::try_from(self.line_count).map_err(|_| DocumentError::LineCountOverflow {
                count: self.line_count,
            })?;
        let actual = self.line_store.len()?;
        if actual != expected {
            return Err(DocumentError::LineStoreOutOfSync { expected, actual });
        }
        let line_id = LineId::new(expected);
        {
            // Validation snapshotはopen overlayのArcを共有する。Line bounds更新まで保持すると
            // `Arc::make_mut`がpathを含むOpenFile全体を各source lineで複製するため、参照検査を
            // 終えた時点でscopeを閉じ、後段のmutationを唯一所有のoverlayへ適用する。
            let structure = self.structure.snapshot()?;
            validate_line_references(
                line_id,
                line.commit_id,
                line.file_id,
                line.hunk_id,
                line.old_line_number,
                line.new_line_number,
                &structure,
                u64::try_from(next_line_count).map_err(|_| DocumentError::LineCountOverflow {
                    count: next_line_count,
                })?,
                &mut self.structure_reader,
            )?;
        }
        if let Some(file_id) = line.file_id
            && (line.old_line_number.is_some() || line.new_line_number.is_some())
        {
            // Boundsはline record append成功後だけ進めるが、更新先のopen overlayは
            // append前に確定する。Finalized fileへの番号付きlineを許すと、record書き込み後に
            // bounds更新だけ失敗し、line tableとstructure tableが分離する。
            let _ = self.open_file(file_id)?;
        }
        let display_len =
            u64::try_from(line.display_len).map_err(|_| DocumentError::DisplayLengthOverflow {
                length: line.display_len,
            })?;
        let terminated = span.terminated();
        let appended = self.line_store.append(
            span,
            LogicalLineMetadata {
                commit_id: line.commit_id.map(CommitId::get),
                file_id: line.file_id.map(FileId::get),
                hunk_id: line.hunk_id.map(HunkId::get),
                old_line_number: line.old_line_number,
                new_line_number: line.new_line_number,
                display_len,
                kind: line.kind,
            },
        )?;
        if appended != expected {
            return Err(DocumentError::LineStoreOutOfSync {
                expected,
                actual: appended,
            });
        }

        if let Some(file_id) = line.file_id
            && (line.old_line_number.is_some() || line.new_line_number.is_some())
        {
            self.open_file_mut(file_id)?
                .line_bounds
                .observe(line.old_line_number, line.new_line_number);
        }
        self.line_count = next_line_count;
        self.trailing_newline = terminated;
        Ok(line_id)
    }

    /// 現在のline high-water markとstructural COW generationを固定したsnapshotを返す。
    pub(crate) fn snapshot(&self) -> Result<DocumentSnapshot, DocumentError> {
        let lines = self.line_store.snapshot()?;
        let expected =
            u64::try_from(self.line_count).map_err(|_| DocumentError::LineCountOverflow {
                count: self.line_count,
            })?;
        if lines.len() != expected {
            return Err(DocumentError::LineStoreOutOfSync {
                expected,
                actual: lines.len(),
            });
        }
        let structure = self.structure.snapshot()?;
        validate_open_snapshot_spans(&structure, expected)?;
        Ok(DocumentSnapshot {
            identity: self.identity.clone(),
            revision: DocumentRevision::new(),
            lines,
            line_count: self.line_count,
            structure,
            complete: self.complete,
            trailing_newline: self.trailing_newline,
        })
    }

    /// Open structureが残っていないline/structure同期済みdocumentをEOF状態にする。
    pub(crate) fn mark_complete(&mut self) -> Result<(), DocumentError> {
        let structure = self.structure.snapshot()?;
        if let Some(hunk) = structure.open_hunk() {
            return Err(DocumentError::OpenStructureAtCompletion {
                kind: "hunk",
                id: hunk.id().get(),
            });
        }
        if let Some(file) = structure.open_file() {
            return Err(DocumentError::OpenStructureAtCompletion {
                kind: "file",
                id: file.id().get(),
            });
        }
        if let Some(commit) = structure.open_commit() {
            return Err(DocumentError::OpenStructureAtCompletion {
                kind: "commit",
                id: commit.id().get(),
            });
        }
        let _ = self.snapshot()?;
        self.complete = true;
        Ok(())
    }

    fn line_high_water(&self) -> Result<u64, DocumentError> {
        u64::try_from(self.line_count).map_err(|_| DocumentError::LineCountOverflow {
            count: self.line_count,
        })
    }

    fn validate_boundary(&self, operation: &'static str, actual: u64) -> Result<(), DocumentError> {
        let expected = self.line_high_water()?;
        if actual != expected {
            return Err(DocumentError::StructureBoundaryMismatch {
                operation,
                expected,
                actual,
            });
        }
        Ok(())
    }
}

fn metadata_from_record(
    document: &DocumentSnapshot,
    line_id: LineId,
    record: LogicalLineRecord,
    structure_reader: &mut StructureMetadataReader,
) -> Result<DocumentLineMetadata, DocumentError> {
    let commit_id = record.commit_id().map(CommitId::new);
    let file_id = record.file_id().map(FileId::new);
    let hunk_id = record.hunk_id().map(HunkId::new);
    let line_count =
        u64::try_from(document.line_count).map_err(|_| DocumentError::LineCountOverflow {
            count: document.line_count,
        })?;
    let validated = validate_line_references(
        line_id,
        commit_id,
        file_id,
        hunk_id,
        record.old_line_number(),
        record.new_line_number(),
        &document.structure,
        line_count,
        structure_reader,
    )?;
    let display_len = if record.display_len_overflowed() {
        LineDisplayLength::Overflowed
    } else {
        LineDisplayLength::Exact(record.display_len())
    };
    let mut kind = record.kind();
    let mut old_line_number = record.old_line_number();
    let mut new_line_number = record.new_line_number();
    let file_is_fallback = validated.file.is_some_and(|file| !file.mode.is_rich());
    let hunk_is_fallback = validated
        .hunk
        .is_some_and(|hunk| hunk.kind != HunkKind::Unified);
    if is_source_line(kind) && (file_is_fallback || hunk_is_fallback) {
        // Hunk bodyがstream中はrichでも、EOF時の不足行や後続のunsupported markerでfile全体が
        // fallbackへ変わり得る。Fixed line recordはappend-onlyなので、snapshot世代のparent modeを
        // typed viewへ反映し、過去に記録したsource番号やstyleを後のsnapshotへ漏らさない。
        kind = LineKind::Fallback;
        old_line_number = None;
        new_line_number = None;
    }
    let file_line_bounds = validated.file.map(|file| {
        if file.mode.is_rich() {
            file.line_bounds
        } else {
            StructureFileLineBounds::default()
        }
    });
    Ok(DocumentLineMetadata {
        id: line_id,
        kind,
        commit_id,
        file_id,
        hunk_id,
        old_line_number,
        new_line_number,
        file_line_bounds,
        terminated: record.terminated(),
        raw_len: record.raw_end().saturating_sub(record.raw_start()),
        display_len,
    })
}

fn line_ref(metadata: DocumentLineMetadata, text: &str) -> DocumentLineRef<'_> {
    DocumentLineRef {
        id: metadata.id,
        text,
        kind: metadata.kind,
        commit_id: metadata.commit_id,
        file_id: metadata.file_id,
        hunk_id: metadata.hunk_id,
        old_line_number: metadata.old_line_number,
        new_line_number: metadata.new_line_number,
        file_line_bounds: metadata.file_line_bounds,
        terminated: metadata.terminated,
    }
}

#[derive(Clone, Copy, Debug)]
struct ValidatedLineStructure {
    file: Option<StoredFileMetadata>,
    hunk: Option<StoredHunk>,
}

#[allow(clippy::too_many_arguments)]
fn validate_line_references(
    line_id: LineId,
    commit_id: Option<CommitId>,
    file_id: Option<FileId>,
    hunk_id: Option<HunkId>,
    old_line_number: Option<u64>,
    new_line_number: Option<u64>,
    structure: &StructureSnapshot,
    line_count: u64,
    reader: &mut StructureMetadataReader,
) -> Result<ValidatedLineStructure, DocumentError> {
    let counts = structure.visible_counts()?;
    let commit = if let Some(id) = commit_id {
        let commit =
            reader
                .read_commit(structure, id)?
                .ok_or(DocumentError::InvalidLineReference {
                    line_id,
                    kind: "commit",
                    referenced_id: id.get(),
                    available: counts.commits,
                })?;
        validate_structure_span("commit", id.get(), commit.span, line_id, line_count)?;
        Some(commit)
    } else {
        None
    };

    let file = if let Some(id) = file_id {
        let Some(parent) = commit else {
            return Err(DocumentError::FileWithoutCommit {
                line_id,
                child_id: id,
            });
        };
        let file = reader.read_file_in_commit(structure, id, parent)?.ok_or(
            DocumentError::InvalidLineReference {
                line_id,
                kind: "file",
                referenced_id: id.get(),
                available: counts.files,
            },
        )?;
        if file.commit_id != parent.id {
            return Err(DocumentError::FileCommitMismatch {
                line_id,
                file_id: id,
                expected: file.commit_id,
                actual: parent.id,
            });
        }
        validate_structure_span("file", id.get(), file.span, line_id, line_count)?;
        Some(file)
    } else {
        None
    };

    let hunk = if let Some(id) = hunk_id {
        let Some(parent) = file else {
            return Err(DocumentError::HunkWithoutFile {
                line_id,
                child_id: id,
            });
        };
        let hunk = reader.read_hunk_in_file(structure, id, parent)?.ok_or(
            DocumentError::InvalidLineReference {
                line_id,
                kind: "hunk",
                referenced_id: id.get(),
                available: counts.hunks,
            },
        )?;
        if hunk.file_id != parent.id {
            return Err(DocumentError::HunkFileMismatch {
                line_id,
                hunk_id: id,
                expected: hunk.file_id,
                actual: parent.id,
            });
        }
        validate_structure_span("hunk", id.get(), hunk.span, line_id, line_count)?;
        Some(hunk)
    } else {
        None
    };

    if file_id.is_none() && (old_line_number.is_some() || new_line_number.is_some()) {
        return Err(DocumentError::LineNumberWithoutFile { line_id });
    }
    Ok(ValidatedLineStructure { file, hunk })
}

fn validate_structure_span(
    kind: &'static str,
    id: u64,
    span: LineSpan,
    line_id: LineId,
    line_count: u64,
) -> Result<(), DocumentError> {
    let start = span.start.get();
    let end = span.end_exclusive.unwrap_or(line_count);
    if start >= end || end > line_count {
        return Err(DocumentError::InvalidStructureSpan {
            kind,
            id,
            start,
            end: span.end_exclusive,
            line_count,
        });
    }
    if line_id.get() < start || line_id.get() >= end {
        return Err(DocumentError::LineOutsideStructureSpan {
            line_id,
            kind,
            id,
            start,
            end,
        });
    }
    Ok(())
}

fn validate_open_snapshot_spans(
    structure: &StructureSnapshot,
    line_count: u64,
) -> Result<(), DocumentError> {
    if let Some(commit) = structure.open_commit() {
        validate_structure_span(
            "commit",
            commit.id().get(),
            commit.span(),
            commit.span().start,
            line_count,
        )?;
    }
    if let Some(file) = structure.open_file() {
        validate_structure_span(
            "file",
            file.id().get(),
            file.span(),
            file.span().start,
            line_count,
        )?;
    }
    if let Some(hunk) = structure.open_hunk() {
        validate_structure_span(
            "hunk",
            hunk.id().get(),
            hunk.span(),
            hunk.span().start,
            line_count,
        )?;
    }
    Ok(())
}

fn is_source_line(kind: LineKind) -> bool {
    matches!(
        kind,
        LineKind::Context | LineKind::Deletion | LineKind::Addition
    )
}

#[cfg(test)]
mod tests {
    use crate::document_store::{LogicalLineMetadata, RawLineSpan, RawLineTracker};
    use crate::input::AppendStore;
    use crate::model::{
        ChangeStatus, CommitId, CommitKind, FileId, FileMode, HunkId, HunkKind, LineId, LineKind,
        SourceRange,
    };
    use crate::storage::{StorageConfig, StructuralStorage};
    use crate::structure_store::{StructureFileLineBounds, StructureMetadataReader};

    use super::{
        AppendDocumentLine, DocumentBuilder, DocumentError, DocumentLineMetadataReader,
        DocumentLineReader, DocumentSnapshot, LineDisplayLength,
    };

    fn begin_rich_structure(
        builder: &mut DocumentBuilder,
        start: LineId,
    ) -> (CommitId, FileId, HunkId) {
        let commit_id = builder
            .begin_commit(CommitKind::WorkingTree, None, start)
            .unwrap();
        let file_id = builder
            .begin_file(
                commit_id,
                Some("src/lib.rs".to_owned()),
                Some("src/lib.rs".to_owned()),
                ChangeStatus::Modified,
                FileMode::Rich,
                start,
            )
            .unwrap();
        let hunk_id = builder
            .begin_hunk(
                file_id,
                HunkKind::Unified,
                start,
                Some(SourceRange {
                    start: 10,
                    count: 2,
                }),
                Some(SourceRange {
                    start: 20,
                    count: 2,
                }),
            )
            .unwrap();
        (commit_id, file_id, hunk_id)
    }

    fn append_tracked_line(
        raw: &AppendStore,
        tracker: &mut RawLineTracker,
        bytes: &[u8],
    ) -> RawLineSpan {
        let start = raw.append(bytes).unwrap();
        let mut span = None;
        for (relative, byte) in bytes.iter().copied().enumerate() {
            span = tracker
                .observe(start + relative as u64, byte)
                .unwrap()
                .or(span);
        }
        span.expect("test line must include a displayed newline")
    }

    #[test]
    fn snapshot_freezes_lines_structures_and_file_line_bounds() {
        let raw = AppendStore::new();
        let storage = StructuralStorage::new().unwrap();
        let mut builder = DocumentBuilder::new(raw.clone(), &storage).unwrap();
        let mut tracker = builder.start_line_tracker().unwrap();

        let (commit_id, file_id, hunk_id) = begin_rich_structure(&mut builder, LineId::new(0));

        let first_start = raw.append(b" old\n").unwrap();
        let mut first_span = None;
        for (relative, byte) in b" old\n".iter().copied().enumerate() {
            first_span = tracker
                .observe(first_start + relative as u64, byte)
                .unwrap()
                .or(first_span);
        }
        let first_id = builder
            .append_line(
                first_span.unwrap(),
                AppendDocumentLine {
                    display_len: 4,
                    kind: LineKind::Context,
                    commit_id: Some(commit_id),
                    file_id: Some(file_id),
                    hunk_id: Some(hunk_id),
                    old_line_number: Some(10),
                    new_line_number: Some(20),
                },
            )
            .unwrap();
        assert_eq!(first_id, LineId::new(0));
        let earlier = builder.snapshot().unwrap();

        builder.open_file_mut(file_id).unwrap().additions = Some(1);
        let second_start = raw.append(b"+new\n").unwrap();
        let mut second_span = None;
        for (relative, byte) in b"+new\n".iter().copied().enumerate() {
            second_span = tracker
                .observe(second_start + relative as u64, byte)
                .unwrap()
                .or(second_span);
        }
        let second_id = builder
            .append_line(
                second_span.unwrap(),
                AppendDocumentLine {
                    display_len: 4,
                    kind: LineKind::Addition,
                    commit_id: Some(commit_id),
                    file_id: Some(file_id),
                    hunk_id: Some(hunk_id),
                    old_line_number: None,
                    new_line_number: Some(99),
                },
            )
            .unwrap();
        assert_eq!(second_id, LineId::new(1));
        let later = builder.snapshot().unwrap();

        assert_eq!(earlier.line_count(), 1);
        assert_eq!(later.line_count(), 2);
        let mut structure_reader = StructureMetadataReader::new();
        let earlier_file = structure_reader
            .read_file(earlier.structure(), file_id)
            .unwrap()
            .unwrap();
        let later_file = structure_reader
            .read_file(later.structure(), file_id)
            .unwrap()
            .unwrap();
        assert_eq!(earlier_file.additions, Some(0));
        assert_eq!(later_file.additions, Some(1));
        assert_eq!(
            earlier_file.line_bounds,
            StructureFileLineBounds {
                max_old_line_number: Some(10),
                max_new_line_number: Some(20),
            }
        );
        assert_eq!(
            later_file.line_bounds,
            StructureFileLineBounds {
                max_old_line_number: Some(10),
                max_new_line_number: Some(99),
            }
        );

        let mut reader = DocumentLineReader::new();
        assert_eq!(
            reader
                .read(&earlier, second_id)
                .unwrap()
                .map(|line| line.text),
            None
        );
        assert_eq!(
            reader
                .read(&later, second_id)
                .unwrap()
                .map(|line| line.text),
            Some("+new")
        );
    }

    #[test]
    fn numbered_line_append_does_not_clone_open_file_paths_for_reference_validation() {
        let raw = AppendStore::new();
        let storage = StructuralStorage::new().unwrap();
        let mut builder = DocumentBuilder::new(raw.clone(), &storage).unwrap();
        let mut tracker = builder.start_line_tracker().unwrap();
        // Parserが受け入れる最大級のpathで、行数に比例したCOW copyへ戻らないことをallocation
        // identityで検査する。Line boundsだけの更新ではpath payloadを置き換える理由がない。
        let long_path = format!("src/{}", "x".repeat((1 << 20) - 4));
        let commit_id = builder
            .begin_commit(CommitKind::WorkingTree, None, LineId::new(0))
            .unwrap();
        let file_id = builder
            .begin_file(
                commit_id,
                Some(long_path.clone()),
                Some(long_path),
                ChangeStatus::Modified,
                FileMode::Rich,
                LineId::new(0),
            )
            .unwrap();
        let hunk_id = builder
            .begin_hunk(
                file_id,
                HunkKind::Unified,
                LineId::new(0),
                Some(SourceRange { start: 1, count: 3 }),
                Some(SourceRange { start: 1, count: 3 }),
            )
            .unwrap();
        let old_path_allocation = builder
            .open_file(file_id)
            .unwrap()
            .old_path
            .as_deref()
            .unwrap()
            .as_ptr();
        let new_path_allocation = builder
            .open_file(file_id)
            .unwrap()
            .new_path
            .as_deref()
            .unwrap()
            .as_ptr();

        for line_number in 1..=3 {
            let span = append_tracked_line(&raw, &mut tracker, b" line\n");
            builder
                .append_line(
                    span,
                    AppendDocumentLine {
                        display_len: 5,
                        kind: LineKind::Context,
                        commit_id: Some(commit_id),
                        file_id: Some(file_id),
                        hunk_id: Some(hunk_id),
                        old_line_number: Some(line_number),
                        new_line_number: Some(line_number),
                    },
                )
                .unwrap();
            let open_file = builder.open_file(file_id).unwrap();
            assert_eq!(
                open_file.old_path.as_deref().unwrap().as_ptr(),
                old_path_allocation
            );
            assert_eq!(
                open_file.new_path.as_deref().unwrap().as_ptr(),
                new_path_allocation
            );
        }
    }

    #[test]
    fn line_reader_returns_typed_borrowed_and_owned_views() {
        let raw = AppendStore::with_memory_limit(0);
        let config = StorageConfig::new(16 * 1024, 16 * 1024, 64, 64).unwrap();
        let storage = StructuralStorage::with_config(config).unwrap();
        let mut builder = DocumentBuilder::new(raw.clone(), &storage).unwrap();
        let mut tracker = builder.start_line_tracker().unwrap();
        let (commit_id, file_id, hunk_id) = begin_rich_structure(&mut builder, LineId::new(0));

        let start = raw.append(b"-old\n").unwrap();
        let mut span = None;
        for (relative, byte) in b"-old\n".iter().copied().enumerate() {
            span = tracker
                .observe(start + relative as u64, byte)
                .unwrap()
                .or(span);
        }
        let line_id = builder
            .append_line(
                span.unwrap(),
                AppendDocumentLine {
                    display_len: 4,
                    kind: LineKind::Deletion,
                    commit_id: Some(commit_id),
                    file_id: Some(file_id),
                    hunk_id: Some(hunk_id),
                    old_line_number: Some(10),
                    new_line_number: None,
                },
            )
            .unwrap();
        builder.finish_hunk(hunk_id, 1).unwrap();
        builder.finish_file(file_id, 1).unwrap();
        builder.finish_commit(commit_id, 1).unwrap();
        let snapshot = builder.snapshot().unwrap();
        assert!(builder.line_store.is_spilled().unwrap());
        let mut reader = DocumentLineReader::new();

        let borrowed = reader.read(&snapshot, line_id).unwrap().unwrap();
        assert_eq!(borrowed.id, line_id);
        assert_eq!(borrowed.text, "-old");
        assert_eq!(borrowed.content(), "old");
        assert_eq!(borrowed.commit_id, Some(commit_id));
        assert_eq!(borrowed.file_id, Some(file_id));
        assert_eq!(borrowed.hunk_id, Some(hunk_id));
        assert_eq!(borrowed.old_line_number, Some(10));
        assert_eq!(borrowed.new_line_number, None);
        assert_eq!(
            borrowed.file_line_bounds,
            Some(StructureFileLineBounds {
                max_old_line_number: Some(10),
                max_new_line_number: None,
            })
        );
        assert!(borrowed.terminated);

        let owned = reader.read_owned(&snapshot, line_id).unwrap().unwrap();
        assert_eq!(owned.text, "-old");
        assert_eq!(owned.as_borrowed().content(), "old");

        let mut metadata_reader = DocumentLineMetadataReader::new();
        let metadata = metadata_reader.read(&snapshot, line_id).unwrap().unwrap();
        assert_eq!(metadata.id, line_id);
        assert_eq!(metadata.kind, LineKind::Deletion);
        assert_eq!(metadata.display_len, LineDisplayLength::Exact(4));
        assert!(!metadata.display_len.exceeds(4));
        assert!(metadata.display_len.exceeds(3));
        let structure_stats = metadata_reader.structure.statistics();
        assert_eq!(structure_stats.commit_records, 1);
        assert_eq!(structure_stats.file_records, 1);
        assert_eq!(structure_stats.hunk_records, 1);
        let capacity = metadata_reader.record_buffer.capacity();
        assert!(capacity >= crate::document_store::LOGICAL_LINE_RECORD_BYTES);
        let _ = metadata_reader.read(&snapshot, line_id).unwrap();
        assert_eq!(metadata_reader.record_buffer.capacity(), capacity);
    }

    #[test]
    fn append_rejects_dangling_stable_ids_before_mutating_bounds() {
        let raw = AppendStore::new();
        let storage = StructuralStorage::new().unwrap();
        let mut builder = DocumentBuilder::new(raw.clone(), &storage).unwrap();
        let mut tracker = builder.start_line_tracker().unwrap();
        let start = raw.append(b"+bad\n").unwrap();
        let mut span = None;
        for (relative, byte) in b"+bad\n".iter().copied().enumerate() {
            span = tracker
                .observe(start + relative as u64, byte)
                .unwrap()
                .or(span);
        }

        let error = builder
            .append_line(
                span.unwrap(),
                AppendDocumentLine {
                    display_len: 4,
                    kind: LineKind::Addition,
                    commit_id: Some(CommitId::new(4)),
                    file_id: Some(FileId::new(3)),
                    hunk_id: Some(HunkId::new(2)),
                    old_line_number: None,
                    new_line_number: Some(900),
                },
            )
            .unwrap_err();

        assert!(error.to_string().contains("commit 4"));
        assert_eq!(builder.line_count(), 0);
        assert!(builder.snapshot().unwrap().is_empty());
    }

    #[test]
    fn failed_record_append_does_not_advance_line_or_file_bounds() {
        let raw = AppendStore::new();
        let storage = StructuralStorage::new().unwrap();
        let mut builder = DocumentBuilder::new(raw, &storage).unwrap();
        let mut tracker = builder.start_line_tracker().unwrap();
        let (commit_id, file_id, hunk_id) = begin_rich_structure(&mut builder, LineId::new(0));

        // Trackerへはindexed emissionを渡すがraw storeには対応byteをappendしない。Boundary検証が
        // record commit前に失敗し、file boundsだけを進めないことを再現する。
        let mut span = None;
        for (offset, byte) in b"+bad\n".iter().copied().enumerate() {
            span = tracker.observe(offset as u64, byte).unwrap().or(span);
        }
        let error = builder
            .append_line(
                span.unwrap(),
                AppendDocumentLine {
                    display_len: 4,
                    kind: LineKind::Addition,
                    commit_id: Some(commit_id),
                    file_id: Some(file_id),
                    hunk_id: Some(hunk_id),
                    old_line_number: None,
                    new_line_number: Some(900),
                },
            )
            .unwrap_err();

        assert!(error.to_string().contains("raw range"));
        assert_eq!(builder.line_count(), 0);
        assert_eq!(
            builder.open_file(file_id).unwrap().line_bounds,
            StructureFileLineBounds::default()
        );
    }

    #[test]
    fn readers_reject_dangling_ids_found_in_the_fixed_record() {
        let raw = AppendStore::new();
        let storage = StructuralStorage::new().unwrap();
        let builder = DocumentBuilder::new(raw.clone(), &storage).unwrap();
        let mut tracker = builder.start_line_tracker().unwrap();
        let start = raw.append(b"x\n").unwrap();
        let mut span = None;
        for (relative, byte) in b"x\n".iter().copied().enumerate() {
            span = tracker
                .observe(start + relative as u64, byte)
                .unwrap()
                .or(span);
        }
        builder
            .line_store
            .append(
                span.unwrap(),
                LogicalLineMetadata {
                    commit_id: Some(8),
                    file_id: None,
                    hunk_id: None,
                    old_line_number: None,
                    new_line_number: None,
                    display_len: 1,
                    kind: LineKind::Plain,
                },
            )
            .unwrap();
        let snapshot = DocumentSnapshot {
            identity: builder.identity.clone(),
            revision: super::DocumentRevision::new(),
            lines: builder.line_store.snapshot().unwrap(),
            line_count: 1,
            structure: builder.structure.snapshot().unwrap(),
            complete: false,
            trailing_newline: true,
        };

        let mut line_reader = DocumentLineReader::new();
        let line_error = line_reader.read(&snapshot, LineId::new(0)).unwrap_err();
        assert!(line_error.to_string().contains("commit 8"));
        let mut metadata_reader = DocumentLineMetadataReader::new();
        let metadata_error = metadata_reader.read(&snapshot, LineId::new(0)).unwrap_err();
        assert!(metadata_error.to_string().contains("commit 8"));
    }

    #[test]
    fn structural_boundaries_are_exact_and_failed_close_keeps_the_open_id() {
        let raw = AppendStore::new();
        let storage = StructuralStorage::new().unwrap();
        let mut builder = DocumentBuilder::new(raw.clone(), &storage).unwrap();
        let mut tracker = builder.start_line_tracker().unwrap();

        let mismatch = builder
            .begin_commit(CommitKind::WorkingTree, None, LineId::new(1))
            .unwrap_err();
        assert!(matches!(
            mismatch,
            DocumentError::StructureBoundaryMismatch {
                expected: 0,
                actual: 1,
                ..
            }
        ));

        let commit_id = builder
            .begin_commit(CommitKind::WorkingTree, None, LineId::new(0))
            .unwrap();
        assert!(matches!(
            builder.snapshot(),
            Err(DocumentError::InvalidStructureSpan {
                kind: "commit",
                id: 0,
                ..
            })
        ));
        assert!(builder.finish_commit(commit_id, 0).is_err());
        assert_eq!(builder.open_commit(commit_id).unwrap().id(), commit_id);

        let span = append_tracked_line(&raw, &mut tracker, b"header\n");
        builder
            .append_line(
                span,
                AppendDocumentLine {
                    display_len: 6,
                    kind: LineKind::CommitHeader,
                    commit_id: Some(commit_id),
                    file_id: None,
                    hunk_id: None,
                    old_line_number: None,
                    new_line_number: None,
                },
            )
            .unwrap();
        let wrong_id = builder.finish_commit(CommitId::new(1), 1).unwrap_err();
        assert!(matches!(
            wrong_id,
            DocumentError::OpenStructureIdentityMismatch {
                expected: 1,
                actual: 0,
                ..
            }
        ));
        assert_eq!(builder.open_commit(commit_id).unwrap().id(), commit_id);
        assert!(matches!(
            builder.mark_complete(),
            Err(DocumentError::OpenStructureAtCompletion {
                kind: "commit",
                id: 0
            })
        ));
        builder.finish_commit(commit_id, 1).unwrap();
        builder.mark_complete().unwrap();
        assert!(builder.snapshot().unwrap().is_complete());
    }

    #[test]
    fn fallback_generation_normalizes_source_semantics_and_effective_bounds() {
        let raw = AppendStore::new();
        let storage = StructuralStorage::new().unwrap();
        let mut builder = DocumentBuilder::new(raw.clone(), &storage).unwrap();
        let mut tracker = builder.start_line_tracker().unwrap();
        let (commit_id, file_id, hunk_id) = begin_rich_structure(&mut builder, LineId::new(0));
        let span = append_tracked_line(&raw, &mut tracker, b"+new\n");
        let line_id = builder
            .append_line(
                span,
                AppendDocumentLine {
                    display_len: 4,
                    kind: LineKind::Addition,
                    commit_id: Some(commit_id),
                    file_id: Some(file_id),
                    hunk_id: Some(hunk_id),
                    old_line_number: None,
                    new_line_number: Some(20),
                },
            )
            .unwrap();
        let rich = builder.snapshot().unwrap();

        {
            let file = builder.open_file_mut(file_id).unwrap();
            file.mode = FileMode::Malformed;
            file.additions = None;
            file.deletions = None;
        }
        {
            let hunk = builder.open_hunk_mut(hunk_id).unwrap();
            hunk.kind = HunkKind::Malformed;
            hunk.old_range = None;
            hunk.new_range = None;
        }
        let fallback = builder.snapshot().unwrap();

        let mut reader = DocumentLineMetadataReader::new();
        let rich_line = reader.read(&rich, line_id).unwrap().unwrap();
        assert_eq!(rich_line.kind, LineKind::Addition);
        assert_eq!(rich_line.new_line_number, Some(20));
        assert_eq!(
            rich_line.file_line_bounds,
            Some(StructureFileLineBounds {
                max_old_line_number: None,
                max_new_line_number: Some(20),
            })
        );
        let fallback_line = reader.read(&fallback, line_id).unwrap().unwrap();
        assert_eq!(fallback_line.kind, LineKind::Fallback);
        assert_eq!(fallback_line.new_line_number, None);
        assert_eq!(
            fallback_line.file_line_bounds,
            Some(StructureFileLineBounds::default())
        );
    }

    #[test]
    fn finalized_empty_child_range_preserves_its_insertion_cursor() {
        let raw = AppendStore::new();
        let storage = StructuralStorage::new().unwrap();
        let mut builder = DocumentBuilder::new(raw.clone(), &storage).unwrap();
        let mut tracker = builder.start_line_tracker().unwrap();
        let (first_commit, first_file, first_hunk) =
            begin_rich_structure(&mut builder, LineId::new(0));
        let first_span = append_tracked_line(&raw, &mut tracker, b" first\n");
        builder
            .append_line(
                first_span,
                AppendDocumentLine {
                    display_len: 6,
                    kind: LineKind::Context,
                    commit_id: Some(first_commit),
                    file_id: Some(first_file),
                    hunk_id: Some(first_hunk),
                    old_line_number: Some(10),
                    new_line_number: Some(20),
                },
            )
            .unwrap();
        // Rangeの残数はparser所有でありDocumentBuilderはspanだけを検査する。
        builder.finish_hunk(first_hunk, 1).unwrap();
        builder.finish_file(first_file, 1).unwrap();
        builder.finish_commit(first_commit, 1).unwrap();

        let second_commit = builder
            .begin_commit(
                CommitKind::Commit,
                Some("abcdef0".to_owned()),
                LineId::new(1),
            )
            .unwrap();
        let second_span = append_tracked_line(&raw, &mut tracker, b"commit abcdef0\n");
        builder
            .append_line(
                second_span,
                AppendDocumentLine {
                    display_len: 14,
                    kind: LineKind::CommitHeader,
                    commit_id: Some(second_commit),
                    file_id: None,
                    hunk_id: None,
                    old_line_number: None,
                    new_line_number: None,
                },
            )
            .unwrap();
        builder.finish_commit(second_commit, 2).unwrap();
        let snapshot = builder.snapshot().unwrap();

        let commit = StructureMetadataReader::new()
            .read_commit(snapshot.structure(), second_commit)
            .unwrap()
            .unwrap();
        assert_eq!(commit.files.start(), 1);
        assert_eq!(commit.files.count(), 0);
        assert_eq!(commit.files.first(), None);
    }

    #[test]
    fn eof_state_is_copied_without_forging_termination_metadata() {
        let raw = AppendStore::new();
        let storage = StructuralStorage::new().unwrap();
        let mut builder = DocumentBuilder::new(raw.clone(), &storage).unwrap();
        let mut tracker = builder.start_line_tracker().unwrap();
        let start = raw.append(b"tail").unwrap();
        for (relative, byte) in b"tail".iter().copied().enumerate() {
            assert!(
                tracker
                    .observe(start + relative as u64, byte)
                    .unwrap()
                    .is_none()
            );
        }
        let high_water = raw.seal().unwrap();
        let span = tracker.seal_eof(high_water).unwrap().unwrap();
        let line_id = builder
            .append_line(
                span,
                AppendDocumentLine {
                    display_len: 4,
                    kind: LineKind::Plain,
                    commit_id: None,
                    file_id: None,
                    hunk_id: None,
                    old_line_number: None,
                    new_line_number: None,
                },
            )
            .unwrap();
        let before_complete = builder.snapshot().unwrap();
        builder.mark_complete().unwrap();
        let complete = builder.snapshot().unwrap();

        assert!(!before_complete.is_complete());
        assert!(complete.is_complete());
        assert!(!complete.has_trailing_newline());
        let line = DocumentLineReader::new()
            .read_owned(&complete, line_id)
            .unwrap()
            .unwrap();
        assert_eq!(line.text, "tail");
        assert!(!line.terminated);
    }

    #[test]
    fn snapshots_share_identity_only_with_their_origin_builder() {
        let storage = StructuralStorage::new().unwrap();
        let first = DocumentBuilder::new(AppendStore::new(), &storage).unwrap();
        let second = DocumentBuilder::new(AppendStore::new(), &storage).unwrap();

        let first_snapshot = first.snapshot().unwrap();
        let cloned_first_snapshot = first_snapshot.clone();
        let later_first_snapshot = first.snapshot().unwrap();
        let second_snapshot = second.snapshot().unwrap();

        assert_eq!(first_snapshot.identity(), later_first_snapshot.identity());
        assert_ne!(first_snapshot.identity(), second_snapshot.identity());
        assert_eq!(first_snapshot.revision(), cloned_first_snapshot.revision());
        assert_ne!(first_snapshot.revision(), later_first_snapshot.revision());
        assert_ne!(first_snapshot.revision(), second_snapshot.revision());
        let mut identities = std::collections::HashSet::new();
        assert!(identities.insert(first_snapshot.identity()));
        assert!(!identities.insert(later_first_snapshot.identity()));
        assert!(identities.insert(second_snapshot.identity()));
    }
}
