use std::ops::Deref;

use git_ultrapager::ansi::{AnsiStripper, escape_interactive_bytes, escape_path_bytes};
use git_ultrapager::document::{
    DocumentLineMetadataReader, DocumentLineReader, DocumentSnapshot, OwnedDocumentLine,
};
use git_ultrapager::input::AppendStore;
use git_ultrapager::model::{
    ChangeStatus, Commit, CommitKind, FileEntry, FileId, FileMode, Hunk, HunkId, HunkKind,
    LineKind, SourceRange,
};
use git_ultrapager::parser::{
    GitParser, ParseDelta, ParseError, decode_git_path, parse_bytes as parse_bytes_result,
};
use git_ultrapager::storage::StructuralStorage;
use git_ultrapager::structure_store::{StructureFileLineBounds, StructureReader};
use pretty_assertions::assert_eq;

struct TestDocument {
    snapshot: DocumentSnapshot,
    commits: Vec<Commit>,
    files: Vec<FileEntry>,
    hunks: Vec<Hunk>,
    file_line_bounds: Vec<StructureFileLineBounds>,
}

impl TestDocument {
    fn commits(&self) -> &[Commit] {
        &self.commits
    }

    fn files(&self) -> &[FileEntry] {
        &self.files
    }

    fn hunks(&self) -> &[Hunk] {
        &self.hunks
    }

    fn hunk(&self, id: HunkId) -> Option<&Hunk> {
        self.hunks
            .get(usize::try_from(id.get()).ok()?)
            .filter(|hunk| hunk.id == id)
    }

    fn file_line_bounds(&self, id: FileId) -> Option<StructureFileLineBounds> {
        self.file_line_bounds
            .get(usize::try_from(id.get()).ok()?)
            .copied()
    }
}

impl Deref for TestDocument {
    type Target = DocumentSnapshot;

    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

fn materialize(snapshot: DocumentSnapshot) -> TestDocument {
    let structure = snapshot.structure();
    let counts = structure.visible_counts().unwrap();
    let mut reader = StructureReader::new();
    let commits = (0..counts.commits)
        .map(|ordinal| {
            let id = structure.commit_id_at(ordinal).unwrap();
            let commit = reader.read_commit(structure, id).unwrap().unwrap();
            Commit {
                id: commit.id,
                kind: commit.kind,
                object_id: commit.object_id.map(str::to_owned),
                subject: commit.subject.map(str::to_owned),
                span: commit.span,
                file_ids: commit.files.iter().collect(),
            }
        })
        .collect();
    let mut file_line_bounds = Vec::new();
    let files = (0..counts.files)
        .map(|ordinal| {
            let id = structure.file_id_at(ordinal).unwrap();
            let file = reader.read_file(structure, id).unwrap().unwrap();
            file_line_bounds.push(if file.mode.is_rich() {
                file.line_bounds
            } else {
                StructureFileLineBounds::default()
            });
            FileEntry {
                id: file.id,
                commit_id: file.commit_id,
                old_path: file.old_path.map(str::to_owned),
                new_path: file.new_path.map(str::to_owned),
                status: file.status,
                mode: file.mode,
                span: file.span,
                hunk_ids: file.hunks.iter().collect(),
                additions: file.additions,
                deletions: file.deletions,
            }
        })
        .collect();
    let hunks = (0..counts.hunks)
        .map(|ordinal| {
            let id = structure.hunk_id_at(ordinal).unwrap();
            let hunk = reader.read_hunk(structure, id).unwrap().unwrap();
            Hunk {
                id: hunk.id,
                file_id: hunk.file_id,
                kind: hunk.kind,
                header_line: hunk.header_line,
                span: hunk.span,
                old_range: hunk.old_range,
                new_range: hunk.new_range,
            }
        })
        .collect();
    TestDocument {
        snapshot,
        commits,
        files,
        hunks,
        file_line_bounds,
    }
}

fn parse_bytes(input: &[u8]) -> TestDocument {
    materialize(parse_bytes_result(input).unwrap())
}

fn decoded_lines(document: &DocumentSnapshot) -> Vec<OwnedDocumentLine> {
    let mut reader = DocumentLineReader::new();
    (0..document.line_count())
        .map(|index| {
            let id = document.line_id_at(index).unwrap();
            reader.read_owned(document, id).unwrap().unwrap()
        })
        .collect()
}

const NEW_MARKDOWN_FILE_DIFF: &str = r#"commit 38c61f63fe28e507afecf620360034d10de013fe
Author: Naoto KAWAMURA <kawamura@xenodrive.co.jp>
Date:   Fri Jul 10 14:58:15 2026 +0900

