//! Raw mode、alternate screen、cursor、job-control の lifecycle guard。
//!
//! `TerminalSession` だけが fullscreen terminal state を所有する。constructor の途中失敗、normal
//! return、panic unwind、suspend の各経路で同じ restore sequence を通し、renderer や AppState は
//! escape command を直接送らない。stdin は Git pipe なので、crossterm の `/dev/tty` event source
//! と stdout backend を分離したまま使う。

use std::fs::{File, OpenOptions};
use std::io::{self, Stdout, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread::{self, JoinHandle};

use crossbeam_channel::TryRecvError;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use rustix::process::{Signal, getpgrp, getpid, kill_process, kill_process_group};
use rustix::termios::tcgetpgrp;
use signal_hook::consts::signal::{SIGHUP, SIGQUIT, SIGTERM, SIGTSTP};
use signal_hook::iterator::{Handle as SignalHandle, Signals};
use thiserror::Error;

/// Fullscreen terminal lifecycle の failure。
#[derive(Debug, Error)]
pub enum TerminalError {
    #[error("controlling terminal is unavailable: {0}")]
    NoControllingTty(#[source] io::Error),
    #[error("terminal I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("job-control operation failed: {0}")]
    JobControl(#[source] rustix::io::Errno),
}

/// Main loop が処理する Unix lifecycle signal。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalEvent {
    Terminate(i32),
    Suspend,
}

const PENDING_HUP: u8 = 1 << 0;
const PENDING_TERM: u8 = 1 << 1;
const PENDING_QUIT: u8 = 1 << 2;
const PENDING_SUSPEND: u8 = 1 << 3;

/// Signal iteratorからmain loopへ渡す4種類のpending flag。
///
/// Producerは`fetch_or`だけを行うため、main threadが停止中でも待たず、同じsignalの連続到着は1 bitへ
/// 畳み込まれる。異なるtermination signalは別bitに残し、consumerはterminationをsuspendより先に返す。
/// Termination待ちの状態でjob controlへ入ると終了が`SIGCONT`まで遅れるため、この優先順にする。
#[derive(Default)]
struct PendingSignals {
    bits: AtomicU8,
    closed: AtomicBool,
}

impl PendingSignals {
    fn record(&self, signal: i32) {
        let bit = match signal {
            SIGHUP => PENDING_HUP,
            SIGTERM => PENDING_TERM,
            SIGQUIT => PENDING_QUIT,
            SIGTSTP => PENDING_SUSPEND,
            _ => return,
        };
        self.bits.fetch_or(bit, Ordering::Release);
    }

    fn try_take(&self) -> Result<SignalEvent, TryRecvError> {
        loop {
            let pending = self.bits.load(Ordering::Acquire);
            let Some((bit, event)) = next_signal_event(pending) else {
                if !self.closed.load(Ordering::Acquire) {
                    return Err(TryRecvError::Empty);
                }
                // Producerは最後のbitをpublishしてからclosedを立てる。最初のbits loadとcloseの間に
                // signalが届いた場合を拾い直し、pending eventを残したままDisconnectedを返さない。
                if self.bits.load(Ordering::Acquire) == 0 {
                    return Err(TryRecvError::Disconnected);
                }
                continue;
            };
            if self
                .bits
                .compare_exchange_weak(pending, pending & !bit, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(event);
            }
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

fn next_signal_event(pending: u8) -> Option<(u8, SignalEvent)> {
    [
        (PENDING_HUP, SignalEvent::Terminate(SIGHUP)),
        (PENDING_TERM, SignalEvent::Terminate(SIGTERM)),
        (PENDING_QUIT, SignalEvent::Terminate(SIGQUIT)),
        (PENDING_SUSPEND, SignalEvent::Suspend),
    ]
    .into_iter()
    .find(|(bit, _)| pending & bit != 0)
}

/// `Receiver`と同じnonblocking read契約を持つ、固定4 bitのsignal event queue。
pub struct SignalEvents {
    pending: Arc<PendingSignals>,
}

impl SignalEvents {
    /// Pending eventを1件返す。同じsignalの未処理eventは1件へcoalesceされる。
    pub fn try_recv(&self) -> Result<SignalEvent, TryRecvError> {
        self.pending.try_take()
    }
}

/// Signal-hook thread と shutdown handle。
pub struct SignalWatcher {
    pub events: SignalEvents,
    handle: SignalHandle,
    join: Option<JoinHandle<()>>,
}

impl SignalWatcher {
    /// Termination と external `SIGTSTP` をmain-loop eventへ変換する。
    pub fn spawn() -> io::Result<Self> {
        let mut signals = Signals::new([SIGHUP, SIGTERM, SIGQUIT, SIGTSTP])?;
        let handle = signals.handle();
        let pending = Arc::new(PendingSignals::default());
        let worker_pending = Arc::clone(&pending);
        let join = thread::Builder::new()
            .name("git-ultrapager-signals".to_owned())
            .spawn(move || {
                for signal in signals.forever() {
                    // Atomic flag更新はallocationも待機も行わない。同じsignalがmain loopの処理より
                    // 速く届いてもpending bitを維持し、watcher shutdownをconsumerから独立させる。
                    worker_pending.record(signal);
                }
                worker_pending.close();
            })?;
        Ok(Self {
            events: SignalEvents { pending },
            handle,
            join: Some(join),
        })
    }
}

impl Drop for SignalWatcher {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Active fullscreen terminal と復元状態を所有する RAII guard。
pub struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    active: bool,
}

impl TerminalSession {
    /// Controlling TTY を事前検査し、raw mode と alternate screen へ入る。
    pub fn enter() -> Result<Self, TerminalError> {
        preflight_controlling_tty()?;
        enable_terminal_modes()?;

        let backend = CrosstermBackend::new(io::stdout());
        let terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = restore_terminal_modes();
                return Err(TerminalError::Io(error));
            }
        };
        Ok(Self {
            terminal,
            active: true,
        })
    }

    /// Ratatui frame を描画する。I/O はmain threadからだけ呼ぶ。
    pub fn draw<F>(&mut self, render: F) -> io::Result<()>
    where
        F: FnOnce(&mut ratatui::Frame<'_>),
    {
        self.terminal.draw(render).map(|_| ())
    }

    /// Crossterm が報告する現在の drawing area を返す。
    pub fn size(&self) -> io::Result<ratatui::layout::Size> {
        self.terminal.size()
    }

    /// 画面内容を捨て、次のdrawで全cellを書き直す。
    pub fn clear(&mut self) -> io::Result<()> {
        self.terminal.clear()
    }

    /// Terminal を復元した状態で foreground job を停止し、`fg` 後にfullscreen modeへ戻る。
    pub fn suspend(&mut self) -> Result<(), TerminalError> {
        self.restore()?;
        suspend_foreground_job()?;
        enable_terminal_modes()?;
        self.active = true;
        self.terminal.clear()?;
        Ok(())
    }

    /// Normal/error return 前に terminal mode を明示的に復元する。
    pub fn restore(&mut self) -> Result<(), TerminalError> {
        if self.active {
            // 一部のescape書き込みだけ失敗した場合はactiveを残し、Dropでもう一度全復元操作を試す。
            restore_terminal_modes()?;
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            let _ = restore_terminal_modes();
        }
    }
}

/// `/dev/tty` をread/writeで開き、interactive input が成立するかを判定する。
pub fn preflight_controlling_tty() -> Result<File, TerminalError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(TerminalError::NoControllingTty)
}

fn enable_terminal_modes() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    let result = execute!(stdout, EnterAlternateScreen, EnableBracketedPaste, Hide)
        .and_then(|()| stdout.flush());
    if let Err(error) = result {
        // `execute!` は途中のcommandまで書いた後に失敗し得るため、raw modeだけでなくcursor、
        // bracketed paste、alternate screenも同じtransaction内で巻き戻す。
        let _ = restore_terminal_modes();
        return Err(error);
    }
    Ok(())
}

fn restore_terminal_modes() -> io::Result<()> {
    let mut first_error = None;
    let mut stdout = io::stdout();
    record_first_error(&mut first_error, execute!(stdout, Show));
    record_first_error(&mut first_error, execute!(stdout, DisableBracketedPaste));
    record_first_error(&mut first_error, execute!(stdout, LeaveAlternateScreen));
    record_first_error(&mut first_error, disable_raw_mode());
    record_first_error(&mut first_error, stdout.flush());
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn record_first_error(first_error: &mut Option<io::Error>, result: io::Result<()>) {
    if let Err(error) = result
        && first_error.is_none()
    {
        *first_error = Some(error);
    }
}

fn suspend_foreground_job() -> Result<(), TerminalError> {
    let tty = preflight_controlling_tty()?;
    let process_group = getpgrp();
    let foreground_group = tcgetpgrp(&tty).map_err(TerminalError::JobControl)?;

    // SIGTSTP はsignal watcherが捕捉するため、復元後の実停止にはcatch不能なSIGSTOPを使う。
    // 通常のcore.pager起動ではGitとpagerが同じforeground groupに属し、両方が停止する。
    if foreground_group == process_group {
        kill_process_group(process_group, Signal::STOP).map_err(TerminalError::JobControl)
    } else {
        kill_process(getpid(), Signal::STOP).map_err(TerminalError::JobControl)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_events_preserve_shell_exit_codes() {
        assert_eq!(SignalEvent::Terminate(SIGTERM), SignalEvent::Terminate(15));
        assert_eq!(128 + SIGQUIT, 131);
    }

    #[test]
    fn pending_signals_coalesce_duplicates_without_dropping_distinct_signals() {
        let pending = PendingSignals::default();
        pending.record(SIGTSTP);
        pending.record(SIGTSTP);
        pending.record(SIGTERM);
        pending.record(SIGQUIT);

        assert_eq!(pending.try_take(), Ok(SignalEvent::Terminate(SIGTERM)));
        assert_eq!(pending.try_take(), Ok(SignalEvent::Terminate(SIGQUIT)));
        assert_eq!(pending.try_take(), Ok(SignalEvent::Suspend));
        assert_eq!(pending.try_take(), Err(TryRecvError::Empty));
    }

    #[test]
    fn pending_termination_takes_priority_over_suspend() {
        let pending = PendingSignals::default();
        pending.record(SIGTSTP);
        pending.record(SIGHUP);

        assert_eq!(pending.try_take(), Ok(SignalEvent::Terminate(SIGHUP)));
        assert_eq!(pending.try_take(), Ok(SignalEvent::Suspend));
    }

    #[test]
    fn closed_pending_queue_drains_before_disconnect() {
        let pending = PendingSignals::default();
        pending.record(SIGTERM);
        pending.close();

        assert_eq!(pending.try_take(), Ok(SignalEvent::Terminate(SIGTERM)));
        assert_eq!(pending.try_take(), Err(TryRecvError::Disconnected));
    }
}
