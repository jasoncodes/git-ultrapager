//! Process mode の選択と各 subsystem の lifecycle を調停する。
//!
//! stdout が TTY でない経路はANSI sequenceだけを除くbyte passthrough、TTY direct 経路はcomplete
//! document、interactive 経路は stdin reader・search worker・terminal event を `AppState` へ集約する。
//! reader と worker の thread は user quit 時に join しない。receiver を drop して process を返す
//! ことで、pipe の読み切りや実行中 regex を終了条件に持ち込まない。

use std::io::{self, IsTerminal, Read, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use crossbeam_channel::{RecvTimeoutError, TryRecvError};
use crossterm::event::{self, Event as CrosstermEvent};

use crate::ansi::AnsiStripper;
use crate::cli::{Cli, PagingChoice};
use crate::decorations::{
    AppliedDecorationSet, DecorationComputer, DecorationRequest, DecorationWorker,
};
use crate::derived_memory::DerivedMemoryLedger;
use crate::direct::{
    DocumentLayoutError, LayoutError, count_rendered_rows_up_to, write_direct_decorated_range,
};
use crate::document::{DocumentError, DocumentSnapshot};
use crate::model::HunkId;
use crate::parser::GitParser;
use crate::runtime::{
    FileFilterJob, FileFilterWorker, FileFilterWorkerEvent, ReaderEvent, ReaderRuntime, SearchJob,
    SearchWorker, SearchWorkerEvent,
};
use crate::state::{AppState, Effect, Event as StateEvent};
use crate::storage::StructuralStorage;
use crate::structure_store::{StructureMetadataReader, StructureStoreError};
use crate::terminal::{SignalEvent, SignalWatcher, TerminalError, TerminalSession};
use crate::theme::{Theme, detect_color_level};
use crate::ui::FrameLoader;

const READ_CHUNK_BYTES: usize = 32 * 1024;
const AUTO_PROBE_TIME: Duration = Duration::from_millis(100);
const EVENT_POLL_TIME: Duration = Duration::from_millis(16);
const DOCUMENT_PUBLISH_TIME: Duration = Duration::from_millis(33);
const DEFAULT_TERMINAL_WIDTH: u16 = 80;
const DEFAULT_TERMINAL_HEIGHT: u16 = 24;

/// CLI、TTY capability、stdin の状態から実行 mode を選び、完了まで所有する。
pub fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    if io::stdin().is_terminal() {
        eprintln!(
            "git-ultrapager reads Git output from stdin.\n\n  git config --global core.pager git-ultrapager\n  cat patch.diff | git-ultrapager"
        );
        return Ok(ExitCode::from(2));
    }

    let stdout_is_terminal = io::stdout().is_terminal();
    let dumb_terminal = std::env::var("TERM")
        .ok()
        .is_none_or(|term| term.is_empty() || term == "dumb");
    if !stdout_is_terminal || dumb_terminal {
        return passthrough(io::stdin().lock(), io::stdout().lock());
    }

    let (width, height) = startup_terminal_size();
    let mut reader = ReaderRuntime::spawn_stdin().context("failed to spawn stdin reader")?;
    let storage = StructuralStorage::new().context("failed to initialize structural storage")?;
    let mut parser = GitParser::new(reader.store.clone(), &storage)
        .context("failed to initialize Git parser")?;
    if cli.paging == PagingChoice::Never {
        return run_streaming_direct(&cli, width, &mut parser, &mut reader);
    }
    let startup = probe_startup(
        cli.paging,
        width,
        height,
        usize::from(cli.tab_width),
        &mut parser,
        &mut reader,
    )?;

    if startup.input_bytes == 0 {
        return Ok(ExitCode::SUCCESS);
    }

    match startup.mode {
        StartupMode::Direct => {
            let document = parser
                .snapshot()
                .context("failed to snapshot parsed input")?;
            write_completed_direct(&cli, &document, width)
        }
        StartupMode::Interactive => {
            // Signal handler はraw/alternate modeより先に有効化する。mode設定直後のTERM/HUPも
            // main loopへ届き、TerminalSessionを復元してから終了できる。
            let signals = SignalWatcher::spawn().context("failed to install signal watcher")?;
            match TerminalSession::enter() {
                Ok(terminal) => {
                    run_interactive(&cli, parser, reader, startup.eof, terminal, signals)
                }
                Err(TerminalError::NoControllingTty(_)) if cli.paging == PagingChoice::Auto => {
                    drop(signals);
                    if !startup.eof {
                        drain_to_eof(&mut reader, &mut parser)?;
                    }
                    let document = parser
                        .snapshot()
                        .context("failed to snapshot parsed input")?;
                    write_completed_direct(&cli, &document, width)
                }
                Err(error) => Err(anyhow::Error::new(error)),
            }
        }
    }
}

