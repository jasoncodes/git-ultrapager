//! Process mode の選択と各 subsystem の lifecycle を調停する。
//!
//! stdout が TTY でない経路は byte-preserving passthrough、TTY direct 経路は complete document、
//! interactive 経路は stdin reader・search worker・terminal event を `AppState` へ集約する。reader
//! と worker の thread は user quit 時に join しない。receiver を drop して process を返すことで、
//! pipe の読み切りや実行中 regex を終了条件に持ち込まない。

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
use crate::direct::{DocumentLayout, layout_document, write_direct};
use crate::model::Document;
use crate::parser::GitParser;
use crate::runtime::{ReaderEvent, ReaderRuntime, SearchJob, SearchWorker, SearchWorkerEvent};
use crate::state::{AppState, Effect, Event as StateEvent};
use crate::terminal::{SignalEvent, SignalWatcher, TerminalError, TerminalSession};
use crate::theme::{Theme, detect_color_level};

const READ_CHUNK_BYTES: usize = 32 * 1024;
const AUTO_PROBE_TIME: Duration = Duration::from_millis(100);
const EVENT_POLL_TIME: Duration = Duration::from_millis(16);
const DOCUMENT_PUBLISH_TIME: Duration = Duration::from_millis(33);
const SEARCH_QUEUE_DEPTH: usize = 2;
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
    let mut parser = GitParser::new();
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
        StartupMode::Direct => write_completed_direct(&cli, parser.document(), width),
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
                    write_completed_direct(&cli, parser.document(), width)
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
            ReaderEvent::Data(bytes) => {
                if first_byte_at.is_none() {
                    first_byte_at = Some(Instant::now());
                }
                input_bytes = input_bytes.saturating_add(bytes.len() as u64);
                let _ = parser.push(&bytes);
            }
            ReaderEvent::Eof => {
                let _ = parser.finish();
                let rows = if paging == PagingChoice::Auto {
                    layout_document(parser.document(), usize::from(width), tab_width)
                        .map_or(usize::MAX, |layout| layout.len())
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
            layout_document(parser.document(), usize::from(width), tab_width)
                .map_or(usize::MAX, |layout| layout.len())
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
    let mut state = AppState::new(
        Arc::new(parser.document().clone()),
        size.width,
        size.height,
        usize::from(cli.tab_width),
        cli.whitespace,
        eof,
    );
    let search =
        SearchWorker::spawn(SEARCH_QUEUE_DEPTH).context("failed to spawn search worker")?;
    let mut redraw = true;
    let mut document_dirty = false;
    let mut last_document_publish = Instant::now();

    loop {
        if let Some(code) = drain_signals(&guards.signals, &mut guards.terminal, &mut redraw)? {
            guards.terminal.restore().map_err(anyhow::Error::new)?;
            return Ok(ExitCode::from(code));
        }

        document_dirty |= drain_reader_updates(&mut reader, &mut parser, &mut eof)?;
        if document_dirty && (eof || last_document_publish.elapsed() >= DOCUMENT_PUBLISH_TIME) {
            let effects = state.update(StateEvent::DocumentUpdated {
                document: Arc::new(parser.document().clone()),
                eof,
            });
            document_dirty = false;
            last_document_publish = Instant::now();
            redraw = true;
            if let Some(code) =
                apply_effects(effects, &state, &mut guards.terminal, &search, &mut redraw)?
            {
                guards.terminal.restore().map_err(anyhow::Error::new)?;
                return Ok(ExitCode::from(code));
            }
        }

        while let Ok(worker_event) = search.responses.try_recv() {
            let event = match worker_event {
                SearchWorkerEvent::Completed {
                    generation,
                    result,
                    preview,
                } => StateEvent::SearchCompleted {
                    generation,
                    result,
                    preview,
                },
                SearchWorkerEvent::Failed {
                    generation,
                    message,
                } => {
                    if generation != state.search().generation() {
                        continue;
                    }
                    StateEvent::WorkerFailed { message }
                }
            };
            let effects = state.update(event);
            redraw = true;
            if let Some(code) =
                apply_effects(effects, &state, &mut guards.terminal, &search, &mut redraw)?
            {
                guards.terminal.restore().map_err(anyhow::Error::new)?;
                return Ok(ExitCode::from(code));
            }
        }

        let timeout = if redraw {
            Duration::ZERO
        } else {
            EVENT_POLL_TIME
        };
        if event::poll(timeout).context("failed to poll terminal events")?
            && let Some(event) = read_terminal_event()?
        {
            let effects = state.update(event);
            redraw = true;
            if let Some(code) =
                apply_effects(effects, &state, &mut guards.terminal, &search, &mut redraw)?
            {
                guards.terminal.restore().map_err(anyhow::Error::new)?;
                return Ok(ExitCode::from(code));
            }
        }

        if redraw {
            guards
                .terminal
                .draw(|frame| crate::ui::render(frame, &state, &theme))
                .context("failed to draw pager")?;
            redraw = false;
        }
    }
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
            Effect::Search(task) => {
                if !search.submit_latest(SearchJob {
                    task,
                    document: state.document_snapshot(),
                }) {
                    bail!("search worker disconnected");
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
            Ok(ReaderEvent::Data(bytes)) => {
                let _ = parser.push(&bytes);
                changed = true;
            }
            Ok(ReaderEvent::Eof) => {
                let _ = parser.finish();
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
            Ok(ReaderEvent::Data(bytes)) => {
                let _ = parser.push(&bytes);
            }
            Ok(ReaderEvent::Eof) => {
                let _ = parser.finish();
                return Ok(());
            }
            Ok(ReaderEvent::Failed(error)) => return Err(anyhow::Error::new(error)),
            Err(_) => bail!("stdin reader channel disconnected before reporting EOF"),
        }
    }
}

fn write_completed_direct(cli: &Cli, document: &Document, width: u16) -> Result<ExitCode> {
    let layout = layout_document(document, usize::from(width), usize::from(cli.tab_width))?;
    write_tty_direct(cli, &layout)
}

fn write_tty_direct(cli: &Cli, layout: &DocumentLayout) -> Result<ExitCode> {
    let theme = Theme::new(cli.theme, detect_color_level(cli.no_color));
    let mut stdout = io::stdout().lock();
    match write_direct(&mut stdout, layout, &theme) {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(ExitCode::SUCCESS),
        Err(error) => Err(error).context("failed to write terminal output"),
    }
}

fn passthrough<R: Read, W: Write>(mut reader: R, mut writer: W) -> Result<ExitCode> {
    let mut stripper = AnsiStripper::default();
    let mut input = [0_u8; READ_CHUNK_BYTES];
    loop {
        let count = reader.read(&mut input).context("failed to read stdin")?;
        if count == 0 {
            stripper.finish();
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

    #[test]
    fn passthrough_preserves_invalid_bytes_and_newline_state() {
        let mut output = Vec::new();
        let code = passthrough(&b"a\xff\x1b[31mb"[..], &mut output).unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(output, b"a\xffb");
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
        let mut parser = GitParser::new();
        let outcome =
            probe_startup(PagingChoice::Never, 80, 24, 4, &mut parser, &mut reader).unwrap();

        assert_eq!(outcome.mode, StartupMode::Direct);
        assert!(outcome.eof);
        assert_eq!(outcome.input_bytes, 6);
        assert!(parser.document().is_complete());
    }

    #[test]
    fn always_opens_after_first_nonempty_chunk() {
        let mut reader = ReaderRuntime::spawn(Cursor::new(b"hello\n".to_vec())).unwrap();
        let mut parser = GitParser::new();
        let outcome =
            probe_startup(PagingChoice::Always, 80, 24, 4, &mut parser, &mut reader).unwrap();

        assert_eq!(outcome.mode, StartupMode::Interactive);
        assert!(!outcome.eof);
        assert_eq!(outcome.input_bytes, 6);
    }

    #[test]
    fn auto_sends_short_completed_input_to_direct_output() {
        let mut reader = ReaderRuntime::spawn(Cursor::new(b"hello\n".to_vec())).unwrap();
        let mut parser = GitParser::new();
        let outcome =
            probe_startup(PagingChoice::Auto, 80, 24, 4, &mut parser, &mut reader).unwrap();

        assert_eq!(outcome.mode, StartupMode::Direct);
        assert!(outcome.eof);
        assert_eq!(outcome.input_bytes, 6);
    }
}
