//! Release buildでlarge fixtureをparseし、lazy viewportのcollection latencyを採取するmanual harness。
//!
//! Cached sampleは同じtail frameをwarm後に再収集する。
//! Cold sampleはcacheをsample前に空にし、document末尾側の異なるlogical lineからframeを作る。
//! 既定500万行fixtureではraw storeと末尾のlogical-line recordがtempfileへ移るため、cold sampleはpreadとlayout missを含む。
//! 小さいfixtureでspillが起きない場合も、cacheを空にしたcollectionであることは変わらない。
//! 測定範囲は[ViewportLayout::collect]だけであり、key inputからterminal再描画までのlatencyは含まない。
//! Target値はreport labelであり、hardware差を理由にprocessを失敗させない。

use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::hint::black_box;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use git_ultrapager::document::DocumentSnapshot;
use git_ultrapager::input::AppendStore;
use git_ultrapager::parser::GitParser;
use git_ultrapager::storage::StructuralStorage;
use git_ultrapager::viewport::{ViewportLayout, ViewportLayoutConfig, VisualAnchor};

const DEFAULT_SAMPLES: usize = 101;
const MAX_SAMPLES: usize = 10_000;
const READ_CHUNK_BYTES: usize = 64 * 1024;
const VIEWPORT_WIDTH: usize = 120;
const BODY_HEIGHT: usize = 40;
const TAB_WIDTH: usize = 4;
const CACHED_TARGET_MILLIS: f64 = 16.0;
const COLD_TARGET_MILLIS: f64 = 50.0;

struct HarnessConfig {
    fixture: PathBuf,
    samples: usize,
}

#[derive(Clone, Copy)]
struct LatencySummary {
    p50: Duration,
    p95: Duration,
    max: Duration,
}

fn main() -> Result<()> {
    let config = parse_args()?;
    let fixture_bytes = fs::metadata(&config.fixture)
        .with_context(|| format!("could not stat {}", config.fixture.display()))?
        .len();
    let raw = AppendStore::new();
    let storage = StructuralStorage::new()?;
    let parse_started = Instant::now();
    let document = parse_document(&config.fixture, raw.clone(), &storage)?;
    let parse_elapsed = parse_started.elapsed();
    let storage_stats = storage.stats()?;
    let raw_spilled = raw.is_spilled()?;

    let viewport_config = ViewportLayoutConfig::new(VIEWPORT_WIDTH, TAB_WIDTH);
    let tail_index = document.line_count().saturating_sub(BODY_HEIGHT.max(1));
    let tail_line = document
        .line_id_at(tail_index)
        .with_context(|| format!("document has no tail line at index {tail_index}"))?;
    let tail_anchor = VisualAnchor::new(tail_line, 0);

    let mut cached_viewport = ViewportLayout::new();
    let cached_samples = measure_cached(
        &mut cached_viewport,
        &document,
        tail_anchor,
        config.samples,
        viewport_config,
    )?;
    let cached_summary = summarize(cached_samples)?;

    let mut cold_viewport = ViewportLayout::new();
    let cold_samples = measure_cold(
        &mut cold_viewport,
        &document,
        config.samples,
        viewport_config,
    )?;
    let cold_summary = summarize(cold_samples)?;

    println!("fixture: {}", config.fixture.display());
    println!(
        "input: bytes={fixture_bytes} logical_lines={} parse_ms={:.3}",
        document.line_count(),
        millis(parse_elapsed)
    );
    println!("raw_store: bytes={} spilled={raw_spilled}", raw.len()?);
    println!(
        "structural_storage: resident_payload={} resident_directory={} resident_total={} resident_limit={} spill_file_allocated={} spill_reserved_bytes={}",
        storage_stats.resident.payload,
        storage_stats.resident.directory,
        storage_stats.resident.total,
        storage_stats.resident.limit,
        storage_stats.spill_file_allocated,
        storage_stats.spill_reserved_bytes
    );
    print_latency(
        "cached viewport collection",
        CACHED_TARGET_MILLIS,
        config.samples,
        cached_summary,
    );
    print_latency(
        "cold viewport collection",
        COLD_TARGET_MILLIS,
        config.samples,
        cold_summary,
    );
    println!(
        "targets are informational labels for ViewportLayout::collect; this harness does not assert timing or measure end-to-end redraw latency"
    );
    print_viewport_stats("cached", &cached_viewport);
    print_viewport_stats("cold", &cold_viewport);
    Ok(())
}

fn parse_args() -> Result<HarnessConfig> {
    let mut args = env::args_os();
    let program = args
        .next()
        .unwrap_or_else(|| OsString::from("large_profile"));
    let Some(fixture) = args.next() else {
        bail!(
            "usage: {} <fixture-path> [sample-count]",
            Path::new(&program).display()
        );
    };
    let samples = match args.next() {
        Some(value) => parse_samples(value)?,
        None => DEFAULT_SAMPLES,
    };
    if args.next().is_some() {
        bail!(
            "usage: {} <fixture-path> [sample-count]",
            Path::new(&program).display()
        );
    }
    Ok(HarnessConfig {
        fixture: PathBuf::from(fixture),
        samples,
    })
}