/// stdout fd のwindow sizeを使い、controlling TTYを起動判定の前提にしない。Pseudo terminalが
/// 0を返す場合はdirect layoutとauto thresholdだけ80x24へ寄せ、interactive開始後はbackendが
/// 取得した実sizeとResize eventを使う。
fn startup_terminal_size() -> (u16, u16) {
    let size = rustix::termios::tcgetwinsize(io::stdout()).ok();
    let width = size
        .map(|value| value.ws_col)
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_TERMINAL_WIDTH);
    let height = size
        .map(|value| value.ws_row)
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_TERMINAL_HEIGHT);
    (width, height)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartupMode {
    Direct,
    Interactive,
}

#[derive(Clone, Copy, Debug)]
struct StartupOutcome {
    mode: StartupMode,
    eof: bool,
    input_bytes: u64,
}

/// 最初の byte 後100 ms、screen 高、EOF の3条件を同じ loop で評価する。
fn probe_startup(
    paging: PagingChoice,
    width: u16,
    height: u16,
    tab_width: usize,
    parser: &mut GitParser,
    reader: &mut ReaderRuntime,
) -> Result<StartupOutcome> {
    let mut first_byte_at: Option<Instant> = None;
    let mut input_bytes = 0_u64;

    loop {
        let event = if paging == PagingChoice::Auto {
            match first_byte_at {
                Some(started) => match AUTO_PROBE_TIME.checked_sub(started.elapsed()) {
                    Some(timeout) => match reader.events.recv_timeout(timeout) {
                        Ok(event) => event,
                        Err(RecvTimeoutError::Timeout) => {
                            return Ok(StartupOutcome {
                                mode: StartupMode::Interactive,
                                eof: false,
                                input_bytes,
                            });
                        }
                        Err(RecvTimeoutError::Disconnected) => {
                            bail!("stdin reader channel disconnected before reporting EOF")
                        }
                    },
                    None => {
                        return Ok(StartupOutcome {
                            mode: StartupMode::Interactive,
                            eof: false,
                            input_bytes,
                        });
                    }
                },
                None => reader.events.recv().map_err(|_| {
                    anyhow!("stdin reader channel disconnected before reporting EOF")
                })?,
            }
        } else {
            reader
                .events
                .recv()
                .map_err(|_| anyhow!("stdin reader channel disconnected before reporting EOF"))?
        };

        match event {
            ReaderEvent::Data { raw_start, bytes } => {
                if first_byte_at.is_none() {
                    first_byte_at = Some(Instant::now());
                }
                input_bytes = input_bytes.saturating_add(bytes.len() as u64);
                let _ = parser
                    .push(raw_start, &bytes)
                    .context("failed to parse startup input")?;
            }
            ReaderEvent::Eof => {
                let _ = parser
                    .finish()
                    .context("failed to finalize startup input")?;
                let rows = if paging == PagingChoice::Auto {
                    probe_rendered_rows(parser, usize::from(width), tab_width, usize::from(height))?
                } else {
                    0
                };
                let mode = match paging {
                    PagingChoice::Always if input_bytes > 0 => StartupMode::Interactive,
                    PagingChoice::Auto if rows >= usize::from(height) => StartupMode::Interactive,
                    _ => StartupMode::Direct,
                };
                return Ok(StartupOutcome {
                    mode,
                    eof: true,
                    input_bytes,
                });
            }
            ReaderEvent::Failed(error) => {
                return Err(anyhow::Error::new(error));
            }
        }

        let rows = if paging == PagingChoice::Auto {
            probe_rendered_rows(parser, usize::from(width), tab_width, usize::from(height))?
        } else {
            0
        };
        if let Some(mode) = startup_mode(
            paging,
            input_bytes > 0,
            false,
            rows,
            usize::from(height),
            first_byte_at.is_some_and(|started| started.elapsed() >= AUTO_PROBE_TIME),
        ) {
            return Ok(StartupOutcome {
                mode,
                eof: false,
                input_bytes,
            });
        }
    }
}

fn startup_mode(
    paging: PagingChoice,
    has_input: bool,
    eof: bool,
    rendered_rows: usize,
    terminal_height: usize,
    probe_expired: bool,
) -> Option<StartupMode> {
    if eof {
        return Some(match paging {
            PagingChoice::Always if has_input => StartupMode::Interactive,
            PagingChoice::Auto if rendered_rows >= terminal_height => StartupMode::Interactive,
            _ => StartupMode::Direct,
        });
    }
    match paging {
        PagingChoice::Never => None,
        PagingChoice::Always => has_input.then_some(StartupMode::Interactive),
        PagingChoice::Auto => (has_input && (rendered_rows >= terminal_height || probe_expired))
            .then_some(StartupMode::Interactive),
    }
}

/// Startup threshold向けに現在snapshotのrow数を数える。
///
/// 幅不足はinteractiveのtoo-small画面でrecoverできるため閾値超過として扱う。Raw/record read failureや
/// snapshot不整合は表示modeで回復しないので、起動判定へ丸めずcallerへ返す。
fn probe_rendered_rows(
    parser: &GitParser,
    width: usize,
    tab_width: usize,
    limit: usize,
) -> Result<usize> {
    let document = parser
        .snapshot()
        .context("failed to snapshot startup input")?;
    match count_rendered_rows_up_to(&document, width, tab_width, limit) {
        Ok(rows) => Ok(rows),
        Err(DocumentLayoutError::Layout(LayoutError::TooNarrow { .. })) => Ok(usize::MAX),
        Err(error) => Err(anyhow::Error::new(error)),
    }
}