    issues: add DEC-0089 position rate gate hot-item contention

    position rate gate が write-pipeline と device_state のホット行を共有する
    構造を、リトライで覆い続けるか分離するかの判断待ちとして起票する。

diff --git a/issues/DEC-0089-position-rate-gate-hot-item.md b/issues/DEC-0089-position-rate-gate-hot-item.md
new file mode 100644
index 00000000..4cb0e9b7
--- /dev/null
+++ b/issues/DEC-0089-position-rate-gate-hot-item.md
@@ -0,0 +1,49 @@
+---
+id: DEC-0089
+title: position rate gate を device_state のホット行から分離するか
+status: open
+category: tech
+severity: med
+phase: alpha
+---
+
+## 背景
+
+device_state は 1 device 1 行のホット行で、毎秒のテレメトリ受信のたびに
+write-pipeline の TransactWriteItems (位置・イベントの採用更新、last_data_at
+更新) が書き込む。positions 系ハンドラの position rate gate (短期 lease 属性)
+も同じ行に同居しているため、トランザクション実行中の数十 ms に gate の
+UpdateItem が重なると DynamoDB が TransactionConflictException を返す。
+
+PoC 検証 (2 端末 × 1 秒間隔送信) では、この衝突が 5xx の約 1% から始まり、
+release 失敗による lease 残留 (30 秒) とクライアントの重ね撃ちが連鎖して
+約 6% まで増幅した。対症療法として gate の acquire / release の衝突リトライ
+(00d1c51) とシミュレーターの送信直列化 (b00e179) は導入済みで、観測上の
+エラーは止まる見込み。
+
+ただし「毎秒読み書きされる行に排他制御を相乗りさせる」構造自体は残っており、
+送信頻度・device 数・トランザクション要素が増えるほど衝突確率とリトライ
+コストが上がる。gate の置き場所はまだ意図的に判断しておらず、リトライで
+覆い続けるかを alpha のうちに決めたい。
+
+## 選択肢
+
+- A: gate を device_state と別 item (同テーブル別 SK または専用テーブル) に
+  分離する。write-pipeline のトランザクションと物理的に衝突しなくなる。
+  lease による直列化の意味論は変えず、変更は device-state ストアと
+  position-rate 境界に閉じる。
+- B: gate を廃止し、レート判定を position_history の条件付き書き込みや
+  materializer 側の判定に寄せて、直列化そのものを不要にする。
+- C: 現状構造のままリトライと監視で運用する。実 SDK の送信間隔では
+  シミュレーターほど衝突しない可能性に賭ける。
+
+## 推奨
+
+A。衝突の物理的な原因 (同一 item の共有) を消しつつ、10点移動窓の検証順序が
+前提にしている直列化はそのまま残るため、影響範囲が読みやすい。B は
+レート検証の設計ごと作り直しになり alpha の期間に収まらないリスクがある。
+C は device 数と送信頻度に比例して悪化する構造を放置することになる。
+
+## 決定
+
+## 反映先
"#;

fn assert_new_markdown_file_is_rich(document: &TestDocument) {
    let file = &document.files()[0];
    assert_eq!(file.mode, FileMode::Rich);
    assert_eq!(file.status, ChangeStatus::Added);
    assert_eq!(file.additions, Some(49));
    assert_eq!(file.deletions, Some(0));

    let hunk = &document.hunks()[0];
    assert_eq!(hunk.kind, HunkKind::Unified);
    assert_eq!(hunk.old_range, Some(SourceRange { start: 0, count: 0 }));
    assert_eq!(
        hunk.new_range,
        Some(SourceRange {
            start: 1,
            count: 49,
        })
    );

    let additions = decoded_lines(document)
        .into_iter()
        .filter(|line| line.kind == LineKind::Addition)
        .collect::<Vec<_>>();
    assert_eq!(additions.len(), 49);
    for (index, line) in additions.iter().enumerate() {
        assert_eq!(line.old_line_number, None);
        assert_eq!(line.new_line_number, Some(index as u64 + 1));
    }
}

fn streaming_parser() -> (AppendStore, GitParser) {
    let raw = AppendStore::new();
    let storage = StructuralStorage::new().unwrap();
    let parser = GitParser::new(raw.clone(), &storage).unwrap();
    (raw, parser)
}

fn push_stored(
    raw: &AppendStore,
    parser: &mut GitParser,
    bytes: &[u8],
) -> Result<ParseDelta, ParseError> {
    let start = raw.append(bytes).unwrap();
    parser.push(start, bytes)
}

#[test]
fn parser_publishes_store_backed_snapshot_from_absolute_offsets() {
    let raw = AppendStore::new();
    let storage = StructuralStorage::new().unwrap();
    let mut parser = GitParser::new(raw.clone(), &storage).unwrap();
    let start = raw.append(b"plain\n").unwrap();

    let delta = parser.push(start, b"plain\n").unwrap();
    assert_eq!(delta.lines, 0..1);
    let snapshot = parser.snapshot().unwrap();
    assert_eq!(snapshot.line_count(), 1);
}

#[test]
fn parser_rejects_chunk_bytes_that_differ_from_the_raw_store() {
    let raw = AppendStore::new();
    let storage = StructuralStorage::new().unwrap();
    let mut parser = GitParser::new(raw.clone(), &storage).unwrap();
    let start = raw.append(b"plain plain plain!\n").unwrap();
    let before = parser.snapshot().unwrap();

    assert!(matches!(
        parser.push(start, b"diff --git a/x b/x\n"),
        Err(ParseError::RawChunkMismatch { start: 0, end: 19 })
    ));
    assert!(matches!(parser.snapshot(), Err(ParseError::Poisoned)));
    assert!(before.is_empty());
}

#[test]
fn ansi_sequence_is_removed_across_chunk_boundaries() {
    let chunks: [&[u8]; 8] = [
        b"plain\x1b[3",
        b"1mred\x1b[0m ",
        b"\x1b]52;c;secret",
        b"\x1b\\safe ",
        b"\x1bPpayload",
        b"\x1b\\end",
        &[0x9b, b'3'],
        b"2m!",
    ];
    let mut stripper = AnsiStripper::default();
    let mut output = Vec::new();
    for chunk in chunks {
        stripper.push(chunk, &mut output);
    }
    stripper.finish_into(&mut output);

    assert_eq!(escape_interactive_bytes(&output), "plainred safe end!");
}

#[test]
fn c1_control_bytes_do_not_consume_valid_utf8_continuations() {
    let mut stripper = AnsiStripper::default();
    let mut output = Vec::new();
    for byte in "ś ŝ remain".as_bytes() {
        stripper.push(std::slice::from_ref(byte), &mut output);
    }
    stripper.push(&[0x9b, b'3', b'1', b'm'], &mut output);
    stripper.push(b" visible", &mut output);

    // OSC payload 内の U+015C は末尾 byte が C1 ST と同じ0x9Cだが、payload を途中終了しない。
    stripper.push(b"\x1b]title;hidden ", &mut output);
    stripper.push("Ŝ 本 still hidden".as_bytes(), &mut output);
    stripper.push(b"\x07 after osc", &mut output);

    assert_eq!(
        escape_interactive_bytes(&output),
        "ś ŝ remain visible after osc"
    );
}

#[test]
fn invalid_utf8_and_controls_are_visible_without_changing_path_layout() {
    assert_eq!(
        escape_interactive_bytes(b"ok\t\x00\xff\xc2\x85"),
        "ok\t\\x00\\xFF\\xC2\\x85"
    );
    assert_eq!(escape_path_bytes(b"a\tb\nc\x7f"), "a\\x09b\\x0Ac\\x7F");
    assert_eq!(
        decode_git_path(r#""dir/line\t\377.rs""#).as_deref(),
        Some("dir/line\\x09\\xFF.rs")
    );
    assert_eq!(decode_git_path(r#""closed" trailing""#), None);
}

#[test]
fn standard_diff_builds_commit_file_hunk_and_line_numbers() {
    let input = br#"commit 0123456789abcdef0123456789abcdef01234567
Author: Example <example@example.com>
Date:   Thu Jul 9 12:00:00 2026 +0900

    Parse quoted paths

diff --git "a/src/foo\040bar.rs" "b/src/foo\040bar.rs"
index 1111111..2222222 100644
--- "a/src/foo\040bar.rs"
+++ "b/src/foo\040bar.rs"
@@ -10,2 +10,3 @@ fn example() {
 unchanged
-old_value
+new_value
+extra_value
"#;
    let document = parse_bytes(input);

    assert!(document.is_complete());
    assert!(document.has_trailing_newline());
    assert_eq!(document.commits().len(), 1);
    assert_eq!(document.files().len(), 1);
    assert_eq!(document.hunks().len(), 1);

    let commit = &document.commits()[0];
    assert_eq!(commit.kind, CommitKind::Commit);
    assert_eq!(commit.subject.as_deref(), Some("Parse quoted paths"));
    assert_eq!(commit.file_ids.len(), 1);

    let file = &document.files()[0];
    assert_eq!(file.old_path.as_deref(), Some("src/foo bar.rs"));
    assert_eq!(file.new_path.as_deref(), Some("src/foo bar.rs"));
    assert_eq!(file.status, ChangeStatus::Modified);
    assert_eq!(file.mode, FileMode::Rich);
    assert_eq!(file.additions, Some(2));
    assert_eq!(file.deletions, Some(1));
    assert!(file.span.is_closed());
    let bounds = document.file_line_bounds(file.id).unwrap();
    assert_eq!(bounds.max_old_line_number, Some(11));
    assert_eq!(bounds.max_new_line_number, Some(12));

    let hunk = &document.hunks()[0];
    assert_eq!(hunk.kind, HunkKind::Unified);
    assert_eq!(
        hunk.old_range.map(|range| (range.start, range.count)),
        Some((10, 2))
    );
    assert_eq!(
        hunk.new_range.map(|range| (range.start, range.count)),
        Some((10, 3))
    );

    let lines = decoded_lines(&document);
    let source_lines: Vec<_> = lines
        .iter()
        .filter(|line| {
            matches!(
                line.kind,
                LineKind::Context | LineKind::Deletion | LineKind::Addition
            )
        })
        .map(|line| {
            (
                line.kind,
                line.old_line_number,
                line.new_line_number,
                line.content(),
            )
        })
        .collect();
    assert_eq!(
        source_lines,
        vec![
            (LineKind::Context, Some(10), Some(10), "unchanged"),
            (LineKind::Deletion, Some(11), None, "old_value"),
            (LineKind::Addition, None, Some(11), "new_value"),
            (LineKind::Addition, None, Some(12), "extra_value"),
        ]
    );
}

#[test]
fn rename_and_copy_paths_are_decoded_and_occurrences_stay_distinct() {
    let input = br#"commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa

    Rename and copy

diff --git "a/old\040name.rs" "b/new\040name.rs"
similarity index 100%
rename from "old\040name.rs"
rename to "new\040name.rs"
diff --git a/source.rs b/copy.rs
similarity index 100%
copy from source.rs
copy to copy.rs
commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb

    Touch the renamed file again

diff --git a/new_name.rs b/new_name.rs
--- a/new_name.rs
+++ b/new_name.rs
@@ -1 +1 @@
-old
+new
"#;
    let document = parse_bytes(input);

    assert_eq!(document.commits().len(), 2);
    assert_eq!(document.files().len(), 3);
    assert_eq!(document.files()[0].status, ChangeStatus::Renamed);
    assert_eq!(
        document.files()[0].display_path(),
        "old name.rs → new name.rs"
    );
    assert_eq!(document.files()[1].status, ChangeStatus::Copied);
    assert_eq!(document.commits()[0].file_ids.len(), 2);
    assert_eq!(document.commits()[1].file_ids.len(), 1);
    assert_ne!(document.files()[0].id, document.files()[2].id);
}

#[test]
fn parser_keeps_partial_lines_and_escape_sequences_between_pushes() {
    let input =
        b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-\x1b[31mold\x1b[0m\n+new";
    let (raw, mut parser) = streaming_parser();
    for chunk in input.chunks(3) {
        let _ = push_stored(&raw, &mut parser, chunk).unwrap();
    }
    let _ = raw.seal().unwrap();
    let _ = parser.finish().unwrap();
    let document = materialize(parser.snapshot().unwrap());
    let lines = decoded_lines(&document);

    assert_eq!(document.files()[0].deletions, Some(1));
    assert_eq!(document.files()[0].additions, Some(1));
    let deletion = lines.iter().find(|line| line.kind == LineKind::Deletion);
    assert_eq!(deletion.map(|line| line.text.as_str()), Some("-old"));
    assert_eq!(lines.last().map(|line| line.terminated), Some(false));
    assert!(!document.has_trailing_newline());
}

#[test]
fn unsupported_file_formats_fall_back_without_losing_navigation_records() {
    let input = br#"diff --cc merge.rs
index aaa,bbb..ccc
@@@ -1,1 -1,1 +1,1 @@@
--parent one
 -parent two
++result
diff --git a/word.txt b/word.txt
@@ -1 +1 @@
the [-old-]{+new+} value
diff --git a/image.bin b/image.bin
GIT binary patch
literal 3
abc
diff --git a/vendor b/vendor
index aaaaaaa..bbbbbbb 160000
@@ -1 +1 @@
-Subproject commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
+Subproject commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
diff --git a/broken.rs b/broken.rs
--- a/broken.rs
+++ b/broken.rs
@@ this is not a unified header @@
raw
"#;
    let document = parse_bytes(input);
    let modes: Vec<_> = document.files().iter().map(|file| file.mode).collect();

    assert_eq!(
        modes,
        vec![
            FileMode::Combined,
            FileMode::WordDiff,
            FileMode::Binary,
            FileMode::Submodule,
            FileMode::Malformed,
        ]
    );
    assert!(
        document
            .files()
            .iter()
            .all(|file| file.additions.is_none() && file.deletions.is_none())
    );
    assert_eq!(document.hunks().len(), 4);
    assert!(document.hunks().iter().all(|hunk| hunk.span.is_closed()));
}

#[test]
fn malformed_header_downgrades_only_its_file() {
    let input = b"diff --git \"a/unterminated b/file\nraw\ndiff --git a/good.rs b/good.rs\n--- a/good.rs\n+++ b/good.rs\n@@ -1 +1 @@\n-old\n+new\n";
    let document = parse_bytes(input);

    assert_eq!(document.files().len(), 2);
    assert_eq!(document.files()[0].mode, FileMode::Malformed);
    assert_eq!(document.files()[1].mode, FileMode::Rich);
    assert_eq!(document.files()[1].additions, Some(1));
}

#[test]
fn malformed_paths_preserve_every_path_that_was_already_decoded() {
    let document = parse_bytes(
        b"diff --git a/known.rs \"b/unterminated\nrename from \"also unterminated\nraw\n",
    );

    assert_eq!(document.files()[0].mode, FileMode::Malformed);
    assert_eq!(document.files()[0].old_path.as_deref(), Some("known.rs"));
    assert_eq!(document.files()[0].new_path, None);

    let source_header = parse_bytes(b"diff --git a/old.rs b/new.rs\n--- \"unterminated\n+++ b/new.rs\n@@ -1 +1 @@\n-old\n+new\n");
    assert_eq!(source_header.files()[0].mode, FileMode::Malformed);
    assert_eq!(source_header.files()[0].old_path.as_deref(), Some("old.rs"));
    assert_eq!(source_header.files()[0].new_path.as_deref(), Some("new.rs"));
}

#[test]
fn overflowing_hunk_range_downgrades_instead_of_repeating_line_numbers() {
    let document = parse_bytes(b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -18446744073709551615,2 +1,2 @@\n-old\n-old2\n+new\n+new2\n");

    assert_eq!(document.files()[0].mode, FileMode::Malformed);
    assert_eq!(document.hunks()[0].kind, HunkKind::Malformed);
    assert!(
        decoded_lines(&document)
            .iter()
            .all(|line| line.old_line_number.is_none())
    );

    for header in [
        "@@ -18446744073709551615,1 +1,1 @@",
        "@@ -18446744073709551614,2 +1,1 @@",
    ] {
        let input = format!("diff --git a/x b/x\n--- a/x\n+++ b/x\n{header}\n-old\n+new\n");
        let document = parse_bytes(input.as_bytes());
        assert_eq!(document.files()[0].mode, FileMode::Malformed);
        assert_eq!(document.hunks()[0].kind, HunkKind::Malformed);
        assert!(
            decoded_lines(&document)
                .iter()
                .all(|line| line.old_line_number.is_none())
        );
    }
}

#[test]
fn incomplete_hunk_body_loses_source_semantics_after_malformed_downgrade() {
    let (raw, mut parser) = streaming_parser();
    let _ = push_stored(
        &raw,
        &mut parser,
        b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-old\n+new\n",
    )
    .unwrap();
    let active = materialize(parser.snapshot().unwrap());
    assert_eq!(active.files()[0].mode, FileMode::Rich);
    assert!(decoded_lines(&active).iter().any(|line| {
        line.text == "-old" && line.kind == LineKind::Deletion && line.old_line_number == Some(1)
    }));

    let _ = raw.seal().unwrap();
    let document = materialize(parser.into_document().unwrap());
    let file = &document.files()[0];
    let body = decoded_lines(&document)
        .into_iter()
        .filter(|line| matches!(line.text.as_str(), "-old" | "+new"))
        .collect::<Vec<_>>();

    assert_eq!(file.mode, FileMode::Malformed);
    assert_eq!(document.hunks()[0].kind, HunkKind::Malformed);
    assert!(body.iter().all(|line| line.kind == LineKind::Fallback));
    assert!(
        body.iter()
            .all(|line| line.old_line_number.is_none() && line.new_line_number.is_none())
    );
    let bounds = document.file_line_bounds(file.id).unwrap();
    assert_eq!(bounds.max_old_line_number, None);
    assert_eq!(bounds.max_new_line_number, None);

    let mut metadata_reader = DocumentLineMetadataReader::new();
    for line in &body {
        let metadata = metadata_reader.read(&document, line.id).unwrap().unwrap();
        assert_eq!(metadata.kind, LineKind::Fallback);
        assert_eq!(metadata.old_line_number, None);
        assert_eq!(metadata.new_line_number, None);
        assert_eq!(
            metadata.file_line_bounds,
            Some(StructureFileLineBounds::default())
        );
    }

    // Final snapshotのCOW downgradeは、workerが既に保持するactive snapshotを書き換えない。
    assert_eq!(active.files()[0].mode, FileMode::Rich);
    assert!(
        decoded_lines(&active)
            .iter()
            .any(|line| line.kind == LineKind::Addition)
    );
}

#[test]
fn non_empty_hunk_ranges_reject_zero_source_line_numbers() {
    let malformed =
        parse_bytes(b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -0,1 +0,1 @@\n-old\n+new\n");
    assert_eq!(malformed.files()[0].mode, FileMode::Malformed);

    let empty_side = parse_bytes(b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -0,0 +1,1 @@\n+new\n");
    assert_eq!(empty_side.files()[0].mode, FileMode::Rich);
    assert_eq!(empty_side.files()[0].additions, Some(1));
}

#[test]
fn new_markdown_file_frontmatter_and_lists_keep_source_line_numbers() {
    assert_new_markdown_file_is_rich(&parse_bytes(NEW_MARKDOWN_FILE_DIFF.as_bytes()));

    let (raw, mut parser) = streaming_parser();
    for chunk in NEW_MARKDOWN_FILE_DIFF.as_bytes().chunks(31) {
        push_stored(&raw, &mut parser, chunk).unwrap();
    }
    raw.seal().unwrap();
    assert_new_markdown_file_is_rich(&materialize(parser.into_document().unwrap()));
}

#[test]
fn source_header_without_hunk_downgrades_only_that_file() {
    let document = parse_bytes(b"diff --git a/broken b/broken\n--- a/broken\n+++ b/broken\ngarbage\ndiff --git a/good b/good\n--- a/good\n+++ b/good\n@@ -1 +1 @@\n-old\n+new\n");

    assert_eq!(document.files()[0].mode, FileMode::Malformed);
    assert_eq!(document.files()[0].additions, None);
    assert_eq!(document.files()[1].mode, FileMode::Rich);
}

#[test]
fn parse_delta_identifies_existing_tail_records_that_need_invalidation() {
    let (raw, mut parser) = streaming_parser();
    let first = push_stored(&raw, &mut parser, b"diff --git a/x b/x\n@@ -1 +1 @@\n").unwrap();
    assert_eq!(first.commits, 0_u64..1);
    assert_eq!(first.files, 0_u64..1);
    assert_eq!(first.hunks, 0_u64..1);
    assert_eq!(first.updated_commit, None);
    assert_eq!(first.updated_file, None);
    assert_eq!(first.updated_hunk, None);

    let second = push_stored(&raw, &mut parser, b"-old\n+new\n").unwrap();
    assert_eq!(second.commits, 1_u64..1);
    assert_eq!(second.files, 1_u64..1);
    assert_eq!(second.hunks, 1_u64..1);
    let document = materialize(parser.snapshot().unwrap());
    assert_eq!(second.updated_commit, Some(document.commits()[0].id));
    assert_eq!(second.updated_file, Some(document.files()[0].id));
    assert_eq!(second.updated_hunk, Some(document.hunks()[0].id));
}

#[test]
fn word_diff_markers_inside_regular_source_do_not_disable_rich_parsing() {
    let document = parse_bytes(
        b"diff --git a/markers.txt b/markers.txt\n--- a/markers.txt\n+++ b/markers.txt\n@@ -1 +1 @@\n-old [-text-]\n+new {+text+}\n",
    );

    assert_eq!(document.files()[0].mode, FileMode::Rich);
    assert_eq!(document.files()[0].deletions, Some(1));
    assert_eq!(document.files()[0].additions, Some(1));
}

#[test]
fn inline_submodule_diff_stays_in_outer_entry_and_next_outer_file_is_detected() {
    let document = parse_bytes(
        br#"diff --git a/outer.txt b/outer.txt
--- a/outer.txt
+++ b/outer.txt
@@ -1 +1 @@
-before
+after
Submodule vendor/sub contains modified content
diff --git a/vendor/sub/nested.txt b/vendor/sub/nested.txt
index 1111111..2222222 100644
--- a/vendor/sub/nested.txt
+++ b/vendor/sub/nested.txt
@@ -1 +1 @@
-nested before
+nested after
diff --git a/vendor/sub/second.txt b/vendor/sub/second.txt
index 3333333..4444444 100644
--- a/vendor/sub/second.txt
+++ b/vendor/sub/second.txt
@@ -1 +1 @@
-second before
+second after
diff --git a/zzz.txt b/zzz.txt
--- a/zzz.txt
+++ b/zzz.txt
@@ -1 +1 @@
-zzz before
+zzz after
"#,
    );

    assert_eq!(document.files().len(), 3);
    assert_eq!(document.files()[0].primary_path(), Some("outer.txt"));
    assert_eq!(document.files()[1].primary_path(), Some("vendor/sub"));
    assert_eq!(document.files()[1].mode, FileMode::Submodule);
    assert_eq!(document.files()[2].primary_path(), Some("zzz.txt"));
    assert_eq!(document.files()[2].mode, FileMode::Rich);
    assert_eq!(document.files()[1].hunk_ids.len(), 2);
    assert!(
        document.files()[1]
            .hunk_ids
            .iter()
            .all(|id| document.hunk(*id).is_some_and(|hunk| hunk.span.is_closed()))
    );
}

#[test]
fn submodule_summary_without_diff_header_still_creates_an_outer_file() {
    let document = parse_bytes(b"Submodule libs/foo..bar 1111111..2222222:\n  > Update dependency\nSubmodule vendor/other 3333333..4444444:\n  > Update other dependency\n");

    assert_eq!(document.files().len(), 2);
    assert_eq!(document.files()[0].mode, FileMode::Submodule);
    assert_eq!(document.files()[0].status, ChangeStatus::Submodule);
    assert_eq!(document.files()[0].primary_path(), Some("libs/foo..bar"));
    assert_eq!(document.files()[1].primary_path(), Some("vendor/other"));
    assert_eq!(document.commits()[0].kind, CommitKind::WorkingTree);
}

#[test]
fn plain_submodule_like_sentence_does_not_create_a_file_entry() {
    let document = parse_bytes(b"Submodule \"foo\" is optional\nSubmodule parser a..b comparison\nSubmodule parser contains examples\n");

    assert!(document.is_plain());
    assert!(document.commits().is_empty());
}

#[test]
fn every_byte_value_and_chunk_boundary_is_safe_to_parse() {
    let mut input = Vec::new();
    for byte in 0_u8..=u8::MAX {
        input.extend_from_slice(b"diff --git a/x b/x\n@@ -1 +1 @@\n+");
        input.push(byte);
        input.push(b'\n');
    }

    for chunk_size in [1, 2, 3, 7, 31, 256] {
        let (raw, mut parser) = streaming_parser();
        for chunk in input.chunks(chunk_size) {
            let _ = push_stored(&raw, &mut parser, chunk).unwrap();
        }
        let _ = raw.seal().unwrap();
        let _ = parser.finish().unwrap();
        let document = parser.snapshot().unwrap();
        assert!(document.is_complete());
        assert!(
            decoded_lines(&document)
                .iter()
                .all(|line| !line.text.contains('\u{1b}'))
        );
    }
}

#[test]
fn parser_poison_hides_partial_state_after_raw_contract_failures() {
    let (raw, mut parser) = streaming_parser();
    let _ = raw.append(b"x").unwrap();

    let error = parser.push(1, b"x").unwrap_err();
    assert!(matches!(error, ParseError::NonContiguousRawChunk { .. }));
    assert!(matches!(parser.snapshot(), Err(ParseError::Poisoned)));
    assert!(matches!(parser.push(0, b"x"), Err(ParseError::Poisoned)));
    assert!(matches!(parser.finish(), Err(ParseError::Poisoned)));

    let (raw, mut parser) = streaming_parser();
    let unstored = parser.push(0, b"x").unwrap_err();
    assert!(matches!(unstored, ParseError::RawChunkNotStored { .. }));
    assert!(matches!(parser.snapshot(), Err(ParseError::Poisoned)));
    drop(raw);
}

#[test]
fn finish_requires_the_exact_sealed_raw_high_water_mark() {
    let (_raw, mut unsealed) = streaming_parser();
    assert!(matches!(
        unsealed.finish(),
        Err(ParseError::RawInputNotSealed)
    ));
    assert!(matches!(unsealed.snapshot(), Err(ParseError::Poisoned)));

    let (raw, mut ahead) = streaming_parser();
    let _ = raw.append(b"unreported").unwrap();
    let _ = raw.seal().unwrap();
    assert!(matches!(
        ahead.finish(),
        Err(ParseError::RawLengthMismatch { .. })
    ));
    assert!(matches!(ahead.snapshot(), Err(ParseError::Poisoned)));
}

#[test]
fn successful_finish_is_idempotent_but_later_push_is_rejected() {
    let (raw, mut parser) = streaming_parser();
    let first = push_stored(&raw, &mut parser, b"plain\n").unwrap();
    assert_eq!(first.lines, 0..1);
    let _ = raw.seal().unwrap();

    let completed = parser.finish().unwrap();
    assert!(completed.completed);
    let repeated = parser.finish().unwrap();
    assert_eq!(repeated.lines, 1..1);
    assert!(repeated.completed);
    assert!(matches!(parser.push(6, b""), Err(ParseError::Finished)));
    assert!(parser.snapshot().unwrap().is_complete());
}

#[test]
fn displayed_newlines_crlf_and_lone_cr_follow_sanitized_boundaries() {
    let document = parse_bytes(b"a\x1b]hidden\ninside\x07b\r\nc\r");
    let lines = decoded_lines(&document);

    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].text, "ab");
    assert!(lines[0].terminated);
    assert_eq!(lines[1].text, "c\\x0D");
    assert!(!lines[1].terminated);
    assert!(!document.has_trailing_newline());
}
