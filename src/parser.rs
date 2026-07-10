//! sanitized Git output を commit / file / hunk 構造へ変換する incremental parser。
//!
//! parser は repository や Git process を参照せず、stdin に現れた standard header だけを根拠に
//! model を構築する。single-parent unified diff は source 行番号まで復元し、combined / word /
//! submodule / binary / malformed section は file identity と navigation anchor を残して raw 表示へ
//! downgrade する。

use std::mem;
use std::ops::Range;

use crate::ansi::{AnsiStripper, escape_interactive_bytes, escape_path_bytes};
use crate::model::{
    ChangeStatus, Commit, CommitId, CommitKind, Document, DocumentLine, FileEntry, FileId,
    FileMode, Hunk, HunkId, HunkKind, LineId, LineKind, LineSpan, SourceRange,
};

/// `push` / `finish` の1回で追加された append-only record 範囲。
///
/// renderer はこの range だけを index/cache へ反映し、既存 ID の再採番を行わない。status や
/// `updated_*` は呼び出し前から存在し、今回の chunk で field が変わり得る末尾 record を示す。
/// false positive を許す conservative invalidation なので、cache は append range の追加前に該当 ID
/// を invalidate し、[`GitParser::document`] から読み直す。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseDelta {
    pub lines: Range<usize>,
    pub commits: Range<usize>,
    pub files: Range<usize>,
    pub hunks: Range<usize>,
    pub updated_commit: Option<CommitId>,
    pub updated_file: Option<FileId>,
    pub updated_hunk: Option<HunkId>,
    pub completed: bool,
}

#[derive(Clone, Copy, Debug)]
struct Snapshot {
    lines: usize,
    commits: usize,
    files: usize,
    hunks: usize,
    open_commit: Option<CommitId>,
    open_file: Option<FileId>,
    open_hunk: Option<HunkId>,
}

impl Snapshot {
    fn from_parser(parser: &GitParser) -> Self {
        Self {
            lines: parser.document.lines().len(),
            commits: parser.document.commits().len(),
            files: parser.document.files().len(),
            hunks: parser.document.hunks().len(),
            open_commit: parser.current_commit,
            open_file: parser.current_file,
            open_hunk: parser.current_hunk.map(|cursor| cursor.id),
        }
    }