/// Error/panic unwindでもsignal handlerをterminal復元後まで維持するdrop順guard。
/// Rustはfield宣言順にdropするため、`terminal`を`signals`より先に置く。
struct InteractiveGuards {
    terminal: TerminalSession,
    signals: SignalWatcher,
}

fn run_interactive(
    cli: &Cli,
    mut parser: GitParser,
    mut reader: ReaderRuntime,
    mut eof: bool,
    terminal: TerminalSession,
    signals: SignalWatcher,
) -> Result<ExitCode> {
    let mut guards = InteractiveGuards { terminal, signals };
    let size = guards
        .terminal
        .size()
        .context("failed to query interactive terminal size")?;
    let theme = Theme::new(cli.theme, detect_color_level(cli.no_color));
    let derived_memory = Arc::new(DerivedMemoryLedger::new());
    let initial_document = Arc::new(
        parser
            .snapshot()
            .context("failed to snapshot initial interactive input")?,
    );
    let mut state = AppState::new(
        initial_document,
        size.width,
        size.height,
        usize::from(cli.tab_width),
        cli.whitespace,
        eof,
    )
    .context("failed to build initial viewport")?;
    // 構造metadataのfallback走査位置と再利用bufferはframe間で保持する。毎frame作り直すと、
    // open fileを含むstreaming入力で同じprefixを繰り返しdecodeしてしまう。
    let mut frame_loader = FrameLoader::new();
    let search = SearchWorker::spawn().context("failed to spawn search worker")?;
    let file_filter = FileFilterWorker::spawn().context("failed to spawn file-filter worker")?;
    let decorations_worker = DecorationWorker::spawn(Arc::clone(&derived_memory))
        .context("failed to spawn decoration worker")?;
    let mut decoration_generation = 0_u64;
    let mut decorations = AppliedDecorationSet::empty(&derived_memory)
        .context("failed to initialize applied decoration memory")?;
    let mut redraw = true;
    let mut document_dirty = false;
    let mut last_document_publish = Instant::now();
    submit_decorations(
        &decorations_worker,
        &mut decoration_generation,
        &mut state,
        &theme,
    )?;

    loop {
        if let Some(code) = drain_signals(&guards.signals, &mut guards.terminal, &mut redraw)? {
            guards.terminal.restore().map_err(anyhow::Error::new)?;
            return Ok(ExitCode::from(code));
        }

        document_dirty |= drain_reader_updates(&mut reader, &mut parser, &mut eof)?;
        if document_dirty && (eof || last_document_publish.elapsed() >= DOCUMENT_PUBLISH_TIME) {
            let document = Arc::new(
                parser
                    .snapshot()
                    .context("failed to snapshot streaming input")?,
            );
            let effects = state
                .update(StateEvent::DocumentUpdated { document, eof })
                .context("failed to update streaming viewport")?;
            // Stable LineIdはappend後も同じだが、open file/hunkのfallback判定とintraline pairingは
            // snapshot更新で変わり得る。新generationの結果が届くまで旧spanを新textへ重ねない。
            decorations
                .clear()
                .context("failed to release stale applied decorations")?;
            document_dirty = false;
            last_document_publish = Instant::now();
            redraw = true;
            if let Some(code) = apply_effects(
                effects,
                &state,
                &mut guards.terminal,
                &search,
                &file_filter,
                &mut redraw,
            )? {
                guards.terminal.restore().map_err(anyhow::Error::new)?;
                return Ok(ExitCode::from(code));
            }
            submit_decorations(
                &decorations_worker,
                &mut decoration_generation,
                &mut state,
                &theme,
            )?;
        }

        while let Ok(worker_event) = search.responses.try_recv() {
            let previous_hunk = viewport_hunk(&state);
            let event = match worker_event {
                SearchWorkerEvent::Completed {
                    generation,
                    document_generation,
                    result,
                    preview,
                } => StateEvent::SearchCompleted {
                    generation,
                    document_generation,
                    result,
                    preview,
                },
                SearchWorkerEvent::Failed {
                    generation,
                    document_generation,
                    preview,
                    message,
                } => StateEvent::WorkerFailed {
                    generation,
                    document_generation,
                    preview,
                    message,
                },
            };
            let effects = state
                .update(event)
                .context("failed to apply search result")?;
            redraw = true;
            if let Some(code) = apply_effects(
                effects,
                &state,
                &mut guards.terminal,
                &search,
                &file_filter,
                &mut redraw,
            )? {
                guards.terminal.restore().map_err(anyhow::Error::new)?;
                return Ok(ExitCode::from(code));
            }
            if viewport_hunk(&state) != previous_hunk {
                submit_decorations(
                    &decorations_worker,
                    &mut decoration_generation,
                    &mut state,
                    &theme,
                )?;
            }
        }

        while let Ok(worker_event) = file_filter.responses.try_recv() {
            let event = match worker_event {
                FileFilterWorkerEvent::Completed(result) => StateEvent::FileFilterCompleted {
                    generation: result.generation,
                    document_generation: result.document_generation,
                    total_matches: result.total_matches,
                    selected_rank: result.selected_rank,
                    start_rank: result.start_rank,
                    file_ids: result.file_ids,
                },
                FileFilterWorkerEvent::Failed {
                    generation,
                    document_generation,
                    message,
                } => StateEvent::FileFilterFailed {
                    generation,
                    document_generation,
                    message,
                },
            };
            let effects = state
                .update(event)
                .context("failed to apply file-filter result")?;
            redraw = true;
            if let Some(code) = apply_effects(
                effects,
                &state,
                &mut guards.terminal,
                &search,
                &file_filter,
                &mut redraw,
            )? {
                guards.terminal.restore().map_err(anyhow::Error::new)?;
                return Ok(ExitCode::from(code));
            }
        }

        while let Ok(response) = decorations_worker.responses().try_recv() {
            if response.generation != decoration_generation {
                continue;
            }
            let message = match response.result {
                Ok(result) => {
                    let warning = result.warning().map(str::to_owned);
                    decorations
                        .replace(result, &derived_memory)
                        .context("failed to apply decoration result")?;
                    if response.complete {
                        warning
                    } else {
                        Some("highlighting…".to_owned())
                    }
                }
                Err(error) => Some(error.to_string()),
            };
            let _ = state
                .update(StateEvent::BackgroundStatus { message })
                .context("failed to update background status")?;
            redraw = true;
        }

        let timeout = if redraw {
            Duration::ZERO
        } else {
            EVENT_POLL_TIME
        };
        if event::poll(timeout).context("failed to poll terminal events")?
            && let Some(event) = read_terminal_event()?
        {
            let previous_whitespace = state.whitespace();
            let previous_hunk = viewport_hunk(&state);
            let effects = state
                .update(event)
                .context("failed to apply terminal event")?;
            redraw = true;
            if let Some(code) = apply_effects(
                effects,
                &state,
                &mut guards.terminal,
                &search,
                &file_filter,
                &mut redraw,
            )? {
                guards.terminal.restore().map_err(anyhow::Error::new)?;
                return Ok(ExitCode::from(code));
            }
            if state.whitespace() != previous_whitespace || viewport_hunk(&state) != previous_hunk {
                submit_decorations(
                    &decorations_worker,
                    &mut decoration_generation,
                    &mut state,
                    &theme,
                )?;
            }
        }

        if redraw {
            let frame_lines = frame_loader
                .load(&state)
                .context("failed to decode viewport lines")?;
            let follow_up_frame = frame_lines.needs_follow_up_frame();
            guards
                .terminal
                .draw(|frame| {
                    crate::ui::render(frame, &state, &theme, &decorations, &frame_lines);
                })
                .context("failed to draw pager")?;
            // Fallback検出は1 frameにつき4,096 fileまでに制限する。未走査分がある間だけevent
            // loopをもう一周し、terminal inputとworker responseを処理する機会を残して走査を進める。
            redraw = follow_up_frame;
        }
    }
}