fn parse_samples(value: OsString) -> Result<usize> {
    let value = value
        .into_string()
        .map_err(|_| anyhow::anyhow!("sample count is not valid UTF-8"))?;
    let samples = value
        .parse::<usize>()
        .with_context(|| format!("invalid sample count: {value}"))?;
    if !(1..=MAX_SAMPLES).contains(&samples) {
        bail!("sample count must be within 1..={MAX_SAMPLES}");
    }
    Ok(samples)
}

fn parse_document(
    path: &Path,
    raw: AppendStore,
    storage: &StructuralStorage,
) -> Result<DocumentSnapshot> {
    let file = File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let mut input = BufReader::with_capacity(READ_CHUNK_BYTES, file);
    let mut parser = GitParser::new(raw.clone(), storage)?;
    let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
    loop {
        let count = input
            .read(&mut buffer)
            .with_context(|| format!("could not read {}", path.display()))?;
        if count == 0 {
            break;
        }
        let start = raw.append(&buffer[..count])?;
        let _ = parser.push(start, &buffer[..count])?;
    }
    let _ = raw.seal()?;
    parser.into_document().map_err(Into::into)
}

fn measure_cached(
    viewport: &mut ViewportLayout,
    document: &DocumentSnapshot,
    anchor: VisualAnchor,
    sample_count: usize,
    config: ViewportLayoutConfig,
) -> Result<Vec<Duration>> {
    let warm = viewport.collect(document, Some(anchor), BODY_HEIGHT, config)?;
    black_box(warm.rows.len());
    drop(warm);

    let mut samples = Vec::new();
    samples
        .try_reserve_exact(sample_count)
        .context("could not reserve cached latency samples")?;
    for _ in 0..sample_count {
        let started = Instant::now();
        let frame = viewport.collect(document, Some(anchor), BODY_HEIGHT, config)?;
        let elapsed = started.elapsed();
        black_box(frame.rows.len());
        samples.push(elapsed);
    }
    Ok(samples)
}

fn measure_cold(
    viewport: &mut ViewportLayout,
    document: &DocumentSnapshot,
    sample_count: usize,
    config: ViewportLayoutConfig,
) -> Result<Vec<Duration>> {
    let last_start = document.line_count().saturating_sub(BODY_HEIGHT.max(1));
    let tail_span = last_start.min(sample_count.saturating_mul(BODY_HEIGHT));
    let first_start = last_start.saturating_sub(tail_span);
    let denominator = sample_count.saturating_sub(1).max(1);
    let mut samples = Vec::new();
    samples
        .try_reserve_exact(sample_count)
        .context("could not reserve cold latency samples")?;

    for sample in 0..sample_count {
        let relative = tail_span.saturating_mul(sample) / denominator;
        let line_index = first_start.saturating_add(relative).min(last_start);
        let line_id = document
            .line_id_at(line_index)
            .with_context(|| format!("document has no sample line at index {line_index}"))?;
        viewport.clear_cache();
        let started = Instant::now();
        let frame = viewport.collect(
            document,
            Some(VisualAnchor::new(line_id, 0)),
            BODY_HEIGHT,
            config,
        )?;
        let elapsed = started.elapsed();
        black_box(frame.rows.len());
        samples.push(elapsed);
    }
    Ok(samples)
}

fn summarize(mut samples: Vec<Duration>) -> Result<LatencySummary> {
    if samples.is_empty() {
        bail!("latency summary requires at least one sample");
    }
    samples.sort_unstable();
    Ok(LatencySummary {
        p50: nearest_rank(&samples, 50)?,
        p95: nearest_rank(&samples, 95)?,
        max: samples
            .last()
            .copied()
            .context("sorted latency samples unexpectedly became empty")?,
    })
}

fn nearest_rank(sorted: &[Duration], percentile: usize) -> Result<Duration> {
    let numerator = percentile
        .checked_mul(sorted.len())
        .context("percentile rank overflowed usize")?;
    let rank = numerator.div_ceil(100).max(1);
    sorted
        .get(rank - 1)
        .copied()
        .with_context(|| format!("percentile rank {rank} is outside {} samples", sorted.len()))
}

fn print_latency(label: &str, target_millis: f64, samples: usize, summary: LatencySummary) {
    println!(
        "{label}: target_p95<={target_millis:.0}ms samples={samples} p50_ms={:.3} p95_ms={:.3} max_ms={:.3}",
        millis(summary.p50),
        millis(summary.p95),
        millis(summary.max)
    );
}

fn print_viewport_stats(label: &str, viewport: &ViewportLayout) {
    let cache = viewport.cache_usage();
    let stats = viewport.stats();
    println!(
        "{label}_viewport: cache_resident={} cache_live={} cache_limit={} metadata_reads={} decoded_lines={} cache_hits={} layout_misses={} insert_rejections={} streaming_raw_bytes={} streaming_peak_buffered={}",
        cache.resident_bytes,
        cache.live_bytes,
        cache.limit_bytes,
        stats.metadata_reads,
        stats.decoded_lines,
        stats.cache_hits,
        stats.layout_misses,
        stats.cache_insert_rejections,
        stats.streaming_raw_bytes_read,
        stats.streaming_peak_buffered_bytes
    );
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}
