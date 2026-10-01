//! Drops a terminal's *late* answers to the CLI's capability probe (UX §12.10, "tmux/SSH safe").
//!
//! The probe waits at most 200 ms for its answers so the first frame stays under 300 ms (SPEC §14). On a
//! high-latency SSH link or a slow terminal the answers arrive after that, when the TUI already reads keys.
//! crossterm drops the CSI answers itself (`CSI ? … u`, `CSI ? … c`, and DECRPM `CSI ? … $ y`, which it
//! folds into the following DA1), but it has no parser for OSC and DCS strings: `ESC ] 11 ; rgb:… ESC \`
//! arrives as Alt+`]`, `1`, `1`, `;`, `r`, `g`, `b`, `:`, … — the `:` opens the command palette and every
//! later key goes into it.
//!
//! [`LateReplyFilter`] recognizes those two string shapes at the event level and swallows them:
//! Alt+`]` + digit … (BEL = Ctrl+G, or ST = Alt+`\`) and Alt+`P` + digit … ST. Events are held while a
//! candidate string is open and handed back unchanged when it turns out not to be one (a non-character key,
//! a mouse or resize event, too long, or no terminator within [`HOLD_MAX`]), so a real Alt+`]` typed by the
//! user still reaches the keymap. The filter only looks for new strings while it is armed (the probe timed
//! out, for [`ARM_WINDOW`] after start) and is a no-op otherwise. Pure: time is passed in.

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::time::{Duration, Instant};

/// How long after start a late answer is still expected (a slow SSH round trip is seconds at worst).
pub const ARM_WINDOW: Duration = Duration::from_secs(10);
/// A terminal writes its answer in one burst; a candidate string still open after this is the user typing.
pub const HOLD_MAX: Duration = Duration::from_millis(300);
/// Longest string swallowed (OSC 11 answers are ~30 characters; DECRQSS ~25).
const MAX_LEN: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `ESC ]` … BEL | ST
    Osc,
    /// `ESC P` … ST
    Dcs,
}

#[derive(Debug)]
pub struct LateReplyFilter {
    armed_until: Option<Instant>,
    open: Option<(Kind, Instant)>,
    held: Vec<Event>,
    /// Strings swallowed so far (for tests and `--debug` style reporting).
    pub dropped: usize,
}

fn is_press(k: &KeyEvent) -> bool {
    k.kind != KeyEventKind::Release
}

/// A plain printable character (Shift allowed: crossterm reports `A`–`F` in hex digits as Shift+char).
fn plain_char(k: &KeyEvent) -> Option<char> {
    match k.code {
        KeyCode::Char(c) if (k.modifiers - KeyModifiers::SHIFT).is_empty() => Some(c),
        _ => None,
    }
}

fn alt_char(k: &KeyEvent) -> Option<char> {
    match k.code {
        KeyCode::Char(c) if k.modifiers.contains(KeyModifiers::ALT) => Some(c),
        _ => None,
    }
}

impl LateReplyFilter {
    /// Armed until `now + window` (`None`: the probe completed or never ran — nothing late can arrive).
    pub fn new(window: Option<Duration>, now: Instant) -> Self {
        LateReplyFilter {
            armed_until: window.map(|w| now + w),
            open: None,
            held: Vec::new(),
            dropped: 0,
        }
    }

    /// A filter that never drops anything.
    pub fn off() -> Self {
        LateReplyFilter {
            armed_until: None,
            open: None,
            held: Vec::new(),
            dropped: 0,
        }
    }

    fn armed(&self, now: Instant) -> bool {
        self.armed_until.is_some_and(|t| now < t)
    }

    /// True while events are held (the caller should poll again soon and call [`Self::expire`]).
    pub fn holding(&self) -> bool {
        self.open.is_some()
    }

    fn release(&mut self, extra: Option<Event>) -> Vec<Event> {
        self.open = None;
        let mut out = std::mem::take(&mut self.held);
        out.extend(extra);
        out
    }

    /// Feeds one event; returns the events to handle now (possibly none, possibly several held ones).
    pub fn push(&mut self, ev: Event, now: Instant) -> Vec<Event> {
        let Some((kind, since)) = self.open else {
            if self.armed(now) {
                if let Event::Key(k) = &ev {
                    if is_press(k) {
                        let kind = match alt_char(k) {
                            Some(']') => Some(Kind::Osc),
                            Some('P') => Some(Kind::Dcs),
                            _ => None,
                        };
                        if let Some(kind) = kind {
                            self.open = Some((kind, now));
                            self.held.push(ev);
                            return Vec::new();
                        }
                    }
                }
            }
            return vec![ev];
        };
        if now.duration_since(since) > HOLD_MAX || self.held.len() >= MAX_LEN {
            // Not a terminal's burst: hand everything back, and look at this event afresh.
            let mut out = self.release(None);
            out.extend(self.push(ev, now));
            return out;
        }
        let Event::Key(k) = &ev else {
            return self.release(Some(ev));
        };
        if !is_press(k) {
            self.held.push(ev);
            return Vec::new();
        }
        let first = self.held.len() == 1;
        // Terminators: ST (`ESC \` = Alt+`\`) for both; BEL (Ctrl+G) for OSC.
        let st = alt_char(k) == Some('\\');
        let bel = k.code == KeyCode::Char('g') && k.modifiers == KeyModifiers::CONTROL;
        if !first && (st || (bel && kind == Kind::Osc)) {
            self.open = None;
            self.held.clear();
            self.dropped += 1;
            return Vec::new();
        }
        match plain_char(k) {
            // Both answers start with a number: `11;rgb:…` and `1$r…` / `0$r`.
            Some(c) if first && !c.is_ascii_digit() => self.release(Some(ev)),
            Some(_) => {
                self.held.push(ev);
                Vec::new()
            }
            None => self.release(Some(ev)),
        }
    }

