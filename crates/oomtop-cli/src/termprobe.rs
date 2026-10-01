//! Live terminal capability queries (UX §12.10), done once, only when stdin and stdout are both a TTY:
//!
//! | Query | Sent | Answer |
//! |---|---|---|
//! | background color | `OSC 11 ; ? ST` | `OSC 11 ; rgb:RRRR/GGGG/BBBB` (BEL or ST) |
//! | Kitty keyboard protocol | `CSI ? u` | `CSI ? <flags> u` |
//! | synchronized output | `CSI ? 2026 $ p` (DECRQM) | `CSI ? 2026 ; <1-4> $ y` |
//! | SGR mouse | `CSI ? 1006 $ p` | `CSI ? 1006 ; <1-4> $ y` |
//! | focus events | `CSI ? 1004 $ p` | `CSI ? 1004 ; <1-4> $ y` |
//! | truecolor | `CSI 38;2;1;2;3 m` + `DCS $ q m ST` (DECRQSS) + `CSI 0 m` | `DCS 1 $ r …38:2:1:2:3 m ST` if kept |
//! | sentinel | `CSI c` (DA1) | `CSI ? … c` — every terminal answers, so we stop waiting early |
//!
//! The truecolor query sets a foreground color, asks the terminal to report the current SGR, and resets it; a
//! terminal that can't show 24-bit color reports a downsampled `38;5;n` (or nothing). No text is printed
//! while the color is set, so nothing visible changes. Not sent on the Linux VT console (`TERM=linux`),
//! whose parser treats `ESC P` as a palette command.
//!
//! The terminal is put in non-canonical, no-echo mode on `/dev/tty` for at most [`TIMEOUT`] and always
//! restored. Parsing is a pure function ([`parse_replies`]) so it is unit-tested without a terminal.

use serde::Serialize;
use std::time::Duration;

/// Upper bound for the whole probe (terminals answer in < 10 ms locally; SSH adds a round trip). 200 ms keeps
/// the first frame under 300 ms (SPEC §14) even when the terminal never answers; a later answer is flushed.
pub const TIMEOUT: Duration = Duration::from_millis(200);

/// The full query, sentinel last.
pub const QUERY: &[u8] =
    b"\x1b]11;?\x1b\\\x1b[?u\x1b[?2026$p\x1b[?1006$p\x1b[?1004$p\x1b[38;2;1;2;3m\x1bP$qm\x1b\\\x1b[0m\x1b[c";

/// For terminals that *print* queries they don't parse. Apple Terminal renders the final byte of each
/// DECRQM (`CSI ? … $ p` → "p") and the body of DECRQSS (`DCS $ q m ST` → "$qm") as text — the `ppp$qm`
/// seen before `oomtop doctor` output. It answers OSC 11 and DA1, so only those are asked.
pub const QUERY_MINIMAL: &[u8] = b"\x1b]11;?\x1b\\\x1b[c";

/// Returned to column 0 and cleared after the probe: wipes anything a terminal printed for a query it
/// didn't understand (a terminal we don't know about, or Apple Terminal reached over SSH, where
/// `TERM_PROGRAM` isn't forwarded). Runs before anything else is written, so the line holds nothing else.
pub const WIPE_LINE: &[u8] = b"\r\x1b[2K";

/// The query to send for `TERM_PROGRAM`, and whether it asks the DECRQM/kitty/DECRQSS questions.
pub fn query_for(term_program: &str) -> (&'static [u8], bool) {
    match term_program {
        "Apple_Terminal" => (QUERY_MINIMAL, false),
        _ => (QUERY, true),
    }
}

/// DECRQM answer for one private mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeState {
    /// 0: the terminal doesn't recognize the mode.
    NotRecognized,
    /// 1 / 2 / 3: supported (set, reset, permanently set).
    Supported,
    /// 4: permanently reset.
    PermanentlyReset,
}