fn submit_decorations(
    worker: &DecorationWorker,
    generation: &mut u64,
    state: &mut AppState,
    theme: &Theme,
) -> Result<()> {
    *generation = generation.wrapping_add(1);
    let request = DecorationRequest {
        generation: *generation,
        document: state.document_snapshot(),
        theme: theme.clone(),
        whitespace: state.whitespace(),
        priority_hunk: viewport_hunk(state),
    };
    worker
        .submit_latest(request)
        .map_err(|_| anyhow!("decoration worker disconnected"))?;
    let _ = state
        .update(StateEvent::BackgroundStatus {
            message: Some("highlighting…".to_owned()),
        })
        .context("failed to update decoration status")?;
    Ok(())
}

/// Viewport先頭が属するhunkを返す。Worker requestはこのIDだけを先に計算してpartial resultを
/// 返すため、長いdocumentでも移動先のsyntax/intraline装飾をfull scanより先に描画できる。
fn viewport_hunk(state: &AppState) -> Option<HunkId> {
    state.current_row().and_then(|row| row.hunk_id)
}

fn read_terminal_event() -> Result<Option<StateEvent>> {
    let event = event::read().context("failed to read terminal event")?;
    Ok(match event {
        CrosstermEvent::Key(key) => Some(StateEvent::Key(key)),
        CrosstermEvent::Paste(text) => Some(StateEvent::Paste(text)),
        CrosstermEvent::Resize(width, height) => Some(StateEvent::Resize { width, height }),
        CrosstermEvent::FocusGained | CrosstermEvent::FocusLost | CrosstermEvent::Mouse(_) => None,
    })
}

fn drain_signals(
    signals: &SignalWatcher,
    terminal: &mut TerminalSession,
    redraw: &mut bool,
) -> Result<Option<u8>> {
    while let Ok(event) = signals.events.try_recv() {
        match event {
            SignalEvent::Terminate(signal) => {
                let code = u8::try_from(128_i32.saturating_add(signal)).unwrap_or(u8::MAX);
                return Ok(Some(code));
            }
            SignalEvent::Suspend => {
                terminal.suspend().map_err(anyhow::Error::new)?;
                *redraw = true;
            }
        }
    }
    Ok(None)
}

