//! parser、background worker、renderer が共有する append-only document model。
//!
//! commit / file / hunk / logical line は追加時に単調増加 ID を受け取り、後続 chunk の到着で ID
//! や既存 record の順序を変えない。parser は status、mode、count、hunk kind/range、終端 offset
//! のような未確定 field だけを更新し、renderer は公開 accessor から同じ record を追跡できる。

use std::fmt;

macro_rules! stable_id {
    ($name:ident, $label:literal) => {
        #[doc = concat!($label, " を document 内で一意に識別する append-only ID。")]
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            /// document 先頭を0とする不変の通し番号を返す。
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }

            pub(crate) const fn new(value: u64) -> Self {
                Self(value)
            }

            pub(crate) const fn index(self) -> usize {
                self.0 as usize
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

stable_id!(LineId, "logical line");
stable_id!(CommitId, "commit group");
stable_id!(FileId, "file occurrence");
stable_id!(HunkId, "hunk");

/// append-only record が占める logical-line 範囲。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineSpan {
    /// 範囲に含まれる先頭 line の ID。
    pub start: LineId,
    /// 範囲の直後にある line ID。streaming 中の現行 record では `None` になる。
    pub end_exclusive: Option<u64>,
}

impl LineSpan {
    pub(crate) const fn open(start: LineId) -> Self {
        Self {
            start,
            end_exclusive: None,
        }
    }

    /// 現時点で終端が確定しているかを返す。
    #[must_use]
    pub const fn is_closed(self) -> bool {
        self.end_exclusive.is_some()
    }
}

/// commit header の由来。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitKind {
    /// `commit <object-id>` で始まる standard Git commit block。
    Commit,
    /// commit header を伴わない `git diff` の file 群。
    WorkingTree,
}

/// file entry の変更種別。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    Binary,
    Submodule,
    Unknown,
}

impl ChangeStatus {
    /// sticky header と file list で使う固定長 status label を返す。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Modified => "M",
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Copied => "C",
            Self::Binary => "binary",
            Self::Submodule => "submodule",
            Self::Unknown => "?",
        }
    }
}

/// file section をどの程度構造化できたかを表す。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileMode {
    /// single-parent unified diff。行番号と syntax / intraline highlight を適用できる。
    Rich,
    /// `diff --cc` / `diff --combined`。hunk navigation 以外は raw 表示する。
    Combined,
    /// word-diff marker を含む section。source 行番号を生成しない。
    WordDiff,
    /// gitlink または `--submodule` output。内部 file を独立 entry として解釈しない。
    Submodule,
    /// binary notice または binary patch payload。
    Binary,
    /// Git-like header はあったが unified grammar を満たさなかった section。
    Malformed,
}

impl FileMode {
    /// source 行番号と rich highlight を適用できるかを返す。
    #[must_use]
    pub const fn is_rich(self) -> bool {
        matches!(self, Self::Rich)
    }

    /// file list の count が算出不能になる fallback mode かを返す。
    #[must_use]
    pub const fn has_unknown_counts(self) -> bool {
        !self.is_rich()
    }
}

/// hunk header が持つ一方の file の line range。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceRange {
    /// hunk header に記録された開始行。空 range では挿入点を表すため0も取りうる。
    pub start: u64,
    /// hunk に含まれる logical source line 数。
    pub count: u64,
}

/// hunk の parser mode。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HunkKind {
    Unified,
    Combined,
    WordDiff,
    Submodule,
    Malformed,
}

/// input の1 logical lineを renderer が扱うための分類。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Plain,
    CommitHeader,
    CommitMetadata,
    CommitMessage,
    DiffHeader,
    FileMetadata,
    HunkHeader,
    Context,
    Deletion,
    Addition,
    NoNewlineMarker,
    Binary,
    Submodule,
    Fallback,
}

/// sanitized input の1 logical line。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentLine {
    pub id: LineId,
    /// ANSI sequence を除去し、invalid/control byte を `\\xNN` にした元の1行。
    pub text: String,
    pub kind: LineKind,
    /// plain preamble では `None`。file line は所有 commit group を参照する。
    pub commit_id: Option<CommitId>,
    pub file_id: Option<FileId>,
    pub hunk_id: Option<HunkId>,
    /// context / deletion line が参照する旧 file 行番号。
    pub old_line_number: Option<u64>,
    /// context / addition line が参照する新 file 行番号。
    pub new_line_number: Option<u64>,
    /// input で LF により終端された行か。最終 unterminated line だけ `false` になる。
    pub terminated: bool,
}

impl DocumentLine {
    /// source line では diff marker を除いた本文、それ以外では行全体を返す。
    #[must_use]
    pub fn content(&self) -> &str {
        if matches!(
            self.kind,
            LineKind::Context | LineKind::Deletion | LineKind::Addition
        ) {
            self.text.get(1..).unwrap_or("")
        } else {
            &self.text
        }
    }
}

/// standard commit block、または commit header を持たない working-tree group。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub id: CommitId,
    pub kind: CommitKind,
    /// standard header から取得した hexadecimal object ID。working-tree group では `None`。
    pub object_id: Option<String>,
    /// standard commit message の最初の非空行。未着または存在しない場合は `None`。
    pub subject: Option<String>,
    pub span: LineSpan,
    /// Git output に現れた順を維持する file occurrence ID。
    pub file_ids: Vec<FileId>,
}

