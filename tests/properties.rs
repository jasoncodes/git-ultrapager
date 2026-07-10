use git_ultrapager::ansi::{AnsiStripper, escape_interactive_bytes};
use git_ultrapager::document::{DocumentLineReader, DocumentSnapshot, OwnedDocumentLine};
use git_ultrapager::input::AppendStore;
use git_ultrapager::model::{Commit, FileEntry, Hunk};
use git_ultrapager::parser::{GitParser, parse_bytes};
use git_ultrapager::storage::StructuralStorage;
use git_ultrapager::structure_store::StructureReader;
use proptest::prelude::*;

#[derive(Debug, PartialEq, Eq)]
struct SnapshotValue {
    lines: Vec<OwnedDocumentLine>,
    commits: Vec<Commit>,
    files: Vec<FileEntry>,
    hunks: Vec<Hunk>,
    complete: bool,
    trailing_newline: bool,
}

fn snapshot_value(document: &DocumentSnapshot) -> SnapshotValue {
    let mut reader = DocumentLineReader::new();
    let lines = (0..document.line_count())
        .map(|index| {
            let id = document.line_id_at(index).unwrap();
            reader.read_owned(document, id).unwrap().unwrap()
        })
        .collect();
    let structure = document.structure();
    let counts = structure.visible_counts().unwrap();
    let mut structure_reader = StructureReader::new();
    let commits = (0..counts.commits)
        .map(|ordinal| {
            let id = structure.commit_id_at(ordinal).unwrap();
            let commit = structure_reader
                .read_commit(structure, id)
                .unwrap()
                .unwrap();
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
    let files = (0..counts.files)
        .map(|ordinal| {
            let id = structure.file_id_at(ordinal).unwrap();
            let file = structure_reader.read_file(structure, id).unwrap().unwrap();
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
            let hunk = structure_reader.read_hunk(structure, id).unwrap().unwrap();
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
    SnapshotValue {
        lines,
        commits,
        files,
        hunks,
        complete: document.is_complete(),
        trailing_newline: document.has_trailing_newline(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn parser_is_chunk_boundary_independent(
        input in prop::collection::vec(any::<u8>(), 0..4096),
        chunk_size in 1_usize..64,
    ) {
        let expected = parse_bytes(&input).unwrap();
        let raw = AppendStore::new();
        let storage = StructuralStorage::new().unwrap();
        let mut parser = GitParser::new(raw.clone(), &storage).unwrap();
        for chunk in input.chunks(chunk_size) {
            let start = raw.append(chunk).unwrap();
            let _ = parser.push(start, chunk).unwrap();
        }
        let _ = raw.seal().unwrap();
        let actual = parser.into_document().unwrap();
        prop_assert_eq!(snapshot_value(&actual), snapshot_value(&expected));
    }

    #[test]
    fn sanitizer_accepts_arbitrary_bytes_and_chunking(
        input in prop::collection::vec(any::<u8>(), 0..4096),
        chunk_size in 1_usize..64,
    ) {
        let mut one_shot = AnsiStripper::default();
        let mut expected = Vec::new();
        one_shot.push(&input, &mut expected);
        one_shot.finish_into(&mut expected);

        let mut chunked = AnsiStripper::default();
        let mut actual = Vec::new();
        for chunk in input.chunks(chunk_size) {
            chunked.push(chunk, &mut actual);
        }
        chunked.finish_into(&mut actual);

        prop_assert_eq!(&actual, &expected);
        let _ = escape_interactive_bytes(&actual);
    }
}