impl ModeState {
    fn from_code(c: u32) -> Self {
        match c {
            1..=3 => ModeState::Supported,
            4 => ModeState::PermanentlyReset,
            _ => ModeState::NotRecognized,
        }
    }
    pub fn supported(self) -> bool {
        self == ModeState::Supported
    }
}

/// What the terminal answered. `None` = no answer (unsupported, or swallowed by a multiplexer).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Probe {
    /// Background color from OSC 11.
    pub background: Option<(u8, u8, u8)>,
    /// Kitty keyboard protocol flags (`Some` = supported).
    pub kitty_keyboard: Option<u32>,
    pub sync_output: Option<ModeState>,
    pub sgr_mouse: Option<ModeState>,
    pub focus_events: Option<ModeState>,
    /// DECRQSS answer to the 24-bit test color: `Some(true)` = kept exactly (truecolor), `Some(false)` =
    /// downsampled or refused.
    pub truecolor: Option<bool>,
    /// DA1 answered (the probe completed; missing answers above are real "no"s).
    pub da1: bool,
    /// Raw DA1 attributes, e.g. "62;22".
    pub da1_attrs: Option<String>,
    /// Whether the DECRQM / kitty / DECRQSS questions were asked ([`query_for`]); when not, their missing
    /// answers mean "unknown", not "no".
    pub modes_queried: bool,
}

impl Probe {
    /// `"#rrggbb"` of the background, if known.
    pub fn background_hex(&self) -> Option<String> {
        self.background.map(|(r, g, b)| format!("#{r:02x}{g:02x}{b:02x}"))
    }
}

fn scale_hex(component: &str) -> Option<u8> {
    if component.is_empty() || component.len() > 4 || !component.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(component, 16).ok()?;
    let max = (1u32 << (4 * component.len() as u32)) - 1;
    Some(((v * 255 + max / 2) / max) as u8)
}

/// Parses `rgb:RRRR/GGGG/BBBB` (1–4 hex digits per component) or `#rrggbb`.
pub fn parse_osc_color(s: &str) -> Option<(u8, u8, u8)> {
    if let Some(rest) = s.strip_prefix("rgb:").or_else(|| s.strip_prefix("rgba:")) {
        let mut it = rest.split('/');
        let r = scale_hex(it.next()?)?;
        let g = scale_hex(it.next()?)?;
        let b = scale_hex(it.next()?)?;
        return Some((r, g, b));
    }
    let hex = s.strip_prefix('#')?;
    if hex.len() == 6 {
        let p = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        return Some((p(0)?, p(2)?, p(4)?));
    }
    None
}

/// DECRQSS reply body (between `ESC P` and ST) for the SGR query: `1$r<sgr>m` (valid) or `0$r` (refused).
/// Accepts both `38;2;1;2;3` and the colon forms `38:2:1:2:3` / `38:2::1:2:3` (xterm's empty color space).
pub fn truecolor_from_decrqss(body: &str) -> Option<bool> {
    if body.starts_with("0$r") {
        return Some(false);
    }
    let sgr = body.strip_prefix("1$r")?;
    let sgr = sgr.strip_suffix('m').unwrap_or(sgr);
    let toks: Vec<&str> = sgr.split([';', ':']).collect();
    for i in 0..toks.len() {
        if toks[i] == "38" && toks.get(i + 1) == Some(&"2") {
            let rgb: Vec<&str> = toks[i + 2..]
                .iter()
                .copied()
                .filter(|t| !t.is_empty())
                .take(3)
                .collect();
            return Some(rgb == ["1", "2", "3"]);
        }
    }
    Some(false)
}

