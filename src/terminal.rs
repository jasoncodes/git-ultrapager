//! Raw mode、alternate screen、cursor、job-control の lifecycle guard。
//!
//! `TerminalSession` だけが fullscreen terminal state を所有する。constructor の途中失敗、normal
//! return、panic unwind、suspend の各経路で同じ restore sequence を通し、renderer や AppState は
//! escape command を直接送らない。stdin は Git pipe なので、crossterm の `/dev/tty` event source
//! と stdout backend を分離したまま使う。

use std::fs::{File, OpenOptions};
use std::io::{self, Stdout, Write};
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, unbounded};
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

/// Signal-hook thread と shutdown handle。
pub struct SignalWatcher {
    pub events: Receiver<SignalEvent>,
    handle: SignalHandle,
    join: Option<JoinHandle<()>>,
}

impl SignalWatcher {
    /// Termination と external `SIGTSTP` をmain-loop eventへ変換する。
    pub fn spawn() -> io::Result<Self> {
        let mut signals = Signals::new([SIGHUP, SIGTERM, SIGQUIT, SIGTSTP])?;
        let handle = signals.handle();
        // Bounded channel のblocking sendは、queueが満杯のままwatcherをdropするとjoinを永久に
        // 待たせる。signal-hookが同一signalをflagでまとめるため、unboundedでもsignal stormごとに
        // allocationする経路にはならず、shutdownをreceiverの消費状態から独立させられる。
        let (sender, receiver) = unbounded();
        let join = thread::Builder::new()
            .name("git-ultrapager-signals".to_owned())
            .spawn(move || {
                for signal in signals.forever() {
                    let event = if signal == SIGTSTP {
                        SignalEvent::Suspend
                    } else {
                        SignalEvent::Terminate(signal)
                    };
                    if sender.send(event).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            events: receiver,
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
}
