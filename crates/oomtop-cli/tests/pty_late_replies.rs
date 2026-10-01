//! Late answers to the terminal probe (UX §12.10, "tmux/SSH safe"): the TUI runs on a pseudo-terminal that
//! answers the capability queries only after a delay longer than the probe's 200 ms cap — a high-latency SSH
//! link or a slow terminal. The answers then arrive after the probe gave up, while the TUI already reads
//! keys. They must be dropped, not read as keystrokes: before the fix, `ESC ] 11 ; rgb:…` became Alt+`]`,
//! `1`, `1`, `;`, `r`, `g`, `b`, `:` … and the `:` opened the command palette, so `q` never quit.
#![cfg(unix)]

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod common;

/// A full answer (xterm/Ghostty style: ST-terminated OSC, DECRPM, DECRQSS, DA1).
const FULL: &[u8] = b"\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\\x1b[?1u\x1b[?2026;2$y\x1b[?1006;2$y\x1b[?1004;2$y\x1bP1$r38:2:1:2:3m\x1b\\\x1b[?62;22c";
/// Terminal.app style: BEL-terminated OSC, no DECRQM/Kitty answers, DECRQSS refused, DA1.
const MINIMAL: &[u8] = b"\x1b]11;rgb:ffff/ffff/ffff\x07\x1bP0$r\x1b\\\x1b[?1;2c";

fn open_pty() -> (OwnedFd, String) {
    // SAFETY: plain libc pty calls; the returned fd is owned below.
    unsafe {
        let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(m >= 0, "posix_openpt");
        assert_eq!(libc::grantpt(m), 0);
        assert_eq!(libc::unlockpt(m), 0);
        let name = std::ffi::CStr::from_ptr(libc::ptsname(m))
            .to_string_lossy()
            .into_owned();
        (OwnedFd::from_raw_fd(m), name)
    }
}

fn fixture() -> String {
    format!(
        "{}/../../fixtures/macos/m5-air-agents.json",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// Runs the TUI (replaying a fixture) on a pty whose "terminal" answers the probe `delay` after the queries,
/// then presses `q` once the answers are in. Returns (quit, time from `q` to exit, screen output).
fn run_tui(delay: Duration, answers: &[u8]) -> (bool, Duration, String) {
    let (master, slave_name) = open_pty();
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&slave_name)
        .unwrap();
    // 120×30, like a normal terminal window.
    let ws = libc::winsize {
        ws_row: 30,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ on the slave fd with a valid winsize.
    unsafe {
        libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ, &ws);
    }
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oomtop"));
    cmd.args(["--replay", &fixture(), "--offline", "--no-learn"])
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("HOME", dir.path())
        .env("TERM", "xterm-256color")
        // Apple Terminal gets a reduced query (termprobe::query_for); these tests answer the full one.
        .env_remove("TERM_PROGRAM")
        .env_remove("NO_COLOR")
        .env_remove("TMUX")
        .env_remove("OOMTOP_CONFIG")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut guard = common::Reaped::new(cmd.spawn().expect("spawn the oomtop TUI on a pty"));
    let child = guard.child();
    let mut m = std::fs::File::from(master);
    // SAFETY: switch the master to non-blocking reads.
    unsafe {
        let fd = m.as_raw_fd();
        libc::fcntl(
            fd,
            libc::F_SETFL,
            libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK,
        );
    }
    let mut out = Vec::new();
    let mut queried_at = None;
    let mut answered_at = None;
    let mut q_at = None;
    let mut exited = None;
    let start = Instant::now();
    let mut buf = [0u8; 8192];
    while start.elapsed() < Duration::from_secs(30) {
        match m.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => {} // EIO once the child closed the slave
        }
        if queried_at.is_none() && out.windows(3).any(|w| w == b"\x1b[c") {
            queried_at = Some(Instant::now());
        }
        if let (Some(t), None) = (queried_at, answered_at) {
            if t.elapsed() >= delay {
                m.write_all(answers).unwrap();
                answered_at = Some(Instant::now());
            }
        }
        // Give the TUI time to read the answers and draw, then quit.
        if let (Some(t), None) = (answered_at, q_at) {
            if t.elapsed() >= Duration::from_millis(700) {
                m.write_all(b"q").unwrap();
                q_at = Some(Instant::now());
            }
        }
        if let Ok(Some(_)) = child.try_wait() {
            exited = Some(Instant::now());
            break;
        }
        if let Some(q) = q_at {
            if q.elapsed() > Duration::from_secs(5) {
                break;
            }
        }
    }
    let quit = exited.is_some() && q_at.is_some();
    let took = match (q_at, exited) {
        (Some(q), Some(e)) => e.saturating_duration_since(q),
        _ => Duration::MAX,
    };
    if exited.is_none() {
        // Only the child this test spawned.
        let _ = child.kill();
    }
    let _ = child.wait();
    assert!(queried_at.is_some(), "the probe never queried the terminal");
    (quit, took, String::from_utf8_lossy(&out).into_owned())
}

fn assert_quits(delay_ms: u64, answers: &[u8]) {
    let (quit, took, screen) = run_tui(Duration::from_millis(delay_ms), answers);
    assert!(
        quit,
        "answers delayed {delay_ms} ms: `q` did not quit the TUI (answers read as keys?)\n{}",
        screen
            .chars()
            .rev()
            .take(3000)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>()
    );
    assert!(took < Duration::from_secs(3), "quit took {took:?}");
}

#[test]
fn prompt_answers_do_not_reach_the_keymap() {
    assert_quits(0, FULL);
    assert_quits(150, FULL);
}

#[test]
fn late_answers_after_the_probe_cap_are_dropped() {
    // Past the 200 ms cap: the probe has given up and flushed; the TUI must drop them itself.
    assert_quits(250, FULL);
    assert_quits(400, FULL);
    assert_quits(400, MINIMAL);
}

#[test]
fn very_late_answers_are_dropped_too() {
    // A slow SSH round trip: the answer lands while the TUI is already running normally.
    assert_quits(1500, FULL);
}
