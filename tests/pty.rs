#![cfg(unix)]

//! 実binaryをcontrolling PTY内で動かし、pipe stdinとterminal lifecycleの組合せを検証する。
//!
//! Reader threadはPTY outputを常時drainし、test threadはdeadline付きchannel waitだけを行う。
//! `PtySession`のdropは未終了childをkillし、writer/masterのcloseで子孫processにもhangupを送るため、
//! assertion failureやtimeoutが後続testとCI runnerを占有しない。

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};
use rustix::process::{Pid, Signal, getpgrp, kill_process, kill_process_group};

const ENTER_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049h";
const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049l";
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const ENABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004h";
const DISABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004l";
const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";

const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const EXIT_TIMEOUT: Duration = Duration::from_secs(3);
const OUTPUT_QUIET: Duration = Duration::from_millis(100);

const PIPE_SCRIPT: &str = r#"printf '%s' "$GUP_FIXTURE" | "$GUP_BIN" --paging always"#;
const SIGNAL_SCRIPT: &str = r#"printf '%s' "$GUP_FIXTURE" | "$GUP_BIN" --paging always & pager=$!; printf '\nGUP_PID=%s\n' "$pager"; wait "$pager""#;
const STREAMING_SCRIPT: &str = r#"i=0; while [ "$i" -lt 30 ]; do printf 'stream-%s\n' "$i" || exit 0; i=$((i + 1)); sleep 0.1; done | "$GUP_BIN" --paging always"#;

enum OutputEvent {
    Data(Vec<u8>),
    Closed,
}

struct PtySession {
    // Drop時はchildをkillしてからwriter/masterを閉じ、shellの子孫をPTYから切り離す。
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    output_events: Receiver<OutputEvent>,
    output: Vec<u8>,
    finished: bool,
}

impl PtySession {
    fn spawn(script: &str, fixture: &str, size: PtySize) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(size).context("failed to open PTY")?;
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", script]);
        command.env("GUP_BIN", env!("CARGO_BIN_EXE_git-ultrapager"));
        command.env("GUP_FIXTURE", fixture);
        command.env("TERM", "xterm-256color");
        command.env("NO_COLOR", "1");

        let child = pair
            .slave
            .spawn_command(command)
            .context("failed to spawn PTY shell")?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("failed to take PTY writer")?;
        let (output_sender, output_events) = mpsc::channel();
        let join = thread::Builder::new()
            .name("git-ultrapager-pty-reader".to_owned())
            .spawn(move || {
                let mut buffer = [0_u8; 4096];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) | Err(_) => {
                            let _ = output_sender.send(OutputEvent::Closed);
                            return;
                        }
                        Ok(count) => {
                            if output_sender
                                .send(OutputEvent::Data(buffer[..count].to_vec()))
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
            })
            .context("failed to spawn PTY reader")?;
        // Blocking PTY readはsession closeで終了する。test終了条件へjoinを持ち込まない。
        drop(join);

