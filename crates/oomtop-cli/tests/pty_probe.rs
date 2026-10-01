//! The terminal probe on a real pseudo-terminal (UX §12.10): `oomtop doctor` runs with the pty as its
//! controlling terminal (`/dev/tty`), a fake terminal answers every query at once, and the probe must read
//! those answers — background detected, kitty keyboard answered — without echoing them onto the screen.
//! Regression test for macOS, where `poll(2)` on `/dev/tty` returns `POLLNVAL` immediately.
#![cfg(unix)]

use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod common;

const ANSWERS: &[u8] = b"\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\\x1b[?1u\x1b[?2026;2$y\x1b[?1006;2$y\x1b[?1004;2$y\x1bP1$r38:2:1:2:3m\x1b\\\x1b[?62;22c";

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

/// Runs `oomtop doctor` on a fresh pty as its controlling terminal, answering the probe (once it sends the
/// DA1 sentinel) with `answers`. Returns whether the probe queried, and everything written to the pty.
fn doctor_on_pty(term_program: Option<&str>, answers: &[u8]) -> (bool, Vec<u8>) {
    let (master, slave_name) = open_pty();
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&slave_name)
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oomtop"));
    cmd.args(["--offline", "doctor"])
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env("TERM", "xterm-256color")
        .env_remove("TERM_PROGRAM")
        .env_remove("NO_COLOR")
        .env_remove("TMUX")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    if let Some(tp) = term_program {
        cmd.env("TERM_PROGRAM", tp);
    }
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
    let mut guard = common::Reaped::new(cmd.spawn().expect("spawn oomtop doctor on a pty"));
    let child = guard.child();
    let mut m = std::fs::File::from(master);
    // SAFETY: switch the master to non-blocking reads.
    unsafe {
        use std::os::fd::AsRawFd;
        let fd = m.as_raw_fd();
        libc::fcntl(
            fd,
            libc::F_SETFL,
            libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK,
        );
    }
    let mut out = Vec::new();
    let mut answered = false;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut buf = [0u8; 4096];
    loop {
        match m.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break, // EIO once the child closed the slave
        }
        // The queries end with DA1 (`CSI c`): answer everything at once, like a fast local terminal.
        if !answered && out.windows(3).any(|w| w == b"\x1b[c") {
            m.write_all(answers).unwrap();
            answered = true;
        }
        if let Ok(Some(_)) = child.try_wait() {
            // drain what is left
            while let Ok(n) = m.read(&mut buf) {
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
            break;
        }
        assert!(Instant::now() < deadline, "doctor did not finish");
    }
    let _ = child.wait();
    (answered, out)
}

#[test]
fn probe_reads_answers_on_a_controlling_tty_without_echo() {
    // No TERM_PROGRAM: the full query (Apple Terminal gets a reduced one, tested below).
    let (answered, out) = doctor_on_pty(None, ANSWERS);
    let text = String::from_utf8_lossy(&out);
    assert!(answered, "the probe never queried the terminal:\n{text}");
    assert!(
        !text.contains("rgb:1c1c") && !text.contains("62;22c"),
        "the terminal's answers were echoed onto the screen:\n{text}"
    );
    let line = text
        .lines()
        .find(|l| l.contains("OSC 11 background"))
        .unwrap_or_else(|| panic!("no OSC 11 row:\n{text}"));
    assert!(line.contains("#1c1c1c"), "background not detected: {line}");
    let kitty = text.lines().find(|l| l.contains("kitty keyboard")).unwrap();
    assert!(kitty.contains("answered"), "kitty answer not read: {kitty}");
}

/// Apple Terminal prints DECRQM/DECRQSS it doesn't parse ("ppp$qm" before `oomtop doctor` output): it is
/// sent only OSC 11 + DA1, the line is wiped after the probe, and its answers still set the background.
#[test]
fn apple_terminal_is_sent_only_queries_it_answers() {
    let (answered, out) = doctor_on_pty(
        Some("Apple_Terminal"),
        b"\x1b]11;rgb:0d0d/0c0c/0b0b\x07\x1b[?1;2c",
    );
    let text = String::from_utf8_lossy(&out);
    assert!(answered, "the probe never queried the terminal:\n{text}");
    let head = &text[..text.find("oomtop ").unwrap_or(text.len())];
    assert!(!head.contains("$p"), "DECRQM sent to Apple Terminal: {head:?}");
    assert!(
        !head.contains("\x1bP"),
        "DECRQSS sent to Apple Terminal: {head:?}"
    );
    assert!(
        !head.contains("[?u"),
        "kitty query sent to Apple Terminal: {head:?}"
    );
    assert!(head.contains("\r\x1b[2K"), "probe line not wiped: {head:?}");
    let bg = text.lines().find(|l| l.contains("OSC 11 background")).unwrap();
    assert!(bg.contains("#0d0c0b"), "{bg}");
    let mouse = text.lines().find(|l| l.contains("mouse (SGR 1006)")).unwrap();
    assert!(
        mouse.contains("environment"),
        "unasked mode reported as a probe result: {mouse}"
    );
}
