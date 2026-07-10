//! Production parserとlazy viewportを、小さいRAM/spill境界で通すlarge-input acceptance test。
//!
//! Fixture本体だけを全長に比例して保持し、line/path/anchorの補助配列は作らない。
//! 25,000 source lineを1つのunified hunkへ入れ、raw inputは1.5 MiBでspill、structural storageは2 MiB ledgerのうち1.5 MiBをrecord/blob pageへ割り当てる。
//! Logical-line tableも同じstructural ledgerを使うため、productionと同じfixed-record spill経路をreduced workloadで通せる。

use std::fmt::Write as _;

use anyhow::{Context, Result};
use git_ultrapager::document::{DocumentLineReader, DocumentSnapshot};
use git_ultrapager::input::AppendStore;
use git_ultrapager::model::LineKind;
use git_ultrapager::parser::GitParser;
use git_ultrapager::storage::{StorageConfig, StructuralStorage};
use git_ultrapager::structure_store::StructureMetadataReader;
use git_ultrapager::viewport::{ViewportLayout, ViewportLayoutConfig, ViewportLayoutStats};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const CHANGED_LINES: usize = 25_000;
const LINES_PER_SIDE: usize = CHANGED_LINES / 2;
const SOURCE_LINE_WIDTH: usize = 96;
const SOURCE_PREFIX_WIDTH: usize = 1 + 5 + 5 + 1;
const SOURCE_PAYLOAD_WIDTH: usize = SOURCE_LINE_WIDTH - SOURCE_PREFIX_WIDTH;
const RAW_MEMORY_BYTES: usize = 1_536 * KIB;
const STRUCTURAL_TOTAL_BYTES: usize = 2 * MIB;
const STRUCTURAL_PAYLOAD_BYTES: usize = 1_536 * KIB;
const STRUCTURAL_DIRECTORY_BYTES: usize = STRUCTURAL_TOTAL_BYTES - STRUCTURAL_PAYLOAD_BYTES;
const STRUCTURAL_PAGE_BYTES: usize = 256 * KIB;
const STRUCTURAL_EXTENT_BYTES: u64 = 64 * KIB as u64;
const PARSER_CHUNK_BYTES: usize = 32 * KIB;
const BODY_HEIGHT: usize = 32;
const FRAME_MISS_LIMIT: u64 = (BODY_HEIGHT * 2) as u64;
const FRAME_METADATA_READ_LIMIT: u64 = (BODY_HEIGHT * 4) as u64;
const VIEWPORT: ViewportLayoutConfig = ViewportLayoutConfig::new(SOURCE_LINE_WIDTH, 4);

fn fixture() -> Result<String> {
    let estimated = (CHANGED_LINES + 4)
        .checked_mul(SOURCE_LINE_WIDTH + 1)
        .context("large-input fixture capacity overflowed usize")?;
    let mut input = String::with_capacity(estimated);
    let payload = "x".repeat(SOURCE_PAYLOAD_WIDTH);
    writeln!(input, "diff --git a/large.txt b/large.txt")?;
    writeln!(input, "--- a/large.txt")?;
    writeln!(input, "+++ b/large.txt")?;
    writeln!(input, "@@ -1,{LINES_PER_SIDE} +1,{LINES_PER_SIDE} @@")?;
    for line in 0..LINES_PER_SIDE {
        writeln!(input, "-line-{line:05} {payload}")?;
    }
    for line in 0..LINES_PER_SIDE {
        writeln!(input, "+line-{line:05} {payload}")?;
    }
    Ok(input)
}

fn parse_fixture(input: &str) -> Result<(DocumentSnapshot, AppendStore, StructuralStorage)> {
    let raw = AppendStore::with_memory_limit(RAW_MEMORY_BYTES);
    // StorageConfigの第2引数はdirectory reserveである。
    // 512 KiBをdirectoryへ残すことで、2 MiB total ledgerのpayload page枠が要求値の1.5 MiBになる。
    let config = StorageConfig::new(
        STRUCTURAL_TOTAL_BYTES,
        STRUCTURAL_DIRECTORY_BYTES,
        STRUCTURAL_PAGE_BYTES,
        STRUCTURAL_EXTENT_BYTES,
    )?;
    let storage = StructuralStorage::with_config(config)?;
    let mut parser = GitParser::new(raw.clone(), &storage)?;
    for chunk in input.as_bytes().chunks(PARSER_CHUNK_BYTES) {
        let start = raw.append(chunk)?;
        let _ = parser.push(start, chunk)?;
    }
    let _ = raw.seal()?;
    let document = parser.into_document()?;
    Ok((document, raw, storage))
}