        Ok(Self {
            child,
            writer,
            master: pair.master,
            output_events,
            output: Vec::new(),
            finished: false,
        })
    }

    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer
            .write_all(bytes)
            .context("failed to write PTY input")?;
        self.writer.flush().context("failed to flush PTY input")
    }

    fn resize(&self, size: PtySize) -> Result<()> {
        self.master.resize(size).context("failed to resize PTY")
    }

    fn wait_for_bytes(&mut self, needle: &[u8], timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if contains_bytes(&self.output, needle) {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!(
                    "timed out waiting for {:?}; output tail: {:?}",
                    String::from_utf8_lossy(needle),
                    self.output_tail()
                );
            }
            match self.output_events.recv_timeout(remaining) {
                Ok(OutputEvent::Data(bytes)) => self.output.extend_from_slice(&bytes),
                Ok(OutputEvent::Closed) => {
                    bail!(
                        "PTY closed before {:?}; output tail: {:?}",
                        String::from_utf8_lossy(needle),
                        self.output_tail()
                    )
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    bail!(
                        "PTY reader disconnected; output tail: {:?}",
                        self.output_tail()
                    )
                }
            }
        }
    }

    fn wait_for_occurrences(
        &mut self,
        needle: &[u8],
        expected: usize,
        timeout: Duration,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if count_bytes(&self.output, needle) >= expected {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!(
                    "timed out waiting for {expected} occurrences of {:?}; output tail: {:?}",
                    String::from_utf8_lossy(needle),
                    self.output_tail()
                );
            }
            match self.output_events.recv_timeout(remaining) {
                Ok(OutputEvent::Data(bytes)) => self.output.extend_from_slice(&bytes),
                Ok(OutputEvent::Closed) => bail!(
                    "PTY closed before {expected} occurrences of {:?}; output tail: {:?}",
                    String::from_utf8_lossy(needle),
                    self.output_tail()
                ),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    bail!(
                        "PTY reader disconnected; output tail: {:?}",
                        self.output_tail()
                    )
                }
            }
        }
    }

    fn wait_for_growth(&mut self, previous_len: usize, timeout: Duration) -> Result<()> {
        let target = previous_len.saturating_add(16);
        let deadline = Instant::now() + timeout;
        while self.output.len() < target {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!(
                    "timed out waiting for redraw; output grew from {previous_len} to {} bytes",
                    self.output.len()
                );
            }
            match self.output_events.recv_timeout(remaining) {
                Ok(OutputEvent::Data(bytes)) => self.output.extend_from_slice(&bytes),
                Ok(OutputEvent::Closed) => bail!("PTY closed before resize redraw"),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => bail!(
                    "PTY reader disconnected before resize redraw; output tail: {:?}",
                    self.output_tail()
                ),
            }
        }
        Ok(())
    }

    fn wait_until_quiet(&mut self, quiet: Duration, maximum: Duration) -> Result<()> {
        let maximum_deadline = Instant::now() + maximum;
        let mut quiet_deadline = Instant::now() + quiet;
        loop {
            let now = Instant::now();
            if now >= quiet_deadline {
                return Ok(());
            }
            if now >= maximum_deadline {
                bail!("PTY output did not become quiet")
            }
            let wait = quiet_deadline
                .min(maximum_deadline)
                .saturating_duration_since(now);
            match self.output_events.recv_timeout(wait) {
                Ok(OutputEvent::Data(bytes)) => {
                    self.output.extend_from_slice(&bytes);
                    quiet_deadline = Instant::now() + quiet;
                }
                Ok(OutputEvent::Closed) => return Ok(()),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            self.drain_available();
            if let Some(status) = self.child.try_wait().context("failed to poll PTY child")? {
                self.finished = true;
                self.collect_exit_output();
                return Ok(status);
            }
            if Instant::now() >= deadline {
                bail!(
                    "PTY child did not exit; output tail: {:?}",
                    self.output_tail()
                )
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn drain_available(&mut self) {
        loop {
            match self.output_events.try_recv() {
                Ok(OutputEvent::Data(bytes)) => self.output.extend_from_slice(&bytes),
                Ok(OutputEvent::Closed)
                | Err(TryRecvError::Empty)
                | Err(TryRecvError::Disconnected) => return,
            }
        }
    }

    fn collect_exit_output(&mut self) {
        let deadline = Instant::now() + Duration::from_millis(250);
        loop {
            match self.output_events.recv_timeout(Duration::from_millis(20)) {
                Ok(OutputEvent::Data(bytes)) => self.output.extend_from_slice(&bytes),
                Ok(OutputEvent::Closed) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) if Instant::now() >= deadline => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn output_tail(&self) -> String {
        let start = self.output.len().saturating_sub(512);
        String::from_utf8_lossy(&self.output[start..]).into_owned()
    }

    fn process_group(&self) -> Result<Pid> {
        let raw = self
            .master
            .process_group_leader()
            .context("PTY process-group leader is unavailable")?;
        Pid::from_raw(raw).context("PTY process-group leader is zero")
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.child.kill();
        }
    }
}

fn default_size() -> PtySize {
    PtySize {
        rows: 20,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn fixture(line_count: usize) -> String {
    (1..=line_count)
        .map(|line| {
            format!("line {line:03}: abcdefghijklmnopqrstuvwxyz abcdefghijklmnopqrstuvwxyz\n")
        })
        .collect()
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    (!needle.is_empty())
        .then(|| {
            haystack
                .windows(needle.len())
                .position(|window| window == needle)
        })
        .flatten()
}

fn count_bytes(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

fn assert_restored(output: &[u8]) -> Result<()> {
    let entered = find_bytes(output, ENTER_ALTERNATE_SCREEN)
        .context("alternate-screen entry sequence is missing")?;
    let left = output
        .windows(LEAVE_ALTERNATE_SCREEN.len())
        .rposition(|window| window == LEAVE_ALTERNATE_SCREEN)
        .context("alternate-screen leave sequence is missing")?;
    ensure!(
        left > entered,
        "alternate screen was left before it was entered"
    );
    ensure!(
        output[entered..]
            .windows(HIDE_CURSOR.len())
            .any(|window| window == HIDE_CURSOR),
        "cursor-hide sequence is missing"
    );
    ensure!(
        output[entered..]
            .windows(SHOW_CURSOR.len())
            .any(|window| window == SHOW_CURSOR),
        "cursor-show sequence is missing"
    );
    ensure!(
        output[entered..]
            .windows(ENABLE_BRACKETED_PASTE.len())
            .any(|window| window == ENABLE_BRACKETED_PASTE),
        "bracketed-paste enable sequence is missing"
    );
    ensure!(
        output[entered..]
            .windows(DISABLE_BRACKETED_PASTE.len())
            .any(|window| window == DISABLE_BRACKETED_PASTE),
        "bracketed-paste disable sequence is missing"
    );
    Ok(())
}

fn pager_pid(output: &[u8]) -> Result<Pid> {
    let marker = b"GUP_PID=";
    let start = find_bytes(output, marker)
        .map(|offset| offset + marker.len())
        .context("pager PID marker is missing")?;
    let end = output[start..]
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .map_or(output.len(), |relative| start + relative);
    let raw = std::str::from_utf8(&output[start..end])
        .context("pager PID is not UTF-8")?
        .parse::<i32>()
        .context("pager PID is not an integer")?;
    Pid::from_raw(raw).context("pager PID is zero")
}

#[test]
fn q_leaves_alternate_screen_and_exits_zero() -> Result<()> {
    let mut session = PtySession::spawn(PIPE_SCRIPT, &fixture(80), default_size())?;
    session.wait_for_bytes(ENTER_ALTERNATE_SCREEN, STARTUP_TIMEOUT)?;
    session.send(b"q")?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;

    ensure!(
        status.exit_code() == 0,
        "unexpected exit status: {status:?}; output tail: {:?}",
        session.output_tail()
    );
    assert_restored(&session.output)
}

#[test]
fn ctrl_c_restores_terminal_and_exits_130() -> Result<()> {
    let mut session = PtySession::spawn(PIPE_SCRIPT, &fixture(80), default_size())?;
    session.wait_for_bytes(ENTER_ALTERNATE_SCREEN, STARTUP_TIMEOUT)?;
    session.send(b"\x03")?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;

    ensure!(
        status.exit_code() == 130,
        "Ctrl-C returned an unexpected status: {status:?}; output tail: {:?}",
        session.output_tail()
    );
    assert_restored(&session.output)
}

#[test]
fn resize_redraws_before_clean_quit() -> Result<()> {
    let mut session = PtySession::spawn(PIPE_SCRIPT, &fixture(80), default_size())?;
    session.wait_for_bytes(ENTER_ALTERNATE_SCREEN, STARTUP_TIMEOUT)?;
    session.wait_until_quiet(OUTPUT_QUIET, Duration::from_secs(1))?;
    let before_resize = session.output.len();

    session.resize(PtySize {
        rows: 12,
        cols: 42,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    session.wait_for_growth(before_resize, STARTUP_TIMEOUT)?;
    session.send(b"q")?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;

    ensure!(
        status.exit_code() == 0,
        "unexpected exit status: {status:?}; output tail: {:?}",
        session.output_tail()
    );
    assert_restored(&session.output)
}

#[test]
fn streaming_input_opens_before_eof_and_q_does_not_drain() -> Result<()> {
    let started = Instant::now();
    let mut session = PtySession::spawn(STREAMING_SCRIPT, "", default_size())?;
    session.wait_for_bytes(ENTER_ALTERNATE_SCREEN, Duration::from_secs(1))?;
    ensure!(
        started.elapsed() < Duration::from_secs(1),
        "pager did not enter alternate screen before the delayed stream completed"
    );

    let quit_started = Instant::now();
    session.send(b"q")?;
    let status = session.wait_for_exit(Duration::from_secs(2))?;
    ensure!(
        status.exit_code() == 0,
        "unexpected exit status: {status:?}; output tail: {:?}",
        session.output_tail()
    );
    ensure!(
        quit_started.elapsed() < Duration::from_secs(2),
        "q waited for the streaming producer to finish"
    );
    assert_restored(&session.output)
}

#[test]
fn sigterm_restores_terminal_and_exits_143() -> Result<()> {
    let mut session = PtySession::spawn(SIGNAL_SCRIPT, &fixture(80), default_size())?;
    session.wait_for_bytes(b"GUP_PID=", STARTUP_TIMEOUT)?;
    session.wait_for_bytes(ENTER_ALTERNATE_SCREEN, STARTUP_TIMEOUT)?;
    let pid = pager_pid(&session.output)?;

    kill_process(pid, Signal::TERM).context("failed to send SIGTERM to pager")?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        status.exit_code() == 143,
        "SIGTERM returned an unexpected status: {status:?}"
    );
    assert_restored(&session.output)
}

#[test]
fn ctrl_z_restores_before_stop_and_reenters_after_sigcont() -> Result<()> {
    let mut session = PtySession::spawn(PIPE_SCRIPT, &fixture(80), default_size())?;
    session.wait_for_bytes(ENTER_ALTERNATE_SCREEN, STARTUP_TIMEOUT)?;
    session.wait_until_quiet(OUTPUT_QUIET, Duration::from_secs(1))?;
    let process_group = session.process_group()?;
    ensure!(
        process_group != getpgrp(),
        "refusing to stop the test runner process group"
    );

    session.send(b"\x1a")?;
    session.wait_for_bytes(LEAVE_ALTERNATE_SCREEN, STARTUP_TIMEOUT)?;
    // Restore sequenceはSIGSTOPの直前にflushされる。Readerがsequenceを受けた瞬間はchildがまだ
    // kill_process_groupへ到達していない可能性があるため、CONTをSTOPより先に消費させない。
    thread::sleep(Duration::from_millis(100));
    kill_process_group(process_group, Signal::CONT)
        .context("failed to send SIGCONT to PTY process group")?;
    session.wait_for_occurrences(ENTER_ALTERNATE_SCREEN, 2, STARTUP_TIMEOUT)?;
    // portable-ptyはterminal emulatorではないため、Ratatui clearが行うDSR queryへtest harnessが
    // cursor responseを返す。通常のterminalではemulatorが同じresponseを生成する。
    session.wait_for_bytes(CURSOR_POSITION_QUERY, STARTUP_TIMEOUT)?;
    session.send(b"\x1b[1;1R")?;
    session.wait_until_quiet(OUTPUT_QUIET, Duration::from_secs(1))?;

    session.send(b"q")?;
    let status = session.wait_for_exit(EXIT_TIMEOUT)?;
    ensure!(
        status.exit_code() == 0,
        "suspended pager returned an unexpected status: {status:?}; output tail: {:?}",
        session.output_tail()
    );
    ensure!(
        count_bytes(&session.output, LEAVE_ALTERNATE_SCREEN) >= 2,
        "terminal was not restored after both suspend and quit"
    );
    assert_restored(&session.output)
}