fn apply_effects(
    effects: Vec<Effect>,
    state: &AppState,
    terminal: &mut TerminalSession,
    search: &SearchWorker,
    file_filter: &FileFilterWorker,
    redraw: &mut bool,
) -> Result<Option<u8>> {
    for effect in effects {
        match effect {
            Effect::Redraw => *redraw = true,
            Effect::Repaint => {
                terminal.clear().context("failed to clear terminal")?;
                *redraw = true;
            }
            Effect::Suspend => {
                terminal.suspend().map_err(anyhow::Error::new)?;
                *redraw = true;
            }
            Effect::Quit(code) => return Ok(Some(code)),
            Effect::CancelSearch => search.cancel_latest(),
            Effect::Search(task) => {
                if task.document_generation != state.document_generation() {
                    bail!(
                        "search task document generation {} does not match current generation {}",
                        task.document_generation.get(),
                        state.document_generation().get()
                    );
                }
                if !search.submit_latest(SearchJob {
                    task,
                    document: state.document_snapshot(),
                }) {
                    bail!("search worker disconnected");
                }
            }
            Effect::CancelFileFilter => file_filter.cancel_latest(),
            Effect::FileFilter(task) => {
                if task.document_generation != state.document_generation() {
                    bail!(
                        "file-filter task document generation {} does not match current generation {}",
                        task.document_generation.get(),
                        state.document_generation().get()
                    );
                }
                if !file_filter.submit_latest(FileFilterJob {
                    task,
                    document: state.document_snapshot(),
                }) {
                    bail!("file-filter worker disconnected");
                }
            }
        }
    }
    Ok(None)
}

fn drain_reader_updates(
    reader: &mut ReaderRuntime,
    parser: &mut GitParser,
    eof: &mut bool,
) -> Result<bool> {
    if *eof {
        return Ok(false);
    }
    let mut changed = false;
    loop {
        match reader.events.try_recv() {
            Ok(ReaderEvent::Data { raw_start, bytes }) => {
                let _ = parser
                    .push(raw_start, &bytes)
                    .context("failed to parse streaming input")?;
                changed = true;
            }
            Ok(ReaderEvent::Eof) => {
                let _ = parser
                    .finish()
                    .context("failed to finalize streaming input")?;
                *eof = true;
                changed = true;
                break;
            }
            Ok(ReaderEvent::Failed(error)) => {
                return Err(anyhow::Error::new(error));
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                bail!("stdin reader channel disconnected before reporting EOF")
            }
        }
    }
    Ok(changed)
}

/// Auto mode で controlling TTY を開けなかった時だけ、残りを同期的に受けて direct document を
/// 完成させる。interactive session へ入った後の `q` では呼ばず、upstream の停止を妨げない。
fn drain_to_eof(reader: &mut ReaderRuntime, parser: &mut GitParser) -> Result<()> {
    loop {
        match reader.events.recv() {
            Ok(ReaderEvent::Data { raw_start, bytes }) => {
                let _ = parser
                    .push(raw_start, &bytes)
                    .context("failed to parse fallback direct input")?;
            }
            Ok(ReaderEvent::Eof) => {
                let _ = parser
                    .finish()
                    .context("failed to finalize fallback direct input")?;
                return Ok(());
            }
            Ok(ReaderEvent::Failed(error)) => return Err(anyhow::Error::new(error)),
            Err(_) => bail!("stdin reader channel disconnected before reporting EOF"),
        }
    }
}

/// `--paging never`はfile entryが閉じるたびに出力し、producerのEOFまでdocument全体を待たない。
/// Plain textはgutter確定を必要としないため、受信済みlogical lineだけを逐次layoutする。
fn run_streaming_direct(
    cli: &Cli,
    width: u16,
    parser: &mut GitParser,
    reader: &mut ReaderRuntime,
) -> Result<ExitCode> {
    let theme = Theme::new(cli.theme, detect_color_level(cli.no_color));
    let decoration_computer = DecorationComputer::new();
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    let mut emitted_lines = 0_usize;
    let settings = DirectRenderSettings {
        theme: &theme,
        decoration_computer: &decoration_computer,
        width: usize::from(width),
        tab_width: usize::from(cli.tab_width),
        whitespace: cli.whitespace,
    };

    loop {
        let event = reader
            .events
            .recv()
            .map_err(|_| anyhow!("stdin reader channel disconnected before reporting EOF"))?;
        let eof = match event {
            ReaderEvent::Data { raw_start, bytes } => {
                let _ = parser
                    .push(raw_start, &bytes)
                    .context("failed to parse direct input")?;
                false
            }
            ReaderEvent::Eof => {
                let _ = parser.finish().context("failed to finalize direct input")?;
                true
            }
            ReaderEvent::Failed(error) => return Err(anyhow::Error::new(error)),
        };

        let document = parser
            .snapshot()
            .context("failed to snapshot direct input")?;
        if let Err(error) =
            emit_direct_prefix(&mut writer, &document, &settings, eof, &mut emitted_lines)
        {
            if matches!(&error, crate::direct::DirectWriteError::Io(source) if source.kind() == io::ErrorKind::BrokenPipe)
            {
                return Ok(ExitCode::SUCCESS);
            }
            return Err(
                anyhow::Error::new(error).context("failed to write streaming direct output")
            );
        }
        if eof {
            return Ok(ExitCode::SUCCESS);
        }
    }
}