/// 1回出現した file section。別 commit の同一 path は別 ID を持つ。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub id: FileId,
    pub commit_id: CommitId,
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub status: ChangeStatus,
    pub mode: FileMode,
    pub span: LineSpan,
    pub hunk_ids: Vec<HunkId>,
    /// rich mode で確定できる追加行数。fallback mode では `None` (`+?`)。
    pub additions: Option<u64>,
    /// rich mode で確定できる削除行数。fallback mode では `None` (`-?`)。
    pub deletions: Option<u64>,
}

impl FileEntry {
    /// syntax 判定と通常 sticky header に使う path を返す。
    #[must_use]
    pub fn primary_path(&self) -> Option<&str> {
        self.new_path.as_deref().or(self.old_path.as_deref())
    }

    /// rename/copy の old/new path を含む人間向け label を作る。
    #[must_use]
    pub fn display_path(&self) -> String {
        match (&self.old_path, &self.new_path) {
            (Some(old), Some(new)) if old != new => format!("{old} → {new}"),
            (Some(path), _) | (_, Some(path)) => path.clone(),
            (None, None) => "(unparsed path)".to_owned(),
        }
    }
}

/// unified または fallback hunk header とその line span。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub id: HunkId,
    pub file_id: FileId,
    pub kind: HunkKind,
    pub header_line: LineId,
    pub span: LineSpan,
    /// single-parent unified hunk 以外では `None`。
    pub old_range: Option<SourceRange>,
    /// single-parent unified hunk 以外では `None`。
    pub new_range: Option<SourceRange>,
}

/// parser がstdinの先頭から構築する document 全体。
///
/// slice の要素は削除・並べ替えされない。`is_complete` が `false` の間は末尾 record の subject、
/// status、mode、count、hunk kind/range、span end が後続 chunk により更新される。incremental
/// consumer は [`crate::parser::ParseDelta`] の `updated_*` ID を invalidate する。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    lines: Vec<DocumentLine>,
    commits: Vec<Commit>,
    files: Vec<FileEntry>,
    hunks: Vec<Hunk>,
    complete: bool,
    trailing_newline: bool,
}

impl Document {
    #[must_use]
    pub fn lines(&self) -> &[DocumentLine] {
        &self.lines
    }

    #[must_use]
    pub fn commits(&self) -> &[Commit] {
        &self.commits
    }

    #[must_use]
    pub fn files(&self) -> &[FileEntry] {
        &self.files
    }

    #[must_use]
    pub fn hunks(&self) -> &[Hunk] {
        &self.hunks
    }

    #[must_use]
    pub fn line(&self, id: LineId) -> Option<&DocumentLine> {
        self.lines.get(id.index())
    }

    #[must_use]
    pub fn commit(&self, id: CommitId) -> Option<&Commit> {
        self.commits.get(id.index())
    }

    #[must_use]
    pub fn file(&self, id: FileId) -> Option<&FileEntry> {
        self.files.get(id.index())
    }

    #[must_use]
    pub fn hunk(&self, id: HunkId) -> Option<&Hunk> {
        self.hunks.get(id.index())
    }

    /// EOF を受け取り、末尾 record の span と hunk count 検証まで終えたかを返す。
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.complete
    }

    /// sanitized input の最終 byte が LF だったかを返す。
    #[must_use]
    pub const fn has_trailing_newline(&self) -> bool {
        self.trailing_newline
    }

    /// file grammar を1件も検出していない plain pager document かを返す。
    #[must_use]
    pub fn is_plain(&self) -> bool {
        self.files.is_empty()
    }

    pub(crate) fn push_line(&mut self, mut line: DocumentLine) -> LineId {
        let id = LineId::new(self.lines.len() as u64);
        line.id = id;
        self.trailing_newline = line.terminated;
        self.lines.push(line);
        id
    }

    pub(crate) fn push_commit(&mut self, mut commit: Commit) -> CommitId {
        let id = CommitId::new(self.commits.len() as u64);
        commit.id = id;
        self.commits.push(commit);
        id
    }

    pub(crate) fn push_file(&mut self, mut file: FileEntry) -> FileId {
        let id = FileId::new(self.files.len() as u64);
        file.id = id;
        self.files.push(file);
        id
    }

    pub(crate) fn push_hunk(&mut self, mut hunk: Hunk) -> HunkId {
        let id = HunkId::new(self.hunks.len() as u64);
        hunk.id = id;
        self.hunks.push(hunk);
        id
    }

    pub(crate) fn commit_mut(&mut self, id: CommitId) -> Option<&mut Commit> {
        self.commits.get_mut(id.index())
    }

    pub(crate) fn file_mut(&mut self, id: FileId) -> Option<&mut FileEntry> {
        self.files.get_mut(id.index())
    }

    pub(crate) fn hunk_mut(&mut self, id: HunkId) -> Option<&mut Hunk> {
        self.hunks.get_mut(id.index())
    }

    pub(crate) fn mark_complete(&mut self) {
        self.complete = true;
    }
}