    fn delta(self, document: &Document, completed: bool, may_update: bool) -> ParseDelta {
        ParseDelta {
            lines: self.lines..document.lines().len(),
            commits: self.commits..document.commits().len(),
            files: self.files..document.files().len(),
            hunks: self.hunks..document.hunks().len(),
            updated_commit: may_update.then_some(self.open_commit).flatten(),
            updated_file: may_update.then_some(self.open_file).flatten(),
            updated_hunk: may_update.then_some(self.open_hunk).flatten(),
            completed,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct HunkCursor {
    id: HunkId,
    old_next: u64,
    new_next: u64,
    old_remaining: u64,
    new_remaining: u64,
}

/// `Err` 側にも decode できた path を残し、malformed fallback の file identity に使う。
type ParsedPaths = Result<(Option<String>, Option<String>), (Option<String>, Option<String>)>;

/// Git byte stream を行単位で追記する parser。
///
/// `push` は任意の byte/chunk 境界を受け付け、ANSI sequence と partial line を内部に保持する。
/// EOF では [`GitParser::finish`] を1回呼ぶ。`finish` と完了後の `push` は idempotent で、既存
/// document を変更しない。
#[derive(Debug, Default)]
pub struct GitParser {
    document: Document,
    ansi: AnsiStripper,
    pending_line: Vec<u8>,
    current_commit: Option<CommitId>,
    current_file: Option<FileId>,
    current_hunk: Option<HunkCursor>,
    awaiting_new_file_header: bool,
    awaiting_hunk_header: bool,
    finished: bool,
}

impl GitParser {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// stdin の次の chunk を取り込み、新しく追加された record range を返す。
    pub fn push(&mut self, input: &[u8]) -> ParseDelta {
        let before = Snapshot::from_parser(self);
        if self.finished {
            return before.delta(&self.document, true, false);
        }

        let mut sanitized = Vec::with_capacity(input.len());
        self.ansi.push(input, &mut sanitized);
        self.consume_sanitized(&sanitized);
        before.delta(&self.document, false, !input.is_empty())
    }

    /// EOF を確定し、unterminated final line と開いている span を閉じる。
    pub fn finish(&mut self) -> ParseDelta {
        let before = Snapshot::from_parser(self);
        if self.finished {
            return before.delta(&self.document, true, false);
        }

        self.ansi.finish();
        if !self.pending_line.is_empty() {
            let bytes = mem::take(&mut self.pending_line);
            self.process_line(escape_interactive_bytes(&bytes), false);
        }

        let end = self.next_line_id();
        self.close_file(end);
        self.close_commit(end);
        self.document.mark_complete();
        self.finished = true;
        before.delta(&self.document, true, true)
    }

    /// 現在までに確定した append-only document を借用する。
    #[must_use]
    pub const fn document(&self) -> &Document {
        &self.document
    }

    /// EOF 処理を行い、所有する document を返す。
    #[must_use]
    pub fn into_document(mut self) -> Document {
        let _ = self.finish();
        self.document
    }

    fn consume_sanitized(&mut self, input: &[u8]) {
        let mut start = 0;
        for (index, &byte) in input.iter().enumerate() {
            if byte != b'\n' {
                continue;
            }

            self.pending_line.extend_from_slice(&input[start..index]);
            if self.pending_line.last() == Some(&b'\r') {
                let _ = self.pending_line.pop();
            }
            let bytes = mem::take(&mut self.pending_line);
            self.process_line(escape_interactive_bytes(&bytes), true);
            start = index.saturating_add(1);
        }
        self.pending_line.extend_from_slice(&input[start..]);
    }

    fn process_line(&mut self, text: String, terminated: bool) {
        let line_id = self.next_line_id();

        if let Some(object_id) = parse_commit_header(&text) {
            self.close_file(line_id);
            self.close_commit(line_id);
            let commit_id = self.document.push_commit(Commit {
                id: CommitId::new(0),
                kind: CommitKind::Commit,
                object_id: Some(object_id),
                subject: None,
                span: LineSpan::open(line_id),
                file_ids: Vec::new(),
            });
            self.current_commit = Some(commit_id);
            self.append_line(
                text,
                terminated,
                LineKind::CommitHeader,
                Some(commit_id),
                None,
                None,
                None,
                None,
            );
            return;
        }

        // `--submodule=log` は環境によって `diff --git` を出さず、summary 行から始まる。
        // その場合も外側の submodule path を1 file occurrence として navigation 対象にする。
        if let Some(path) = parse_submodule_path(&text) {
            let belongs_to_current = self
                .current_file
                .and_then(|id| self.document.file(id))
                .and_then(FileEntry::primary_path)
                .is_some_and(|current| current == path);
            if !belongs_to_current {
                self.start_file(
                    Ok((Some(path.clone()), Some(path))),
                    FileMode::Submodule,
                    line_id,
                );
            } else {
                self.mark_current_file_mode(FileMode::Submodule);
            }
            self.append_current_file_line(text, terminated, LineKind::Submodule, None, None);
            return;
        }

        if let Some(rest) = text.strip_prefix("diff --git ") {
            let paths = parse_diff_git_paths(rest);
            if self.paths_are_inside_current_submodule(&paths) {
                self.close_hunk(line_id);
                self.append_current_file_line(text, terminated, LineKind::Submodule, None, None);
                return;
            }
            self.start_file(paths, FileMode::Rich, line_id);
            self.append_current_file_line(text, terminated, LineKind::DiffHeader, None, None);
            return;
        }

        if let Some(rest) = text
            .strip_prefix("diff --cc ")
            .or_else(|| text.strip_prefix("diff --combined "))
        {
            let path = decode_git_path(rest.trim());
            let paths = path
                .map(|value| (Some(value.clone()), Some(value)))
                .ok_or((None, None));
            self.start_file(paths, FileMode::Combined, line_id);
            self.append_current_file_line(text, terminated, LineKind::DiffHeader, None, None);
            return;
        }

        if self.current_file.is_some() {
            self.process_file_line(text, terminated, line_id);
        } else {
            self.process_non_file_line(text, terminated);
        }
    }

    fn process_non_file_line(&mut self, text: String, terminated: bool) {
        let commit_id = self.current_commit;
        let kind = if let Some(id) = commit_id {
            if text.starts_with("    ") {
                let subject = text.trim();
                if !subject.is_empty()
                    && self
                        .document
                        .commit(id)
                        .is_some_and(|commit| commit.subject.is_none())
                    && let Some(commit) = self.document.commit_mut(id)
                {
                    commit.subject = Some(subject.to_owned());
                }
                LineKind::CommitMessage
            } else {
                LineKind::CommitMetadata
            }
        } else {
            LineKind::Plain
        };

        self.append_line(text, terminated, kind, commit_id, None, None, None, None);
    }

    fn process_file_line(&mut self, text: String, terminated: bool, line_id: LineId) {
        if self.awaiting_new_file_header && !text.starts_with("+++ ") {
            self.mark_current_file_mode(FileMode::Malformed);
            self.awaiting_new_file_header = false;
        }
        if self.awaiting_hunk_header && !text.starts_with("@@") && !is_binary_line(&text) {
            self.mark_current_file_mode(FileMode::Malformed);
            self.awaiting_hunk_header = false;
        }

        if text.starts_with("@@@") {
            self.awaiting_hunk_header = false;
            self.close_hunk(line_id);
            self.mark_current_file_mode(FileMode::Combined);
            self.start_fallback_hunk(line_id, HunkKind::Combined);
            self.append_current_file_line(text, terminated, LineKind::HunkHeader, None, None);
            return;
        }

        if text.starts_with("@@") {
            self.awaiting_hunk_header = false;
            self.close_hunk(line_id);
            if let Some((old_range, new_range)) = parse_unified_hunk_header(&text) {
                let mode = self.current_file_mode().unwrap_or(FileMode::Malformed);
                if mode.is_rich() {
                    self.start_unified_hunk(line_id, old_range, new_range);
                } else {
                    self.start_fallback_hunk(line_id, hunk_kind_for_mode(mode));
                }
            } else {
                self.mark_current_file_mode(FileMode::Malformed);
                self.start_fallback_hunk(line_id, HunkKind::Malformed);
            }
            self.append_current_file_line(text, terminated, LineKind::HunkHeader, None, None);
            return;
        }

        if text == "\\ No newline at end of file" {
            self.append_current_file_line(text, terminated, LineKind::NoNewlineMarker, None, None);
            return;
        }

        if is_binary_line(&text) {
            self.awaiting_hunk_header = false;
            self.mark_current_file_mode(FileMode::Binary);
            self.append_current_file_line(text, terminated, LineKind::Binary, None, None);
            return;
        }

        if self.current_hunk.is_some() {
            if self.current_file_mode() == Some(FileMode::Rich) {
                if let Some((kind, old_line, new_line)) = self.consume_rich_hunk_line(&text) {
                    self.append_current_file_line(text, terminated, kind, old_line, new_line);
                } else if is_word_diff_line(&text) {
                    self.mark_current_file_mode(FileMode::WordDiff);
                    self.append_current_file_line(text, terminated, LineKind::Fallback, None, None);
                } else if is_submodule_line(&text) {
                    self.mark_current_file_mode(FileMode::Submodule);
                    self.append_current_file_line(
                        text,
                        terminated,
                        LineKind::Submodule,
                        None,
                        None,
                    );
                } else {
                    self.mark_current_file_mode(FileMode::Malformed);
                    self.append_current_file_line(text, terminated, LineKind::Fallback, None, None);
                }
            } else {
                let kind =
                    line_kind_for_mode(self.current_file_mode().unwrap_or(FileMode::Malformed));
                self.append_current_file_line(text, terminated, kind, None, None);
            }
            return;
        }

        // inline submodule diff の `index` / `---` / `+++` は外側 file の path/status を
        // 上書きしてはならない。hunk header は上で navigation anchor として記録済みである。
        if self.current_file_mode() == Some(FileMode::Submodule) {
            self.append_current_file_line(text, terminated, LineKind::Submodule, None, None);
            return;
        }

        if is_gitlink_metadata(&text) {
            self.mark_current_file_mode(FileMode::Submodule);
            self.append_current_file_line(text, terminated, LineKind::FileMetadata, None, None);
            return;
        }

        if let Some(path) = text.strip_prefix("rename from ") {
            self.update_extended_path(path, true, ChangeStatus::Renamed);
            self.append_current_file_line(text, terminated, LineKind::FileMetadata, None, None);
            return;
        }
        if let Some(path) = text.strip_prefix("rename to ") {
            self.update_extended_path(path, false, ChangeStatus::Renamed);
            self.append_current_file_line(text, terminated, LineKind::FileMetadata, None, None);
            return;
        }
        if let Some(path) = text.strip_prefix("copy from ") {
            self.update_extended_path(path, true, ChangeStatus::Copied);
            self.append_current_file_line(text, terminated, LineKind::FileMetadata, None, None);
            return;
        }
        if let Some(path) = text.strip_prefix("copy to ") {
            self.update_extended_path(path, false, ChangeStatus::Copied);
            self.append_current_file_line(text, terminated, LineKind::FileMetadata, None, None);
            return;
        }
        if text.starts_with("new file mode ") {
            self.update_status(ChangeStatus::Added);
            self.append_current_file_line(text, terminated, LineKind::FileMetadata, None, None);
            return;
        }
        if text.starts_with("deleted file mode ") {
            self.update_status(ChangeStatus::Deleted);
            self.append_current_file_line(text, terminated, LineKind::FileMetadata, None, None);
            return;
        }
        if let Some(path) = text.strip_prefix("--- ") {
            self.awaiting_new_file_header = true;
            self.update_source_path(path, true);
            self.append_current_file_line(text, terminated, LineKind::FileMetadata, None, None);
            return;
        }
        if let Some(path) = text.strip_prefix("+++ ") {
            if !self.awaiting_new_file_header {
                self.mark_current_file_mode(FileMode::Malformed);
            }
            self.awaiting_new_file_header = false;
            self.awaiting_hunk_header = true;
            self.update_source_path(path, false);
            self.append_current_file_line(text, terminated, LineKind::FileMetadata, None, None);
            return;
        }

        let kind = line_kind_for_mode(self.current_file_mode().unwrap_or(FileMode::Malformed));
        self.append_current_file_line(text, terminated, kind, None, None);
    }

    fn consume_rich_hunk_line(
        &mut self,
        text: &str,
    ) -> Option<(LineKind, Option<u64>, Option<u64>)> {
        let marker = text.as_bytes().first().copied()?;
        let cursor = self.current_hunk?;

        let (kind, old_line, new_line) = match marker {
            b' ' if cursor.old_remaining > 0 && cursor.new_remaining > 0 => (
                LineKind::Context,
                Some(cursor.old_next),
                Some(cursor.new_next),
            ),
            b'-' if cursor.old_remaining > 0 => (LineKind::Deletion, Some(cursor.old_next), None),
            b'+' if cursor.new_remaining > 0 => (LineKind::Addition, None, Some(cursor.new_next)),
            _ => return None,
        };

        if let Some(active) = self.current_hunk.as_mut() {
            match kind {
                LineKind::Context => {
                    active.old_next = active.old_next.saturating_add(1);
                    active.new_next = active.new_next.saturating_add(1);
                    active.old_remaining = active.old_remaining.saturating_sub(1);
                    active.new_remaining = active.new_remaining.saturating_sub(1);
                }
                LineKind::Deletion => {
                    active.old_next = active.old_next.saturating_add(1);
                    active.old_remaining = active.old_remaining.saturating_sub(1);
                }
                LineKind::Addition => {
                    active.new_next = active.new_next.saturating_add(1);
                    active.new_remaining = active.new_remaining.saturating_sub(1);
                }
                _ => {}
            }
        }

        if let Some(file_id) = self.current_file
            && let Some(file) = self.document.file_mut(file_id)
        {
            match kind {
                LineKind::Deletion => {
                    file.deletions = file.deletions.map(|value| value.saturating_add(1));
                }
                LineKind::Addition => {
                    file.additions = file.additions.map(|value| value.saturating_add(1));
                }
                _ => {}
            }
        }

        Some((kind, old_line, new_line))
    }

    fn start_file(&mut self, paths: ParsedPaths, requested_mode: FileMode, line_id: LineId) {
        self.close_file(line_id);
        let commit_id = self.ensure_working_commit(line_id);
        let (old_path, new_path, mode) = match paths {
            Ok((old_path, new_path)) => (old_path, new_path, requested_mode),
            Err((old_path, new_path)) => (old_path, new_path, FileMode::Malformed),
        };
        let counts = if mode.is_rich() { Some(0) } else { None };
        let status = match mode {
            FileMode::Binary => ChangeStatus::Binary,
            FileMode::Submodule => ChangeStatus::Submodule,
            _ => ChangeStatus::Modified,
        };
        let file_id = self.document.push_file(FileEntry {
            id: FileId::new(0),
            commit_id,
            old_path,
            new_path,
            status,
            mode,
            span: LineSpan::open(line_id),
            hunk_ids: Vec::new(),
            additions: counts,
            deletions: counts,
        });
        if let Some(commit) = self.document.commit_mut(commit_id) {
            commit.file_ids.push(file_id);
        }
        self.current_file = Some(file_id);
        self.awaiting_new_file_header = false;
        self.awaiting_hunk_header = false;
    }

    fn ensure_working_commit(&mut self, line_id: LineId) -> CommitId {
        if let Some(id) = self.current_commit {
            return id;
        }

        let id = self.document.push_commit(Commit {
            id: CommitId::new(0),
            kind: CommitKind::WorkingTree,
            object_id: None,
            subject: None,
            span: LineSpan::open(line_id),
            file_ids: Vec::new(),
        });
        self.current_commit = Some(id);
        id
    }

    fn start_unified_hunk(
        &mut self,
        line_id: LineId,
        old_range: SourceRange,
        new_range: SourceRange,
    ) {
        let Some(file_id) = self.current_file else {
            return;
        };
        let hunk_id = self.document.push_hunk(Hunk {
            id: HunkId::new(0),
            file_id,
            kind: HunkKind::Unified,
            header_line: line_id,
            span: LineSpan::open(line_id),
            old_range: Some(old_range),
            new_range: Some(new_range),
        });
        if let Some(file) = self.document.file_mut(file_id) {
            file.hunk_ids.push(hunk_id);
        }
        self.current_hunk = Some(HunkCursor {
            id: hunk_id,
            old_next: old_range.start,
            new_next: new_range.start,
            old_remaining: old_range.count,
            new_remaining: new_range.count,
        });
    }

    fn start_fallback_hunk(&mut self, line_id: LineId, kind: HunkKind) {
        let Some(file_id) = self.current_file else {
            return;
        };
        let hunk_id = self.document.push_hunk(Hunk {
            id: HunkId::new(0),
            file_id,
            kind,
            header_line: line_id,
            span: LineSpan::open(line_id),
            old_range: None,
            new_range: None,
        });
        if let Some(file) = self.document.file_mut(file_id) {
            file.hunk_ids.push(hunk_id);
        }
        self.current_hunk = Some(HunkCursor {
            id: hunk_id,
            old_next: 0,
            new_next: 0,
            old_remaining: 0,
            new_remaining: 0,
        });
    }

    fn close_hunk(&mut self, end: LineId) {
        let Some(cursor) = self.current_hunk.take() else {
            return;
        };
        let incomplete = self.current_file_mode() == Some(FileMode::Rich)
            && (cursor.old_remaining != 0 || cursor.new_remaining != 0);
        if incomplete {
            self.mark_current_file_mode(FileMode::Malformed);
            if let Some(hunk) = self.document.hunk_mut(cursor.id) {
                hunk.kind = HunkKind::Malformed;
                hunk.old_range = None;
                hunk.new_range = None;
            }
        }
        if let Some(hunk) = self.document.hunk_mut(cursor.id) {
            hunk.span.end_exclusive = Some(end.get());
        }
    }

    fn close_file(&mut self, end: LineId) {
        self.close_hunk(end);
        if self.awaiting_new_file_header {
            self.mark_current_file_mode(FileMode::Malformed);
            self.awaiting_new_file_header = false;
        }
        if self.awaiting_hunk_header {
            self.mark_current_file_mode(FileMode::Malformed);
            self.awaiting_hunk_header = false;
        }
        if let Some(file_id) = self.current_file.take()
            && let Some(file) = self.document.file_mut(file_id)
        {
            file.span.end_exclusive = Some(end.get());
        }
    }

    fn close_commit(&mut self, end: LineId) {
        if let Some(commit_id) = self.current_commit.take()
            && let Some(commit) = self.document.commit_mut(commit_id)
        {
            commit.span.end_exclusive = Some(end.get());
        }
    }

    fn mark_current_file_mode(&mut self, mode: FileMode) {
        let Some(file_id) = self.current_file else {
            return;
        };
        if let Some(file) = self.document.file_mut(file_id) {
            file.mode = mode;
            file.additions = None;
            file.deletions = None;
            if mode == FileMode::Binary {
                file.status = ChangeStatus::Binary;
            } else if mode == FileMode::Submodule {
                file.status = ChangeStatus::Submodule;
            }
        }
        if let Some(cursor) = self.current_hunk
            && let Some(hunk) = self.document.hunk_mut(cursor.id)
        {
            hunk.kind = hunk_kind_for_mode(mode);
            if !mode.is_rich() {
                hunk.old_range = None;
                hunk.new_range = None;
            }
        }
    }

    fn current_file_mode(&self) -> Option<FileMode> {
        self.current_file
            .and_then(|file_id| self.document.file(file_id))
            .map(|file| file.mode)
    }

    fn paths_are_inside_current_submodule(&self, paths: &ParsedPaths) -> bool {
        let Some(file_id) = self.current_file else {
            return false;
        };
        let Some(file) = self.document.file(file_id) else {
            return false;
        };
        if file.mode != FileMode::Submodule {
            return false;
        }
        let Some(root) = file.primary_path() else {
            return false;
        };
        let (old_path, new_path) = match paths {
            Ok(paths) | Err(paths) => paths,
        };
        old_path
            .as_deref()
            .into_iter()
            .chain(new_path.as_deref())
            .any(|path| {
                path.strip_prefix(root)
                    .is_some_and(|suffix| suffix.starts_with('/'))
            })
    }

    fn update_status(&mut self, status: ChangeStatus) {
        if let Some(file_id) = self.current_file
            && let Some(file) = self.document.file_mut(file_id)
        {
            file.status = status;
        }
    }

    fn update_extended_path(&mut self, value: &str, old: bool, status: ChangeStatus) {
        let decoded = decode_git_path(value.trim());
        let malformed = decoded.is_none();
        let Some(file_id) = self.current_file else {
            return;
        };
        if let Some(file) = self.document.file_mut(file_id) {
            file.status = status;
            if let Some(decoded) = decoded {
                if old {
                    file.old_path = Some(decoded);
                } else {
                    file.new_path = Some(decoded);
                }
            }
        }
        if malformed {
            self.mark_current_file_mode(FileMode::Malformed);
        }
    }

    fn update_source_path(&mut self, value: &str, old: bool) {
        // Git の `---` / `+++` header は timestamp field を持たないため、unquoted space も path
        // の一部として扱う。quoted path では decoder が閉じ quote 外の byte を拒否する。
        let token = value.trim();
        let decoded = decode_git_path_bytes(token);
        let is_dev_null = decoded.as_deref() == Some(b"/dev/null".as_slice());
        let display = decoded.as_deref().map(|bytes| {
            let without_prefix = if old {
                bytes.strip_prefix(b"a/").unwrap_or(bytes)
            } else {
                bytes.strip_prefix(b"b/").unwrap_or(bytes)
            };
            escape_path_bytes(without_prefix)
        });

        let Some(file_id) = self.current_file else {
            return;
        };
        if let Some(file) = self.document.file_mut(file_id) {
            if old {
                if is_dev_null {
                    file.old_path = None;
                    file.status = ChangeStatus::Added;
                } else if display.is_some() {
                    file.old_path = display;
                }
            } else if is_dev_null {
                file.new_path = None;
                file.status = ChangeStatus::Deleted;
            } else if display.is_some() {
                file.new_path = display;
            }
        }
        if decoded.is_none() {
            self.mark_current_file_mode(FileMode::Malformed);
        }
    }

    fn append_current_file_line(
        &mut self,
        text: String,
        terminated: bool,
        kind: LineKind,
        old_line_number: Option<u64>,
        new_line_number: Option<u64>,
    ) {
        let file_id = self.current_file;
        let commit_id = file_id
            .and_then(|id| self.document.file(id))
            .map(|file| file.commit_id);
        let hunk_id = self.current_hunk.map(|cursor| cursor.id);
        self.append_line(
            text,
            terminated,
            kind,
            commit_id,
            file_id,
            hunk_id,
            old_line_number,
            new_line_number,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn append_line(
        &mut self,
        text: String,
        terminated: bool,
        kind: LineKind,
        commit_id: Option<CommitId>,
        file_id: Option<FileId>,
        hunk_id: Option<HunkId>,
        old_line_number: Option<u64>,
        new_line_number: Option<u64>,
    ) {
        let _ = self.document.push_line(DocumentLine {
            id: LineId::new(0),
            text,
            kind,
            commit_id,
            file_id,
            hunk_id,
            old_line_number,
            new_line_number,
            terminated,
        });
    }

    fn next_line_id(&self) -> LineId {
        LineId::new(self.document.lines().len() as u64)
    }
}

/// byte slice 全体を解析し、EOF 済み document を返す。
#[must_use]
pub fn parse_bytes(input: &[u8]) -> Document {
    let mut parser = GitParser::new();
    let _ = parser.push(input);
    parser.into_document()
}

/// Git の1つの path token を C-style quote / octal escape から表示 text へ戻す。
///
/// quoted token は閉じ quote の外側を受け付けない。unquoted path は field 全体として扱う。
/// tab、newline、invalid UTF-8 は path layout を壊さない `\\xNN` 表記になる。
#[must_use]
pub fn decode_git_path(token: &str) -> Option<String> {
    decode_git_path_bytes(token).map(|bytes| escape_path_bytes(&bytes))
}

fn parse_commit_header(line: &str) -> Option<String> {
    let rest = line.strip_prefix("commit ")?;
    let object_id = rest.split_ascii_whitespace().next()?;
    if (7..=64).contains(&object_id.len()) && object_id.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        Some(object_id.to_owned())
    } else {
        None
    }
}

fn parse_diff_git_paths(rest: &str) -> ParsedPaths {
    let Some((old_token, remaining)) = take_token(rest) else {
        return Err((None, None));
    };
    let old_path = decode_diff_path(old_token, b"a/");
    let Some((new_token, trailing)) = take_token(remaining) else {
        return Err((old_path, None));
    };
    let new_path = decode_diff_path(new_token, b"b/");
    let malformed = old_path.is_none() || new_path.is_none() || !trailing.trim().is_empty();
    if malformed {
        Err((old_path, new_path))
    } else {
        Ok((old_path, new_path))
    }
}

fn decode_diff_path(token: &str, prefix: &[u8]) -> Option<String> {
    decode_git_path_bytes(token).map(|bytes| {
        let path = bytes.strip_prefix(prefix).unwrap_or(&bytes);
        escape_path_bytes(path)
    })
}

fn take_token(input: &str) -> Option<(&str, &str)> {
    let input = input.trim_start_matches([' ', '\t']);
    if input.is_empty() {
        return None;
    }
    if input.starts_with('"') {
        let bytes = input.as_bytes();
        let mut escaped = false;
        for (index, &byte) in bytes.iter().enumerate().skip(1) {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                let end = index.saturating_add(1);
                return Some((&input[..end], &input[end..]));
            }
        }
        None
    } else {
        let end = input.find([' ', '\t']).unwrap_or(input.len());
        Some((&input[..end], &input[end..]))
    }
}

fn decode_git_path_bytes(token: &str) -> Option<Vec<u8>> {
    if !token.starts_with('"') {
        return Some(token.as_bytes().to_vec());
    }
    if !token.ends_with('"') || token.len() < 2 {
        return None;
    }

    let inner = &token.as_bytes()[1..token.len().saturating_sub(1)];
    let mut output = Vec::with_capacity(inner.len());
    let mut index = 0;
    while index < inner.len() {
        if inner[index] != b'\\' {
            if inner[index] == b'"' {
                return None;
            }
            output.push(inner[index]);
            index = index.saturating_add(1);
            continue;
        }

        index = index.saturating_add(1);
        let escaped = *inner.get(index)?;
        if (b'0'..=b'7').contains(&escaped) {
            let mut value = 0_u16;
            let mut digits = 0;
            while digits < 3 && index < inner.len() && (b'0'..=b'7').contains(&inner[index]) {
                value = value
                    .saturating_mul(8)
                    .saturating_add(u16::from(inner[index] - b'0'));
                index = index.saturating_add(1);
                digits += 1;
            }
            if value > u16::from(u8::MAX) {
                return None;
            }
            output.push(value as u8);
            continue;
        }

        let decoded = match escaped {
            b'a' => 0x07,
            b'b' => 0x08,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 0x0b,
            b'f' => 0x0c,
            b'r' => b'\r',
            b'\\' => b'\\',
            b'"' => b'"',
            _ => return None,
        };
        output.push(decoded);
        index = index.saturating_add(1);
    }
    Some(output)
}

fn parse_unified_hunk_header(line: &str) -> Option<(SourceRange, SourceRange)> {
    let body = line.strip_prefix("@@ ")?;
    let end = body.find(" @@")?;
    let coordinates = &body[..end];
    let mut fields = coordinates.split_ascii_whitespace();
    let old = parse_source_range(fields.next()?, b'-')?;
    let new = parse_source_range(fields.next()?, b'+')?;
    if fields.next().is_some() {
        return None;
    }
    Some((old, new))
}

fn parse_source_range(value: &str, marker: u8) -> Option<SourceRange> {
    let bytes = value.as_bytes();
    if bytes.first().copied() != Some(marker) {
        return None;
    }
    let value = value.get(1..)?;
    let (start, count): (u64, u64) = match value.split_once(',') {
        Some((start, count)) => (parse_decimal(start)?, parse_decimal(count)?),
        None => (parse_decimal(value)?, 1),
    };
    if start == 0 && count > 0 {
        return None;
    }
    if count > 0 {
        let _ = start.checked_add(count.saturating_sub(1))?;
    }
    Some(SourceRange { start, count })
}

fn parse_decimal(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn is_binary_line(line: &str) -> bool {
    line == "GIT binary patch"
        || line.starts_with("Binary files ")
        || line.starts_with("literal ")
        || line.starts_with("delta ")
}

fn is_submodule_line(line: &str) -> bool {
    let content = line
        .strip_prefix(['+', '-', ' '])
        .unwrap_or(line)
        .trim_start();
    content.starts_with("Subproject commit ") || content.starts_with("Submodule ")
}

fn is_gitlink_metadata(line: &str) -> bool {
    (line.starts_with("index ")
        || line.starts_with("old mode ")
        || line.starts_with("new mode ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode "))
        && line.ends_with(" 160000")
}

fn parse_submodule_path(line: &str) -> Option<String> {
    let rest = line.strip_prefix("Submodule ")?;
    if rest.starts_with('"') {
        let (token, suffix) = take_token(rest)?;
        if !is_submodule_summary_suffix(suffix) {
            return None;
        }
        return decode_git_path(token);
    }

    // path 自体に `..` や `contains` が含まれても、右側の object range / marker を優先する。
    // field 数に比例する pointer 配列は作らず、oversized line でも追加 memory を一定に保つ。
    let mut scan_from = 0;
    let mut marker_start = None;
    for field in rest.split_ascii_whitespace() {
        let relative = rest.get(scan_from..)?.find(field)?;
        let start = scan_from.saturating_add(relative);
        let is_contains_marker =
            field == "contains" && rest.get(start..).is_some_and(is_submodule_contains_suffix);
        if is_contains_marker || is_git_object_range(field) {
            marker_start = Some(start);
        }
        scan_from = start.saturating_add(field.len());
    }
    let path = rest.get(..marker_start?)?.trim_end();
    if path.is_empty() {
        return None;
    }
    Some(escape_path_bytes(path.as_bytes()))
}

fn is_submodule_summary_suffix(suffix: &str) -> bool {
    is_submodule_contains_suffix(suffix)
        || suffix
            .split_ascii_whitespace()
            .next()
            .is_some_and(is_git_object_range)
}

fn is_submodule_contains_suffix(suffix: &str) -> bool {
    matches!(
        suffix.trim(),
        "contains modified content" | "contains untracked content"
    )
}

fn is_git_object_range(field: &str) -> bool {
    let field = field.trim_end_matches(':');
    let Some((old, new)) = field.split_once("..") else {
        return false;
    };
    (7..=64).contains(&old.len())
        && (7..=64).contains(&new.len())
        && old.bytes().all(|byte| byte.is_ascii_hexdigit())
        && new.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_word_diff_line(line: &str) -> bool {
    line == "~"
        || line.contains("{+")
        || line.contains("+}")
        || line.contains("[-")
        || line.contains("-]")
}

fn hunk_kind_for_mode(mode: FileMode) -> HunkKind {
    match mode {
        FileMode::Rich => HunkKind::Unified,
        FileMode::Combined => HunkKind::Combined,
        FileMode::WordDiff => HunkKind::WordDiff,
        FileMode::Submodule => HunkKind::Submodule,
        FileMode::Binary | FileMode::Malformed => HunkKind::Malformed,
    }
}

fn line_kind_for_mode(mode: FileMode) -> LineKind {
    match mode {
        FileMode::Binary => LineKind::Binary,
        FileMode::Submodule => LineKind::Submodule,
        FileMode::Rich => LineKind::FileMetadata,
        FileMode::Combined | FileMode::WordDiff | FileMode::Malformed => LineKind::Fallback,
    }
}