struct DirectRenderSettings<'theme> {
    theme: &'theme Theme,
    decoration_computer: &'theme DecorationComputer,
    width: usize,
    tab_width: usize,
    whitespace: crate::cli::WhitespaceChoice,
}

fn emit_direct_prefix<W: Write>(
    writer: &mut W,
    document: &DocumentSnapshot,
    settings: &DirectRenderSettings<'_>,
    eof: bool,
    emitted_lines: &mut usize,
) -> Result<(), crate::direct::DirectWriteError> {
    let mut structure_reader = StructureMetadataReader::new();
    let safe = safe_direct_line_exclusive(document, eof, &mut structure_reader)?;
    if safe <= *emitted_lines {
        return Ok(());
    }

    let mut cursor = *emitted_lines;
    while cursor < safe {
        let segment = next_direct_segment(document, cursor, safe, &mut structure_reader)?;
        let decorations = settings.decoration_computer.compute_range(
            document,
            settings.theme,
            settings.whitespace,
            segment.clone(),
        )?;
        write_direct_decorated_range(
            writer,
            document,
            &decorations,
            settings.theme,
            segment.clone(),
            settings.width,
            settings.tab_width,
        )?;
        cursor = segment.end;
    }
    *emitted_lines = safe;
    Ok(())
}

/// Direct streamをplain metadataまたは1 file entryずつに区切る。
///
/// File spanはGit出力順で重ならない。`partition_point`でcursor直後のfileをO(log files)で探し、
/// decoration/layoutの一時所有範囲を1 entryへ制限する。EOF時のcommit metadataも次file開始までの
/// metadata segmentとして返す。
fn next_direct_segment(
    document: &DocumentSnapshot,
    cursor: usize,
    safe_end: usize,
    reader: &mut StructureMetadataReader,
) -> Result<std::ops::Range<usize>, crate::direct::DirectWriteError> {
    let cursor_u64 = u64::try_from(cursor)
        .map_err(|_| crate::direct::DirectWriteError::UnavailableLineIndex { index: cursor })?;
    let safe_u64 = u64::try_from(safe_end)
        .map_err(|_| crate::direct::DirectWriteError::UnavailableLineIndex { index: safe_end })?;
    let files = reader
        .files_intersecting(document.structure(), cursor_u64..safe_u64)
        .map_err(DocumentError::from)?;
    let Some(file_id) = files.first() else {
        return Ok(cursor..safe_end);
    };
    let file = reader
        .read_file(document.structure(), file_id)
        .map_err(DocumentError::from)?
        .ok_or_else(|| {
            crate::direct::DirectWriteError::Document(DocumentError::StructureStore(
                StructureStoreError::OutsideSnapshot {
                    kind: "file",
                    id: file_id.get(),
                    high_water: document.structure().file_count(),
                },
            ))
        })?;
    let file_start = line_index(file.span.start.get(), document.line_count());
    if cursor < file_start {
        return Ok(cursor..file_start.min(safe_end));
    }
    let file_end = file
        .span
        .end_exclusive
        .map_or(document.line_count(), |end| {
            line_index(end, document.line_count())
        });
    Ok(cursor
        ..file_end
            .min(safe_end)
            .max(cursor.saturating_add(1).min(safe_end)))
}

fn safe_direct_line_exclusive(
    document: &DocumentSnapshot,
    eof: bool,
    reader: &mut StructureMetadataReader,
) -> Result<usize, crate::direct::DirectWriteError> {
    if eof {
        return Ok(document.line_count());
    }
    let counts = document
        .structure()
        .visible_counts()
        .map_err(DocumentError::from)?;
    let Some(last_ordinal) = counts.files.checked_sub(1) else {
        return Ok(document.line_count());
    };
    let last_id = document.structure().file_id_at(last_ordinal).ok_or({
        crate::direct::DirectWriteError::Document(DocumentError::StructureStore(
            StructureStoreError::OutsideSnapshot {
                kind: "file",
                id: last_ordinal,
                high_water: counts.files,
            },
        ))
    })?;
    let last = reader
        .read_file(document.structure(), last_id)
        .map_err(DocumentError::from)?
        .ok_or_else(|| {
            crate::direct::DirectWriteError::Document(DocumentError::StructureStore(
                StructureStoreError::OutsideSnapshot {
                    kind: "file",
                    id: last_id.get(),
                    high_water: counts.files,
                },
            ))
        })?;
    if last.span.end_exclusive.is_some() {
        // 最後のfileが閉じていれば、その後に届いたcommit/plain metadataはgutterやhunk pairingで
        // 遡って変わらない。次file/EOFを待たず、現在のhigh-water markまで出力できる。
        Ok(document.line_count())
    } else {
        Ok(line_index(last.span.start.get(), document.line_count()))
    }
}

