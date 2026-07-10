//! Raw logical-line table と Git structure を同じ high-water mark で公開する document layer。
//!
//! [`DocumentBuilder`] は parser thread だけが所有し、line text を保持せず
//! [`crate::document_store::DocumentLineStore`] へ固定長 metadata を追記する。Commit / file / hunk と
//! file ごとの最大行番号は copy-on-write の `Arc<Vec<_>>` に置き、[`DocumentSnapshot`] は line table の
//! high-water mark と各 `Arc` をcloneするだけで取得する。Snapshot取得後の追記や末尾recordの更新は
//! `Arc::make_mut` で分離され、既にworkerへ渡したsnapshotを書き換えない。
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
use crate::model::{Commit, CommitId, FileEntry, FileId, Hunk, HunkId, LineId, LineKind};
use crate::storage::StructuralStorage;

/// Document snapshotの構築、metadata検証、lazy decodeに失敗した理由。
#[derive(Debug, Error)]
pub enum DocumentError {
    #[error(transparent)]
    LineStore(#[from] LineStoreError),
    #[error("logical line count {count} does not fit a stable u64 ID")]
    LineCountOverflow { count: usize },
    #[error("logical line display length {length} does not fit the on-disk u64 field")]
    DisplayLengthOverflow { length: usize },
    #[error(
        "document builder expects {expected} logical lines, but its line store contains {actual}"
    )]
    LineStoreOutOfSync { expected: u64, actual: u64 },
    #[error("document contains {file_count} files but {bounds_count} file line-bound records")]
    FileBoundsOutOfSync {
        file_count: usize,
        bounds_count: usize,
    },
    #[error(
        "logical line {line_id} references {kind} {referenced_id}, but only {available} records exist"
    )]
    InvalidLineReference {
        line_id: LineId,
        kind: &'static str,
        referenced_id: u64,
        available: usize,
    },
    #[error(
        "logical line {line_id} references {kind} {referenced_id}, but that slot stores ID {stored_id}"
    )]
    InvalidLineReferenceIdentity {
        line_id: LineId,
        kind: &'static str,
        referenced_id: u64,
        stored_id: u64,
    },
    #[error(
        "{record_kind} {record_id} references {referenced_kind} {referenced_id}, but only {available} records exist"
    )]
    InvalidStructureReference {
        record_kind: &'static str,
        record_id: u64,
        referenced_kind: &'static str,
        referenced_id: u64,
        available: usize,
    },
    #[error("{kind} slot {expected_id} stores stable ID {stored_id}")]
    InvalidStructureIdentity {
        kind: &'static str,
        expected_id: u64,
        stored_id: u64,
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
    #[error("could not allocate {requested} bytes for an owned logical line")]
    OwnedLineAllocation { requested: usize },
}

/// File内のsource lineを揃えるgutter幅の算出に使う最大行番号。
///
/// 値はparserがline metadataをappendした時だけ増加する。Snapshotはfile recordと同じCOW世代の
/// vectorを保持するため、後続lineで桁数が増えても以前のsnapshotのreflow条件は変わらない。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileLineBounds {
    /// File内で旧側gutterへ表示する最大行番号。旧側を消費するlineが未着なら`None`。
    pub max_old_line_number: Option<u64>,
    /// File内で新側gutterへ表示する最大行番号。新側を消費するlineが未着なら`None`。
    pub max_new_line_number: Option<u64>,
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

