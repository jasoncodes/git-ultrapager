//! 実diffをrelease buildでparseし、全syntax decorationが完成するまでのcold latencyを測る。
//!
//! 測定範囲は[`DecorationComputer::compute`]の初回呼び出しで、syntax assetの初期化、hunk prefix
//! 推定、syntax/intraline span生成を含む。Parserとfixture readは別に表示する。100 msを超えた実行は
//! performance regressionとして非zero終了する。

use std::env;
use std::ffi::OsString;
use std::fs::File;
use std::hint::black_box;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use git_ultrapager::cli::{ThemeChoice, WhitespaceChoice};
use git_ultrapager::decorations::DecorationComputer;
use git_ultrapager::document::DocumentSnapshot;
use git_ultrapager::input::AppendStore;
use git_ultrapager::parser::GitParser;
use git_ultrapager::storage::StructuralStorage;
use git_ultrapager::theme::{ColorLevel, Theme};

const READ_CHUNK_BYTES: usize = 64 * 1024;
const HIGHLIGHT_TARGET: Duration = Duration::from_millis(100);

fn main() -> Result<()> {
    let fixture = parse_args()?;
    let raw = AppendStore::new();
    let storage = StructuralStorage::new()?;
    let parse_started = Instant::now();
    let document = parse_document(&fixture, raw.clone(), &storage)?;
    let parse_elapsed = parse_started.elapsed();

    let computer = DecorationComputer::new();
    let theme = Theme::new(ThemeChoice::Dark, ColorLevel::TrueColor);
    let highlight_started = Instant::now();
    let decorations = computer.compute(&document, &theme, WhitespaceChoice::Ignore)?;
    let highlight_elapsed = highlight_started.elapsed();
    black_box(decorations);

    println!("fixture: {}", fixture.display());
    println!(
        "input: bytes={} logical_lines={} parse_ms={:.3}",
        raw.len()?,
        document.line_count(),
        millis(parse_elapsed),
    );
    println!(
        "cold highlight: target_ms={} actual_ms={:.3}",
        HIGHLIGHT_TARGET.as_millis(),
        millis(highlight_elapsed),
    );
    if highlight_elapsed > HIGHLIGHT_TARGET {
        bail!(
            "cold syntax decoration took {:.3} ms, target is {} ms",
            millis(highlight_elapsed),
            HIGHLIGHT_TARGET.as_millis(),
        );
    }
    Ok(())
}

fn parse_args() -> Result<PathBuf> {
    let mut args = env::args_os();
    let program = args
        .next()
        .unwrap_or_else(|| OsString::from("highlight_profile"));
    let Some(fixture) = args.next() else {
        bail!("usage: {} <fixture-path>", Path::new(&program).display());
    };
    if args.next().is_some() {
        bail!("usage: {} <fixture-path>", Path::new(&program).display());
    }
    Ok(PathBuf::from(fixture))
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

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}