/// Parses everything the terminal wrote back (in any order, possibly interleaved with nothing else).
pub fn parse_replies(buf: &[u8]) -> Probe {
    // Replies to the full [`QUERY`]; `probe_tty` overrides this for [`QUERY_MINIMAL`].
    let mut p = Probe {
        modes_queried: true,
        ..Default::default()
    };
    let mut i = 0;
    while i < buf.len() {
        if buf[i] != 0x1b || i + 1 >= buf.len() {
            i += 1;
            continue;
        }
        match buf[i + 1] {
            b']' => {
                // OSC ... terminated by BEL or ESC \
                let start = i + 2;
                let mut j = start;
                let mut end = None;
                while j < buf.len() {
                    if buf[j] == 0x07 {
                        end = Some((j, j + 1));
                        break;
                    }
                    if buf[j] == 0x1b && j + 1 < buf.len() && buf[j + 1] == b'\\' {
                        end = Some((j, j + 2));
                        break;
                    }
                    j += 1;
                }
                let Some((body_end, next)) = end else { break };
                let body = String::from_utf8_lossy(&buf[start..body_end]);
                if let Some(color) = body.strip_prefix("11;") {
                    p.background = parse_osc_color(color.trim());
                }
                i = next;
            }
            b'P' => {
                // DCS ... ST (only DECRQSS answers are expected)
                let start = i + 2;
                let Some(off) = buf[start..].windows(2).position(|w| w == b"\x1b\\") else {
                    break;
                };
                let body = String::from_utf8_lossy(&buf[start..start + off]);
                if body.contains("$r") {
                    p.truecolor = truecolor_from_decrqss(&body);
                }
                i = start + off + 2;
            }
            b'[' => {
                // CSI params final
                let start = i + 2;
                let mut j = start;
                while j < buf.len() && !(0x40..=0x7e).contains(&buf[j]) {
                    j += 1;
                }
                if j >= buf.len() {
                    break;
                }
                let params = String::from_utf8_lossy(&buf[start..j]).into_owned();
                let fin = buf[j];
                if let Some(q) = params.strip_prefix('?') {
                    match fin {
                        b'u' => p.kitty_keyboard = q.parse().ok().or(Some(0)),
                        b'c' => {
                            p.da1 = true;
                            p.da1_attrs = Some(q.to_string());
                        }
                        b'y' => {
                            // DECRPM: ?<mode>;<state>$y
                            if let Some(body) = q.strip_suffix('$') {
                                if let Some((mode, state)) = body.split_once(';') {
                                    let st = ModeState::from_code(state.parse().unwrap_or(0));
                                    match mode {
                                        "2026" => p.sync_output = Some(st),
                                        "1006" => p.sgr_mouse = Some(st),
                                        "1004" => p.focus_events = Some(st),
                                        _ => {}
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                i = j + 1;
            }
            _ => i += 1,
        }
    }
    p
}

/// True when both stdin and stdout are terminals (the only case in which we query).
pub fn interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Terminals that must not be queried: `dumb` answers nothing, and the Linux VT console (`linux`) parses
/// `ESC P` / `ESC ]` as palette commands.
pub fn skip_probe_for(term: &str) -> bool {
    term == "dumb" || term == "linux" || term.is_empty()
}

/// Queries the controlling terminal. `None` when not interactive, `TERM` is `dumb`/`linux`/unset, or
/// `/dev/tty` can't be used.
#[cfg(unix)]
pub fn probe() -> Option<Probe> {
    if !interactive() || skip_probe_for(&std::env::var("TERM").unwrap_or_default()) {
        return None;
    }
    probe_tty(TIMEOUT, &std::env::var("TERM_PROGRAM").unwrap_or_default())
}

#[cfg(not(unix))]
pub fn probe() -> Option<Probe> {
    None
}

/// Waits until `fd` is readable or `timeout` passes. Uses `select(2)`: on macOS `poll(2)` on `/dev/tty`
/// returns `POLLNVAL` at once (tty devices don't support poll there), which made the probe give up before
/// the terminal answered — the answers then arrived in cooked mode, were echoed, and read as keystrokes.
#[cfg(unix)]
pub(crate) fn wait_readable(fd: i32, timeout: Duration) -> bool {
    if fd < 0 || fd as usize >= libc::FD_SETSIZE {
        return false;
    }
    // SAFETY: fd_set is plain data; FD_ZERO/FD_SET write inside it for fd < FD_SETSIZE.
    let mut set: libc::fd_set = unsafe { std::mem::zeroed() };
    unsafe {
        libc::FD_ZERO(&mut set);
        libc::FD_SET(fd, &mut set);
    }
    let mut tv = libc::timeval {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_usec: timeout.subsec_micros() as libc::suseconds_t,
    };
    loop {
        // SAFETY: valid fd_set and timeval; the other sets are null.
        let n = unsafe {
            libc::select(
                fd + 1,
                &mut set,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut tv,
            )
        };
        if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        // SAFETY: `set` was filled by select.
        return n > 0 && unsafe { libc::FD_ISSET(fd, &set) };
    }
}

#[cfg(unix)]
fn probe_tty(timeout: Duration, term_program: &str) -> Option<Probe> {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::Instant;

    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    let fd = tty.as_raw_fd();
    // SAFETY: termios is plain data; tcgetattr fills it for a valid fd.
    let mut orig: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut orig) } != 0 {
        return None;
    }
    let mut raw = orig;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO);
    raw.c_cc[libc::VMIN] = 0;
    raw.c_cc[libc::VTIME] = 0;
    // SAFETY: valid fd and termios.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
        return None;
    }
    struct Restore(i32, libc::termios);
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: restoring the attributes read above on the same fd.
            unsafe {
                libc::tcsetattr(self.0, libc::TCSANOW, &self.1);
            }
        }
    }
    let _restore = Restore(fd, orig);

    let (query, modes_queried) = query_for(term_program);
    if tty.write_all(query).and_then(|_| tty.flush()).is_err() {
        return None;
    }
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::with_capacity(256);
    let mut chunk = [0u8; 256];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        if !wait_readable(fd, left) {
            break;
        }
        match tty.read(&mut chunk) {
            Ok(0) => break,
            Ok(k) => {
                buf.extend_from_slice(&chunk[..k]);
                if parse_replies(&buf).da1 || buf.len() > 4096 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let mut p = parse_replies(&buf);
    p.modes_queried = modes_queried;
    let _ = tty.write_all(WIPE_LINE).and_then(|_| tty.flush());
    if !p.da1 {
        // Timed out: drop whatever part of the answers already arrived so it isn't read as keystrokes.
        // SAFETY: valid fd; TCIFLUSH only discards unread input.
        unsafe {
            libc::tcflush(fd, libc::TCIFLUSH);
        }
    }
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Apple Terminal prints DECRQM finals and DECRQSS bodies ("ppp$qm"); it is only asked OSC 11 + DA1.
    #[test]
    fn apple_terminal_gets_only_queries_it_parses() {
        let (q, modes) = query_for("Apple_Terminal");
        assert!(!modes);
        let q = String::from_utf8_lossy(q);
        assert!(!q.contains("$p"), "no DECRQM: {q:?}");
        assert!(!q.contains("\x1bP"), "no DCS/DECRQSS: {q:?}");
        assert!(!q.contains("[?u"), "no kitty query: {q:?}");
        assert!(
            q.starts_with("\x1b]11;?") && q.ends_with("\x1b[c"),
            "OSC 11 then the DA1 sentinel"
        );
        let (full, modes) = query_for("ghostty");
        assert!(modes);
        assert_eq!(full, QUERY);
        assert_eq!(WIPE_LINE, b"\r\x1b[2K");
    }

    #[test]
    fn parses_a_full_ghostty_style_answer() {
        let reply =
            b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\\x1b[?1u\x1b[?2026;2$y\x1b[?1006;2$y\x1b[?1004;2$y\x1b[?62;22c";
        let p = parse_replies(reply);
        assert_eq!(p.background, Some((0x1e, 0x1e, 0x2e)));
        assert_eq!(p.background_hex().as_deref(), Some("#1e1e2e"));
        assert_eq!(p.kitty_keyboard, Some(1));
        assert_eq!(p.sync_output, Some(ModeState::Supported));
        assert_eq!(p.sgr_mouse, Some(ModeState::Supported));
        assert_eq!(p.focus_events, Some(ModeState::Supported));
        assert!(p.da1);
        assert_eq!(p.da1_attrs.as_deref(), Some("62;22"));
    }

    #[test]
    fn terminal_app_style_minimal_answer() {
        // Only DA1 and a BEL-terminated OSC 11 with 2-digit components; no Kitty, DECRQM unknown.
        let reply = b"\x1b]11;rgb:ff/ff/ff\x07\x1b[?2026;0$y\x1b[?1;2c";
        let p = parse_replies(reply);
        assert_eq!(p.background, Some((255, 255, 255)));
        assert_eq!(p.kitty_keyboard, None);
        assert_eq!(p.sync_output, Some(ModeState::NotRecognized));
        assert!(!p.sync_output.unwrap().supported());
        assert_eq!(p.sgr_mouse, None);
        assert!(p.da1);
    }

    #[test]
    fn garbage_and_truncation_never_panic() {
        for b in [
            &b""[..],
            b"\x1b",
            b"\x1b]11;rgb:zz/zz/zz\x07",
            b"\x1b]11;rgb:1234",
            b"\x1b[?2026;",
            b"\x1b[?u",
            b"hello\x1b[?1;2c",
        ] {
            let _ = parse_replies(b);
        }
        assert_eq!(parse_replies(b"\x1b[?u").kitty_keyboard, Some(0));
        assert_eq!(parse_replies(b"\x1b]11;rgb:zz/zz/zz\x07").background, None);
    }

    #[test]
    fn truecolor_via_decrqss() {
        // kitty / WezTerm / Ghostty style (colon form), xterm style (empty color space), semicolon form.
        for body in ["1$r38:2:1:2:3m", "1$r0;38:2::1:2:3m", "1$r38;2;1;2;3m"] {
            assert_eq!(truecolor_from_decrqss(body), Some(true), "{body}");
        }
        // Downsampled to the 256 palette, refused, or unrelated.
        assert_eq!(truecolor_from_decrqss("1$r38;5;16m"), Some(false));
        assert_eq!(truecolor_from_decrqss("1$r38:2::0:0:0m"), Some(false));
        assert_eq!(truecolor_from_decrqss("0$r"), Some(false));
        assert_eq!(truecolor_from_decrqss("1$r0m"), Some(false));
        assert_eq!(truecolor_from_decrqss("garbage"), None);
        let p = parse_replies(b"\x1b[?1u\x1bP1$r0;38:2::1:2:3m\x1b\\\x1b[?62;22c");
        assert_eq!(p.truecolor, Some(true));
        assert_eq!(p.kitty_keyboard, Some(1), "parsing continues after the DCS");
        assert!(p.da1);
        let p = parse_replies(b"\x1bP1$r38;5;16m\x1b\\\x1b[?1;2c");
        assert_eq!(p.truecolor, Some(false));
        // Unterminated DCS never panics.
        assert_eq!(parse_replies(b"\x1bP1$r38:2").truecolor, None);
    }

    #[test]
    fn the_query_resets_the_test_color_and_ends_with_da1() {
        let q = String::from_utf8_lossy(QUERY);
        let set = q.find("\x1b[38;2;1;2;3m").unwrap();
        let ask = q.find("\x1bP$qm\x1b\\").unwrap();
        let reset = q.find("\x1b[0m").unwrap();
        assert!(set < ask && ask < reset, "set → ask → reset");
        assert!(q.ends_with("\x1b[c"), "DA1 sentinel last");
        assert!(skip_probe_for("linux") && skip_probe_for("dumb") && skip_probe_for(""));
        assert!(!skip_probe_for("xterm-ghostty"));
    }

    #[test]
    fn osc_color_scaling() {
        assert_eq!(parse_osc_color("rgb:f/0/8"), Some((255, 0, 136)));
        assert_eq!(parse_osc_color("rgb:ffff/0000/8080"), Some((255, 0, 128)));
        assert_eq!(parse_osc_color("#102030"), Some((16, 32, 48)));
        assert_eq!(parse_osc_color("rgb:12345/0/0"), None);
    }
}
