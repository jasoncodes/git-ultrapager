//! parser、document store、renderer が共有するstructural recordとstable ID。
//!
//! commit / file / hunk / logical line は追加時に単調増加 ID を受け取り、後続 chunk の到着でIDや
//! record順を変えない。Logical-line textとsnapshot ownershipは[`crate::document`]が持ち、このmoduleは
//! Git structureの値型だけを定義する。

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