fn assert_line(
    reader: &mut DocumentLineReader,
    document: &DocumentSnapshot,
    index: usize,
    expected_text: &str,
    expected_kind: LineKind,
) -> Result<()> {
    let line_id = document
        .line_id_at(index)
        .with_context(|| format!("document does not contain line index {index}"))?;
    let line = reader
        .read(document, line_id)?
        .with_context(|| format!("line {line_id} disappeared from its snapshot"))?;
    assert_eq!(line.text, expected_text);
    assert_eq!(line.kind, expected_kind);
    Ok(())
}

fn assert_frame_misses_bounded(
    label: &str,
    before: ViewportLayoutStats,
    after: ViewportLayoutStats,
) {
    let decoded = after.decoded_lines.saturating_sub(before.decoded_lines);
    let misses = after.layout_misses.saturating_sub(before.layout_misses);
    let metadata_reads = after.metadata_reads.saturating_sub(before.metadata_reads);
    let streaming_raw_bytes = after
        .streaming_raw_bytes_read
        .saturating_sub(before.streaming_raw_bytes_read);
    assert!(
        decoded <= FRAME_MISS_LIMIT,
        "{label} decoded {decoded} logical lines for a {BODY_HEIGHT}-row frame"
    );
    assert!(
        misses <= FRAME_MISS_LIMIT,
        "{label} missed {misses} layouts for a {BODY_HEIGHT}-row frame"
    );
    assert!(
        metadata_reads <= FRAME_METADATA_READ_LIMIT,
        "{label} read {metadata_reads} metadata records for a {BODY_HEIGHT}-row frame"
    );
    assert_eq!(
        streaming_raw_bytes, 0,
        "{label} unexpectedly used the huge-line streaming decoder"
    );
}

#[test]
fn reduced_large_input_spills_and_keeps_viewport_work_bounded() -> Result<()> {
    let input = fixture()?;
    let (document, raw, storage) = parse_fixture(&input)?;
    assert_eq!(document.line_count(), CHANGED_LINES + 4);
    assert!(document.is_complete());
    assert!(raw.is_spilled()?);
    let counts = document.structure_counts()?;
    assert_eq!(counts.commits, 1);
    assert_eq!(counts.files, 1);
    assert_eq!(counts.hunks, 1);
    let file_id = document
        .structure()
        .file_id_at(0)
        .context("large fixture has no file occurrence")?;
    let mut structure_reader = StructureMetadataReader::new();
    let file = structure_reader
        .read_file(document.structure(), file_id)?
        .context("large fixture file disappeared from its snapshot")?;
    assert_eq!(file.additions, Some(LINES_PER_SIDE as u64));
    assert_eq!(file.deletions, Some(LINES_PER_SIDE as u64));

    let storage_stats = storage.stats()?;
    assert!(storage_stats.spill_file_allocated);
    assert!(storage_stats.spill_reserved_bytes > 0);
    assert_eq!(storage_stats.resident.limit, STRUCTURAL_TOTAL_BYTES);
    assert!(storage_stats.resident.payload <= STRUCTURAL_PAYLOAD_BYTES);
    assert!(storage_stats.resident.directory <= STRUCTURAL_DIRECTORY_BYTES);
    assert!(storage_stats.resident.total <= STRUCTURAL_TOTAL_BYTES);

    let payload = "x".repeat(SOURCE_PAYLOAD_WIDTH);
    let mut reader = DocumentLineReader::new();
    assert_line(
        &mut reader,
        &document,
        0,
        "diff --git a/large.txt b/large.txt",
        LineKind::DiffHeader,
    )?;
    assert_line(
        &mut reader,
        &document,
        4 + LINES_PER_SIDE,
        &format!("+line-00000 {payload}"),
        LineKind::Addition,
    )?;
    assert_line(
        &mut reader,
        &document,
        document.line_count() - 1,
        &format!("+line-12499 {payload}"),
        LineKind::Addition,
    )?;

    let mut viewport = ViewportLayout::new();
    let before_top = viewport.stats();
    let top = viewport.collect(&document, None, BODY_HEIGHT, VIEWPORT)?;
    let after_top = viewport.stats();
    assert!(!top.rows.is_empty());
    assert!(top.rows.len() <= BODY_HEIGHT);
    assert_eq!(top.top.map(|anchor| anchor.line_id.get()), Some(0));
    assert_frame_misses_bounded("top frame", before_top, after_top);
    drop(top);

    let before_last = viewport.stats();
    let last_top = viewport.bottom_aligned_top(&document, BODY_HEIGHT, VIEWPORT)?;
    let last = viewport.collect(&document, last_top, BODY_HEIGHT, VIEWPORT)?;
    let after_last = viewport.stats();
    assert!(!last.rows.is_empty());
    assert!(last.rows.len() <= BODY_HEIGHT);
    assert!(last.at_end);
    assert_frame_misses_bounded("last frame", before_last, after_last);
    Ok(())
}