    /// Called when no event arrived: hands back a held candidate that stayed open past [`HOLD_MAX`].
    pub fn expire(&mut self, now: Instant) -> Vec<Event> {
        match self.open {
            Some((_, since)) if now.duration_since(since) > HOLD_MAX => self.release(None),
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Turns raw bytes into the events crossterm 0.29 produces for them outside CSI/SS3 sequences:
    /// `ESC x` = Alt+x, 0x01–0x1a = Ctrl+letter, uppercase = Shift.
    fn events(bytes: &[u8]) -> Vec<Event> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let (b, alt) = if bytes[i] == 0x1b && i + 1 < bytes.len() {
                i += 1;
                (bytes[i], true)
            } else {
                (bytes[i], false)
            };
            i += 1;
            let (code, mut m) = match b {
                0x01..=0x1a => (KeyCode::Char((b - 1 + b'a') as char), KeyModifiers::CONTROL),
                c if (c as char).is_ascii_uppercase() => (KeyCode::Char(c as char), KeyModifiers::SHIFT),
                c => (KeyCode::Char(c as char), KeyModifiers::NONE),
            };
            if alt {
                m |= KeyModifiers::ALT;
            }
            out.push(Event::Key(KeyEvent::new(code, m)));
        }
        out
    }

    fn run(f: &mut LateReplyFilter, evs: Vec<Event>, now: Instant) -> Vec<Event> {
        evs.into_iter().flat_map(|e| f.push(e, now)).collect()
    }

    #[test]
    fn osc_and_dcs_answers_are_swallowed_while_armed() {
        let t = Instant::now();
        let mut f = LateReplyFilter::new(Some(ARM_WINDOW), t);
        let osc_st = events(b"\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\");
        let osc_bel = events(b"\x1b]11;rgb:FFFF/ffff/ffff\x07");
        let dcs = events(b"\x1bP1$r38:2:1:2:3m\x1b\\");
        let refused = events(b"\x1bP0$r\x1b\\");
        for evs in [osc_st, osc_bel, dcs, refused] {
            assert!(run(&mut f, evs, t).is_empty());
        }
        assert_eq!(f.dropped, 4);
        // A key after the answers is delivered as usual.
        let q = events(b"q");
        assert_eq!(run(&mut f, q.clone(), t), q);
    }

    #[test]
    fn user_keys_are_never_lost() {
        let t = Instant::now();
        let mut f = LateReplyFilter::new(Some(ARM_WINDOW), t);
        // Alt+] then a letter: not an answer → both come back, in order.
        let evs = events(b"\x1b]q");
        assert_eq!(run(&mut f, evs.clone(), t), evs);
        // Alt+P then Enter.
        let alt_p = events(b"\x1bP");
        let enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(run(&mut f, alt_p.clone(), t).is_empty());
        let out = f.push(enter.clone(), t);
        assert_eq!(out, [alt_p[0].clone(), enter]);
        // Alt+] + digit, then nothing for a while: handed back by expire().
        let evs = events(b"\x1b]1");
        assert!(run(&mut f, evs.clone(), t).is_empty());
        assert!(f.holding());
        assert!(f.expire(t + Duration::from_millis(100)).is_empty());
        assert_eq!(f.expire(t + HOLD_MAX + Duration::from_millis(1)), evs);
        assert!(!f.holding());
        // Mouse/resize events interrupt a candidate and pass through.
        let _ = run(&mut f, events(b"\x1b]1"), t);
        let out = f.push(Event::Resize(80, 24), t);
        assert_eq!(out.len(), 3);
        assert_eq!(out[2], Event::Resize(80, 24));
        assert_eq!(f.dropped, 0);
    }

    #[test]
    fn disarmed_filters_pass_everything() {
        let t = Instant::now();
        let answer = events(b"\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\");
        let mut off = LateReplyFilter::off();
        assert_eq!(run(&mut off, answer.clone(), t), answer);
        let mut f = LateReplyFilter::new(Some(ARM_WINDOW), t);
        let later = t + ARM_WINDOW + Duration::from_millis(1);
        assert_eq!(run(&mut f, answer.clone(), later), answer);
        assert!(!f.holding());
    }

    #[test]
    fn a_stale_candidate_is_released_before_the_next_event() {
        let t = Instant::now();
        let mut f = LateReplyFilter::new(Some(ARM_WINDOW), t);
        let start = events(b"\x1b]1");
        assert!(run(&mut f, start.clone(), t).is_empty());
        let later = t + HOLD_MAX + Duration::from_millis(5);
        let q = events(b"q");
        let out = run(&mut f, q.clone(), later);
        assert_eq!(out, [start, q].concat());
    }

    #[test]
    fn overlong_strings_are_handed_back() {
        let t = Instant::now();
        let mut f = LateReplyFilter::new(Some(ARM_WINDOW), t);
        let mut bytes = b"\x1b]1".to_vec();
        bytes.extend(std::iter::repeat_n(b'a', MAX_LEN + 10));
        let evs = events(&bytes);
        let out = run(&mut f, evs.clone(), t);
        assert_eq!(out, evs);
    }
}