fn line_index(value: u64, document_len: usize) -> usize {
    usize::try_from(value)
        .unwrap_or(document_len)
        .min(document_len)
}

fn write_completed_direct(cli: &Cli, document: &DocumentSnapshot, width: u16) -> Result<ExitCode> {
    let theme = Theme::new(cli.theme, detect_color_level(cli.no_color));
    let decoration_computer = DecorationComputer::new();
    let settings = DirectRenderSettings {
        theme: &theme,
        decoration_computer: &decoration_computer,
        width: usize::from(width),
        tab_width: usize::from(cli.tab_width),
        whitespace: cli.whitespace,
    };
    let mut stdout = io::stdout().lock();
    let mut emitted_lines = 0_usize;
    match emit_direct_prefix(&mut stdout, document, &settings, true, &mut emitted_lines) {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(crate::direct::DirectWriteError::Io(error))
            if error.kind() == io::ErrorKind::BrokenPipe =>
        {
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => Err(anyhow::Error::new(error).context("failed to write terminal output")),
    }
}

fn passthrough<R: Read, W: Write>(mut reader: R, mut writer: W) -> Result<ExitCode> {
    let mut stripper = AnsiStripper::default();
    let mut input = [0_u8; READ_CHUNK_BYTES];
    loop {
        let count = reader.read(&mut input).context("failed to read stdin")?;
        if count == 0 {
            let mut sanitized_tail = Vec::new();
            stripper.finish_into(&mut sanitized_tail);
            if let Err(error) = writer.write_all(&sanitized_tail) {
                if error.kind() == io::ErrorKind::BrokenPipe {
                    return Ok(ExitCode::SUCCESS);
                }
                return Err(error).context("failed to write passthrough output");
            }
            break;
        }
        let mut sanitized = Vec::with_capacity(count);
        stripper.push(&input[..count], &mut sanitized);
        if let Err(error) = writer.write_all(&sanitized) {
            if error.kind() == io::ErrorKind::BrokenPipe {
                return Ok(ExitCode::SUCCESS);
            }
            return Err(error).context("failed to write passthrough output");
        }
    }
    if let Err(error) = writer.flush() {
        if error.kind() == io::ErrorKind::BrokenPipe {
            return Ok(ExitCode::SUCCESS);
        }
        bail!("failed to flush passthrough output: {error}");
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::cli::{ThemeChoice, WhitespaceChoice};
    use crate::theme::ColorLevel;

    fn parser_for_reader(reader: &ReaderRuntime) -> GitParser {
        let storage = StructuralStorage::new().unwrap();
        GitParser::new(reader.store.clone(), &storage).unwrap()
    }

    fn parser_with_store() -> (crate::input::AppendStore, GitParser) {
        let raw = crate::input::AppendStore::new();
        let storage = StructuralStorage::new().unwrap();
        let parser = GitParser::new(raw.clone(), &storage).unwrap();
        (raw, parser)
    }

    fn push_stored(parser: &mut GitParser, raw: &crate::input::AppendStore, bytes: &[u8]) {
        let raw_start = raw.append(bytes).unwrap();
        let _ = parser.push(raw_start, bytes).unwrap();
    }

    #[test]
    fn passthrough_preserves_invalid_bytes_and_newline_state() {
        let mut output = Vec::new();
        let code = passthrough(&b"a\xff\x1b[31mb"[..], &mut output).unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(output, b"a\xffb");
    }

    #[test]
    fn passthrough_replays_invalid_utf8_c1_and_flushes_safe_eof_tail() {
        let mut output = Vec::new();
        let code = passthrough(&b"a\xf0\x9b31mb\xe0\xa0"[..], &mut output).unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(output, b"a\xf0b\xe0\xa0");
    }

    #[test]
    fn auto_probe_pages_at_exact_terminal_height() {
        assert_eq!(
            startup_mode(PagingChoice::Auto, true, false, 40, 40, false),
            Some(StartupMode::Interactive)
        );
        assert_eq!(
            startup_mode(PagingChoice::Auto, true, true, 39, 40, false),
            Some(StartupMode::Direct)
        );
    }

    #[test]
    fn always_still_waits_for_the_first_byte() {
        assert_eq!(
            startup_mode(PagingChoice::Always, false, false, 0, 40, true),
            None
        );
        assert_eq!(
            startup_mode(PagingChoice::Always, false, true, 0, 40, true),
            Some(StartupMode::Direct)
        );
    }

    #[test]
    fn never_drains_to_eof_without_probe_timeout() {
        let mut reader = ReaderRuntime::spawn(Cursor::new(b"hello\n".to_vec())).unwrap();
        let mut parser = parser_for_reader(&reader);
        let outcome =
            probe_startup(PagingChoice::Never, 80, 24, 4, &mut parser, &mut reader).unwrap();

        assert_eq!(outcome.mode, StartupMode::Direct);
        assert!(outcome.eof);
        assert_eq!(outcome.input_bytes, 6);
        assert!(parser.snapshot().unwrap().is_complete());
    }

    #[test]
    fn always_opens_after_first_nonempty_chunk() {
        let mut reader = ReaderRuntime::spawn(Cursor::new(b"hello\n".to_vec())).unwrap();
        let mut parser = parser_for_reader(&reader);
        let outcome =
            probe_startup(PagingChoice::Always, 80, 24, 4, &mut parser, &mut reader).unwrap();

        assert_eq!(outcome.mode, StartupMode::Interactive);
        assert!(!outcome.eof);
        assert_eq!(outcome.input_bytes, 6);
    }

    #[test]
    fn auto_sends_short_completed_input_to_direct_output() {
        let mut reader = ReaderRuntime::spawn(Cursor::new(b"hello\n".to_vec())).unwrap();
        let mut parser = parser_for_reader(&reader);
        let outcome =
            probe_startup(PagingChoice::Auto, 80, 24, 4, &mut parser, &mut reader).unwrap();

        assert_eq!(outcome.mode, StartupMode::Direct);
        assert!(outcome.eof);
        assert_eq!(outcome.input_bytes, 6);
    }

    #[test]
    fn streaming_direct_emits_plain_lines_but_holds_open_file() {
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        let decoration_computer = DecorationComputer::new();
        let (raw, mut parser) = parser_with_store();
        let mut output = Vec::new();
        let mut emitted = 0;
        let settings = DirectRenderSettings {
            theme: &theme,
            decoration_computer: &decoration_computer,
            width: 80,
            tab_width: 4,
            whitespace: WhitespaceChoice::Ignore,
        };

        push_stored(&mut parser, &raw, b"preamble\n");
        let document = parser.snapshot().unwrap();
        emit_direct_prefix(&mut output, &document, &settings, false, &mut emitted).unwrap();
        assert!(String::from_utf8_lossy(&output).contains("preamble"));

        let before_file = output.len();
        push_stored(
            &mut parser,
            &raw,
            b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n",
        );
        let document = parser.snapshot().unwrap();
        emit_direct_prefix(&mut output, &document, &settings, false, &mut emitted).unwrap();
        assert_eq!(output.len(), before_file);

        push_stored(&mut parser, &raw, b"diff --git a/b.rs b/b.rs\n");
        let document = parser.snapshot().unwrap();
        emit_direct_prefix(&mut output, &document, &settings, false, &mut emitted).unwrap();
        let plain = crate::ansi::strip_ansi(&output);
        let rendered = String::from_utf8_lossy(&plain);
        assert!(rendered.contains("│-old"));
        assert!(rendered.contains("│+new"));
        assert!(!rendered.contains("b/b.rs"));

        push_stored(
            &mut parser,
            &raw,
            b"--- a/b.rs\n+++ b/b.rs\n@@ -1 +1 @@\n-before\n+after\n",
        );
        let document = parser.snapshot().unwrap();
        emit_direct_prefix(&mut output, &document, &settings, false, &mut emitted).unwrap();
        let before_eof = crate::ansi::strip_ansi(&output);
        assert!(!String::from_utf8_lossy(&before_eof).contains("before"));

        raw.seal().unwrap();
        parser.finish().unwrap();
        let document = parser.snapshot().unwrap();
        emit_direct_prefix(&mut output, &document, &settings, true, &mut emitted).unwrap();
        let plain = crate::ansi::strip_ansi(&output);
        let rendered = String::from_utf8_lossy(&plain);
        assert_eq!(rendered.matches("diff --git a/a.rs b/a.rs").count(), 1);
        assert_eq!(rendered.matches("diff --git a/b.rs b/b.rs").count(), 1);
        assert!(rendered.contains("│-before"));
        assert!(rendered.contains("│+after"));
    }

    #[test]
    fn streaming_direct_emits_metadata_after_the_last_file_closes() {
        let theme = Theme::new(ThemeChoice::Dark, ColorLevel::None);
        let decoration_computer = DecorationComputer::new();
        let settings = DirectRenderSettings {
            theme: &theme,
            decoration_computer: &decoration_computer,
            width: 80,
            tab_width: 4,
            whitespace: WhitespaceChoice::Ignore,
        };
        let (raw, mut parser) = parser_with_store();
        let mut output = Vec::new();
        let mut emitted = 0;
        push_stored(
            &mut parser,
            &raw,
            b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n",
        );
        let document = parser.snapshot().unwrap();
        emit_direct_prefix(&mut output, &document, &settings, false, &mut emitted).unwrap();
        assert!(output.is_empty());

        push_stored(
            &mut parser,
            &raw,
            b"commit 0123456789abcdef0123456789abcdef01234567\n",
        );
        let document = parser.snapshot().unwrap();
        emit_direct_prefix(&mut output, &document, &settings, false, &mut emitted).unwrap();
        let rendered = String::from_utf8_lossy(&output);
        assert!(rendered.contains("diff --git a/a.rs b/a.rs"));
        assert!(rendered.contains("commit 0123456789abcdef"));
    }
}