impl FileLineBounds {
    fn observe(&mut self, old: Option<u64>, new: Option<u64>) {
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
    /// Parserがrecord append時に確定した表示上のline分類。
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
/// Cloneはraw store/table handleと4つの`Arc<Vec<_>>`だけを増やす。Line textは含まず、後から
/// builderへappendされたlineは[`Self::line_count`]にもreaderにも現れない。
#[derive(Clone)]
pub struct DocumentSnapshot {
    identity: DocumentIdentity,
    lines: LineSnapshot,
    line_count: usize,
    commits: Arc<Vec<Commit>>,
    files: Arc<Vec<FileEntry>>,
    hunks: Arc<Vec<Hunk>>,
    file_line_bounds: Arc<Vec<FileLineBounds>>,
    complete: bool,
    trailing_newline: bool,
}

impl DocumentSnapshot {
    /// このsnapshotを生成したbuilder lineageを返す。
    #[must_use]
    pub fn identity(&self) -> DocumentIdentity {
        self.identity.clone()
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

    /// Snapshot取得時点のcommit groupをGit出力順で返す。
    #[must_use]
    pub fn commits(&self) -> &[Commit] {
        self.commits.as_slice()
    }

    /// Snapshot取得時点のfile occurrenceをGit出力順で返す。
    #[must_use]
    pub fn files(&self) -> &[FileEntry] {
        self.files.as_slice()
    }

    /// Snapshot取得時点のhunkをGit出力順で返す。
    #[must_use]
    pub fn hunks(&self) -> &[Hunk] {
        self.hunks.as_slice()
    }

    /// Stable IDのcommitを返す。Snapshot外のIDなら`None`。
    #[must_use]
    pub fn commit(&self, id: CommitId) -> Option<&Commit> {
        checked_index(id.get(), self.commits.len())
            .and_then(|index| self.commits.get(index))
            .filter(|commit| commit.id == id)
    }

    /// Stable IDのfile occurrenceを返す。Snapshot外のIDなら`None`。
    #[must_use]
    pub fn file(&self, id: FileId) -> Option<&FileEntry> {
        checked_index(id.get(), self.files.len())
            .and_then(|index| self.files.get(index))
            .filter(|file| file.id == id)
    }

    /// Stable IDのhunkを返す。Snapshot外のIDなら`None`。
    #[must_use]
    pub fn hunk(&self, id: HunkId) -> Option<&Hunk> {
        checked_index(id.get(), self.hunks.len())
            .and_then(|index| self.hunks.get(index))
            .filter(|hunk| hunk.id == id)
    }

    /// File IDに対応する旧/新source lineの最大値をO(1)で返す。
    ///
    /// Streaming中に蓄積した番号の後でfileがfallbackへdowngradeされたsnapshotでは、gutterを再表示
    /// しないよう両側`None`のboundsを返す。以前に取得したrich snapshotのboundsは変化しない。
    #[must_use]
    pub fn file_line_bounds(&self, id: FileId) -> Option<FileLineBounds> {
        let file = self.file(id)?;
        if !file.mode.is_rich() {
            return Some(FileLineBounds::default());
        }
        checked_index(id.get(), self.file_line_bounds.len())
            .and_then(|index| self.file_line_bounds.get(index))
            .copied()
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
    pub fn is_plain(&self) -> bool {
        self.files.is_empty()
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
            .field("commit_count", &self.commits.len())
            .field("file_count", &self.files.len())
            .field("hunk_count", &self.hunks.len())
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
}

impl DocumentLineReader {
    /// Allocation前の空bufferを持つreaderを作る。各bufferは最初に必要になったreadで確保する。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            decoder: LineDecoder::new(),
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
        let Some(decoded) = self.decoder.decode(&document.lines, line_id.get())? else {
            return Ok(None);
        };
        let metadata = metadata_from_record(document, line_id, decoded.record)?;
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
}

impl DocumentLineMetadataReader {
    /// Spill record用bufferをまだ確保していない空readerを作る。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            record_buffer: Vec::new(),
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
        metadata_from_record(document, line_id, record).map(Some)
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
    commits: Arc<Vec<Commit>>,
    files: Arc<Vec<FileEntry>>,
    hunks: Arc<Vec<Hunk>>,
    file_line_bounds: Arc<Vec<FileLineBounds>>,
    complete: bool,
    trailing_newline: bool,
}

impl fmt::Debug for DocumentBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DocumentBuilder")
            .field("line_count", &self.line_count)
            .field("commit_count", &self.commits.len())
            .field("file_count", &self.files.len())
            .field("hunk_count", &self.hunks.len())
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
            commits: Arc::new(Vec::new()),
            files: Arc::new(Vec::new()),
            hunks: Arc::new(Vec::new()),
            file_line_bounds: Arc::new(Vec::new()),
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

    #[must_use]
    pub(crate) fn commits(&self) -> &[Commit] {
        self.commits.as_slice()
    }

    #[must_use]
    pub(crate) fn files(&self) -> &[FileEntry] {
        self.files.as_slice()
    }

    #[must_use]
    pub(crate) fn hunks(&self) -> &[Hunk] {
        self.hunks.as_slice()
    }

    #[must_use]
    pub(crate) fn commit(&self, id: CommitId) -> Option<&Commit> {
        checked_index(id.get(), self.commits.len())
            .and_then(|index| self.commits.get(index))
            .filter(|commit| commit.id == id)
    }

    #[must_use]
    pub(crate) fn file(&self, id: FileId) -> Option<&FileEntry> {
        checked_index(id.get(), self.files.len())
            .and_then(|index| self.files.get(index))
            .filter(|file| file.id == id)
    }

    pub(crate) fn commit_mut(&mut self, id: CommitId) -> Option<&mut Commit> {
        let index = checked_index(id.get(), self.commits.len())?;
        Arc::make_mut(&mut self.commits)
            .get_mut(index)
            .filter(|commit| commit.id == id)
    }

    pub(crate) fn file_mut(&mut self, id: FileId) -> Option<&mut FileEntry> {
        let index = checked_index(id.get(), self.files.len())?;
        Arc::make_mut(&mut self.files)
            .get_mut(index)
            .filter(|file| file.id == id)
    }

    pub(crate) fn hunk_mut(&mut self, id: HunkId) -> Option<&mut Hunk> {
        let index = checked_index(id.get(), self.hunks.len())?;
        Arc::make_mut(&mut self.hunks)
            .get_mut(index)
            .filter(|hunk| hunk.id == id)
    }

    /// Commitをappendし、callerが渡したIDとchild listをbuilder所有値へ置き換える。
    pub(crate) fn push_commit(&mut self, mut commit: Commit) -> CommitId {
        let id = CommitId::new(self.commits.len() as u64);
        commit.id = id;
        commit.file_ids.clear();
        Arc::make_mut(&mut self.commits).push(commit);
        id
    }

    /// Fileをappendし、parent commitのfile listとbounds vectorを同じCOW世代へ更新する。
    pub(crate) fn push_file(&mut self, mut file: FileEntry) -> Result<FileId, DocumentError> {
        let id = FileId::new(self.files.len() as u64);
        let Some(commit_index) = checked_index(file.commit_id.get(), self.commits.len()) else {
            return Err(DocumentError::InvalidStructureReference {
                record_kind: "file",
                record_id: id.get(),
                referenced_kind: "commit",
                referenced_id: file.commit_id.get(),
                available: self.commits.len(),
            });
        };
        if let Some(commit) = self.commits.get(commit_index)
            && commit.id != file.commit_id
        {
            return Err(DocumentError::InvalidStructureIdentity {
                kind: "commit",
                expected_id: file.commit_id.get(),
                stored_id: commit.id.get(),
            });
        }
        file.id = id;
        file.hunk_ids.clear();
        Arc::make_mut(&mut self.files).push(file);
        Arc::make_mut(&mut self.file_line_bounds).push(FileLineBounds::default());
        if let Some(commit) = Arc::make_mut(&mut self.commits).get_mut(commit_index) {
            commit.file_ids.push(id);
        }
        Ok(id)
    }

    /// Hunkをappendし、parent fileのhunk listへ同じstable IDを追加する。
    pub(crate) fn push_hunk(&mut self, mut hunk: Hunk) -> Result<HunkId, DocumentError> {
        let id = HunkId::new(self.hunks.len() as u64);
        let Some(file_index) = checked_index(hunk.file_id.get(), self.files.len()) else {
            return Err(DocumentError::InvalidStructureReference {
                record_kind: "hunk",
                record_id: id.get(),
                referenced_kind: "file",
                referenced_id: hunk.file_id.get(),
                available: self.files.len(),
            });
        };
        if let Some(file) = self.files.get(file_index)
            && file.id != hunk.file_id
        {
            return Err(DocumentError::InvalidStructureIdentity {
                kind: "file",
                expected_id: hunk.file_id.get(),
                stored_id: file.id.get(),
            });
        }
        hunk.id = id;
        Arc::make_mut(&mut self.hunks).push(hunk);
        if let Some(file) = Arc::make_mut(&mut self.files).get_mut(file_index) {
            file.hunk_ids.push(id);
        }
        Ok(id)
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
        if self.files.len() != self.file_line_bounds.len() {
            return Err(DocumentError::FileBoundsOutOfSync {
                file_count: self.files.len(),
                bounds_count: self.file_line_bounds.len(),
            });
        }
        let line_id = LineId::new(expected);
        validate_line_references(
            line_id,
            line.commit_id,
            line.file_id,
            line.hunk_id,
            line.old_line_number,
            line.new_line_number,
            &self.commits,
            &self.files,
            &self.hunks,
        )?;
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
            && let Some(index) = checked_index(file_id.get(), self.file_line_bounds.len())
            && let Some(bounds) = Arc::make_mut(&mut self.file_line_bounds).get_mut(index)
        {
            bounds.observe(line.old_line_number, line.new_line_number);
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
        if self.files.len() != self.file_line_bounds.len() {
            return Err(DocumentError::FileBoundsOutOfSync {
                file_count: self.files.len(),
                bounds_count: self.file_line_bounds.len(),
            });
        }
        Ok(DocumentSnapshot {
            identity: self.identity.clone(),
            lines,
            line_count: self.line_count,
            commits: Arc::clone(&self.commits),
            files: Arc::clone(&self.files),
            hunks: Arc::clone(&self.hunks),
            file_line_bounds: Arc::clone(&self.file_line_bounds),
            complete: self.complete,
            trailing_newline: self.trailing_newline,
        })
    }

    pub(crate) fn mark_complete(&mut self) {
        self.complete = true;
    }
}

fn metadata_from_record(
    document: &DocumentSnapshot,
    line_id: LineId,
    record: LogicalLineRecord,
) -> Result<DocumentLineMetadata, DocumentError> {
    let commit_id = record.commit_id().map(CommitId::new);
    let file_id = record.file_id().map(FileId::new);
    let hunk_id = record.hunk_id().map(HunkId::new);
    validate_line_references(
        line_id,
        commit_id,
        file_id,
        hunk_id,
        record.old_line_number(),
        record.new_line_number(),
        &document.commits,
        &document.files,
        &document.hunks,
    )?;
    let display_len = if record.display_len_overflowed() {
        LineDisplayLength::Overflowed
    } else {
        LineDisplayLength::Exact(record.display_len())
    };
    let mut kind = record.kind();
    let mut old_line_number = record.old_line_number();
    let mut new_line_number = record.new_line_number();
    let file_is_fallback = file_id
        .and_then(|id| document.file(id))
        .is_some_and(|file| !file.mode.is_rich());
    let hunk_is_fallback = hunk_id
        .and_then(|id| document.hunk(id))
        .is_some_and(|hunk| hunk.kind != crate::model::HunkKind::Unified);
    if is_source_line(kind) && (file_is_fallback || hunk_is_fallback) {
        // Hunk bodyがstream中はrichでも、EOF時の不足行や後続のunsupported markerでfile全体が
        // fallbackへ変わり得る。Fixed line recordはappend-onlyなので、snapshot世代のparent modeを
        // typed viewへ反映し、過去に記録したsource番号やstyleを後のsnapshotへ漏らさない。
        kind = LineKind::Fallback;
        old_line_number = None;
        new_line_number = None;
    }
    Ok(DocumentLineMetadata {
        id: line_id,
        kind,
        commit_id,
        file_id,
        hunk_id,
        old_line_number,
        new_line_number,
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
        terminated: metadata.terminated,
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_line_references(
    line_id: LineId,
    commit_id: Option<CommitId>,
    file_id: Option<FileId>,
    hunk_id: Option<HunkId>,
    old_line_number: Option<u64>,
    new_line_number: Option<u64>,
    commits: &[Commit],
    files: &[FileEntry],
    hunks: &[Hunk],
) -> Result<(), DocumentError> {
    if let Some(id) = commit_id {
        validate_line_reference(line_id, "commit", id.get(), commits.len())?;
        if let Some(commit) =
            checked_index(id.get(), commits.len()).and_then(|index| commits.get(index))
            && commit.id != id
        {
            return Err(DocumentError::InvalidLineReferenceIdentity {
                line_id,
                kind: "commit",
                referenced_id: id.get(),
                stored_id: commit.id.get(),
            });
        }
    }
    let file = if let Some(id) = file_id {
        validate_line_reference(line_id, "file", id.get(), files.len())?;
        let index = checked_index(id.get(), files.len());
        let file = index.and_then(|value| files.get(value));
        if let Some(file) = file
            && file.id != id
        {
            return Err(DocumentError::InvalidLineReferenceIdentity {
                line_id,
                kind: "file",
                referenced_id: id.get(),
                stored_id: file.id.get(),
            });
        }
        file
    } else {
        None
    };
    let hunk = if let Some(id) = hunk_id {
        validate_line_reference(line_id, "hunk", id.get(), hunks.len())?;
        let index = checked_index(id.get(), hunks.len());
        let hunk = index.and_then(|value| hunks.get(value));
        if let Some(hunk) = hunk
            && hunk.id != id
        {
            return Err(DocumentError::InvalidLineReferenceIdentity {
                line_id,
                kind: "hunk",
                referenced_id: id.get(),
                stored_id: hunk.id.get(),
            });
        }
        hunk
    } else {
        None
    };

    if let Some(file_id) = file_id {
        let Some(actual_commit) = commit_id else {
            return Err(DocumentError::FileWithoutCommit {
                line_id,
                child_id: file_id,
            });
        };
        if let Some(file) = file
            && file.commit_id != actual_commit
        {
            return Err(DocumentError::FileCommitMismatch {
                line_id,
                file_id,
                expected: file.commit_id,
                actual: actual_commit,
            });
        }
    }
    if let Some(hunk_id) = hunk_id {
        let Some(actual_file) = file_id else {
            return Err(DocumentError::HunkWithoutFile {
                line_id,
                child_id: hunk_id,
            });
        };
        if let Some(hunk) = hunk
            && hunk.file_id != actual_file
        {
            return Err(DocumentError::HunkFileMismatch {
                line_id,
                hunk_id,
                expected: hunk.file_id,
                actual: actual_file,
            });
        }
    }
    if file_id.is_none() && (old_line_number.is_some() || new_line_number.is_some()) {
        return Err(DocumentError::LineNumberWithoutFile { line_id });
    }
    Ok(())
}

fn validate_line_reference(
    line_id: LineId,
    kind: &'static str,
    referenced_id: u64,
    available: usize,
) -> Result<(), DocumentError> {
    if checked_index(referenced_id, available).is_none() {
        return Err(DocumentError::InvalidLineReference {
            line_id,
            kind,
            referenced_id,
            available,
        });
    }
    Ok(())
}

fn checked_index(value: u64, len: usize) -> Option<usize> {
    usize::try_from(value).ok().filter(|index| *index < len)
}

fn is_source_line(kind: LineKind) -> bool {
    matches!(
        kind,
        LineKind::Context | LineKind::Deletion | LineKind::Addition
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::document_store::LogicalLineMetadata;
    use crate::input::AppendStore;
    use crate::model::{
        ChangeStatus, Commit, CommitId, CommitKind, FileEntry, FileId, FileMode, Hunk, HunkId,
        HunkKind, LineId, LineKind, LineSpan, SourceRange,
    };
    use crate::storage::{StorageConfig, StructuralStorage};

    use super::{
        AppendDocumentLine, DocumentBuilder, DocumentLineMetadataReader, DocumentLineReader,
        DocumentSnapshot, FileLineBounds, LineDisplayLength,
    };

    fn commit() -> Commit {
        Commit {
            id: CommitId::new(0),
            kind: CommitKind::WorkingTree,
            object_id: None,
            subject: None,
            span: LineSpan::open(LineId::new(0)),
            file_ids: Vec::new(),
        }
    }

    fn file(commit_id: CommitId) -> FileEntry {
        FileEntry {
            id: FileId::new(0),
            commit_id,
            old_path: Some("src/lib.rs".to_owned()),
            new_path: Some("src/lib.rs".to_owned()),
            status: ChangeStatus::Modified,
            mode: FileMode::Rich,
            span: LineSpan::open(LineId::new(0)),
            hunk_ids: Vec::new(),
            additions: Some(0),
            deletions: Some(0),
        }
    }

    fn hunk(file_id: FileId) -> Hunk {
        Hunk {
            id: HunkId::new(0),
            file_id,
            kind: HunkKind::Unified,
            header_line: LineId::new(0),
            span: LineSpan::open(LineId::new(0)),
            old_range: Some(SourceRange {
                start: 10,
                count: 2,
            }),
            new_range: Some(SourceRange {
                start: 20,
                count: 2,
            }),
        }
    }

    #[test]
    fn snapshot_freezes_lines_structures_and_file_line_bounds() {
        let raw = AppendStore::new();
        let storage = StructuralStorage::new().unwrap();
        let mut builder = DocumentBuilder::new(raw.clone(), &storage).unwrap();
        let mut tracker = builder.start_line_tracker().unwrap();

        let commit_id = builder.push_commit(commit());
        let file_id = builder.push_file(file(commit_id)).unwrap();
        let hunk_id = builder.push_hunk(hunk(file_id)).unwrap();

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

        builder.file_mut(file_id).unwrap().additions = Some(1);
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
        assert_eq!(earlier.files()[0].additions, Some(0));
        assert_eq!(later.files()[0].additions, Some(1));
        assert!(Arc::ptr_eq(&earlier.commits, &later.commits));
        assert!(Arc::ptr_eq(&earlier.hunks, &later.hunks));
        assert!(!Arc::ptr_eq(&earlier.files, &later.files));
        assert!(!Arc::ptr_eq(
            &earlier.file_line_bounds,
            &later.file_line_bounds
        ));
        assert_eq!(
            earlier.file_line_bounds(file_id),
            Some(FileLineBounds {
                max_old_line_number: Some(10),
                max_new_line_number: Some(20),
            })
        );
        assert_eq!(
            later.file_line_bounds(file_id),
            Some(FileLineBounds {
                max_old_line_number: Some(10),
                max_new_line_number: Some(99),
            })
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
    fn line_reader_returns_typed_borrowed_and_owned_views() {
        let raw = AppendStore::with_memory_limit(0);
        let config = StorageConfig::new(16 * 1024, 16 * 1024, 64, 64).unwrap();
        let storage = StructuralStorage::with_config(config).unwrap();
        let mut builder = DocumentBuilder::new(raw.clone(), &storage).unwrap();
        let mut tracker = builder.start_line_tracker().unwrap();
        let commit_id = builder.push_commit(commit());
        let file_id = builder.push_file(file(commit_id)).unwrap();
        let hunk_id = builder.push_hunk(hunk(file_id)).unwrap();

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
        let capacity = metadata_reader.record_buffer.capacity();
        assert!(capacity >= crate::document_store::LOGICAL_LINE_RECORD_BYTES);
        let _ = metadata_reader.read(&snapshot, line_id).unwrap();
        assert_eq!(metadata_reader.record_buffer.capacity(), capacity);

        let mut corrupted = snapshot.clone();
        Arc::make_mut(&mut corrupted.commits)[0].id = CommitId::new(7);
        let identity_error = DocumentLineReader::new()
            .read_owned(&corrupted, line_id)
            .unwrap_err();
        assert!(identity_error.to_string().contains("stores ID 7"));
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
        let commit_id = builder.push_commit(commit());
        let file_id = builder.push_file(file(commit_id)).unwrap();
        let hunk_id = builder.push_hunk(hunk(file_id)).unwrap();

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
            builder.snapshot().unwrap().file_line_bounds(file_id),
            Some(FileLineBounds::default())
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
            lines: builder.line_store.snapshot().unwrap(),
            line_count: 1,
            commits: Arc::new(Vec::new()),
            files: Arc::new(Vec::new()),
            hunks: Arc::new(Vec::new()),
            file_line_bounds: Arc::new(Vec::new()),
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
        builder.mark_complete();
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
        let later_first_snapshot = first.snapshot().unwrap();
        let second_snapshot = second.snapshot().unwrap();

        assert_eq!(first_snapshot.identity(), later_first_snapshot.identity());
        assert_ne!(first_snapshot.identity(), second_snapshot.identity());
        let mut identities = std::collections::HashSet::new();
        assert!(identities.insert(first_snapshot.identity()));
        assert!(!identities.insert(later_first_snapshot.identity()));
        assert!(identities.insert(second_snapshot.identity()));
    }
}
