//! A second reading of the child's output, next to the emulator's, for the
//! sequences that are not about the screen at all and must reach the real
//! terminal (or be answered) rather than be emulated:
//!
//!   * keyboard encoding - kitty protocol push/pop/set/query, xterm
//!     modifyOtherKeys. The real terminal does the encoding, so it has to be
//!     told; the emulator never sees a key.
//!   * DECSCUSR cursor shape, forwarded as-is so the user's own default shape
//!     stays put until a program asks for another.
//!   * `CSI 3 J` - the program clearing its scrollback. Ours lives in the real
//!     terminal, so it is cleared there.
//!   * OSC notifications and cwd (7, 9, 99, 777) - the emulator drops them, a
//!     pane watcher wants them.
//!   * synchronized output (mode 2026), so a frame is never painted half done.
//!
//! Nothing that moves the cursor or touches a cell is ever forwarded: that is
//! the whole point of the wrapper.

use alacritty_terminal::vte::{Params, Parser, Perform};
use std::time::{Duration, Instant};

/// How long a synchronized update may hold painting back, same as the
/// emulator's own limit for it.
const SYNC_TIMEOUT: Duration = Duration::from_millis(150);

const FORWARD_OSC: &[&[u8]] = &[b"7", b"9", b"99", b"777"];

pub struct Scan {
    parser: Parser,
    state: State,
}

#[derive(Default)]
struct State {
    kitty_ok: bool,
    /// The child's kitty keyboard stack, as it believes it to be.
    kitty: Vec<u16>,
    kitty_base: u16,
    /// Net pushes forwarded to the real terminal, popped again on exit.
    kitty_pushed: usize,
    kitty_set_base: bool,
    other_keys: bool,
    cursor_shape: bool,
    sync_until: Option<Instant>,
    history_reset: bool,
    outer: Vec<u8>,
    reply: Vec<u8>,
}

impl Scan {
    pub fn new(kitty_ok: bool) -> Scan {
        Scan { parser: Parser::new(), state: State { kitty_ok, ..State::default() } }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.state, bytes);
    }

    /// Bytes for the real terminal, in the order the child sent them.
    pub fn take_outer(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.state.outer)
    }

    /// Answers for the child.
    pub fn take_reply(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.state.reply)
    }

    /// True once per `CSI 3 J` seen since the last call.
    pub fn take_history_reset(&mut self) -> bool {
        std::mem::take(&mut self.state.history_reset)
    }

    /// When an open synchronized update stops holding the paint back.
    pub fn sync_until(&self) -> Option<Instant> {
        self.state.sync_until
    }

    pub fn end_sync(&mut self) {
        self.state.sync_until = None;
    }

    /// Undoes everything forwarded that the real terminal would otherwise
    /// keep after we are gone.
    pub fn restore(&self) -> Vec<u8> {
        let s = &self.state;
        let mut out = Vec::new();
        if s.kitty_pushed > 0 {
            out.extend_from_slice(format!("\x1b[<{}u", s.kitty_pushed).as_bytes());
        }
        if s.kitty_set_base {
            out.extend_from_slice(b"\x1b[=0;1u");
        }
        if s.other_keys {
            out.extend_from_slice(b"\x1b[>4;0m");
        }
        if s.cursor_shape {
            out.extend_from_slice(b"\x1b[0 q");
        }
        out
    }
}

fn first(params: &Params, default: u16) -> u16 {
    params.iter().next().and_then(|p| p.first().copied()).filter(|&v| v != 0).unwrap_or(default)
}

fn nth(params: &Params, n: usize) -> Option<u16> {
    params.iter().nth(n).and_then(|p| p.first().copied())
}

impl State {
    fn kitty_top(&mut self) -> &mut u16 {
        match self.kitty.last_mut() {
            Some(top) => top,
            None => &mut self.kitty_base,
        }
    }

    fn kitty(&mut self, params: &Params, intermediates: &[u8]) {
        match intermediates {
            b">" => {
                let flags = nth(params, 0).unwrap_or(0);
                self.kitty.push(flags);
                if self.kitty_ok {
                    self.kitty_pushed += 1;
                    self.outer.extend_from_slice(format!("\x1b[>{flags}u").as_bytes());
                }
            }
            b"<" => {
                let n = first(params, 1) as usize;
                let n = n.min(self.kitty.len());
                self.kitty.truncate(self.kitty.len() - n);
                if self.kitty_ok && n > 0 {
                    let n = n.min(self.kitty_pushed);
                    if n > 0 {
                        self.kitty_pushed -= n;
                        self.outer.extend_from_slice(format!("\x1b[<{n}u").as_bytes());
                    }
                }
            }
            b"=" => {
                let flags = nth(params, 0).unwrap_or(0);
                let mode = nth(params, 1).filter(|&m| m != 0).unwrap_or(1);
                let top = self.kitty_top();
                *top = match mode {
                    2 => *top | flags,
                    3 => *top & !flags,
                    _ => flags,
                };
                if self.kitty_ok {
                    if self.kitty_pushed == 0 {
                        self.kitty_set_base = true;
                    }
                    self.outer.extend_from_slice(format!("\x1b[={flags};{mode}u").as_bytes());
                }
            }
            b"?" if self.kitty_ok => {
                let top = *self.kitty_top();
                self.reply.extend_from_slice(format!("\x1b[?{top}u").as_bytes());
            }
            _ => {}
        }
    }
}

impl Perform for State {
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        match (action, intermediates) {
            ('u', b">" | b"<" | b"=" | b"?") => self.kitty(params, intermediates),
            ('m', b">") if nth(params, 0) == Some(4) => {
                let level = nth(params, 1).unwrap_or(0);
                self.other_keys = level != 0;
                self.outer.extend_from_slice(format!("\x1b[>4;{level}m").as_bytes());
            }
            ('q', b" ") => {
                self.cursor_shape = true;
                self.outer.extend_from_slice(format!("\x1b[{} q", nth(params, 0).unwrap_or(0)).as_bytes());
            }
            ('J', b"") if nth(params, 0) == Some(3) => {
                self.history_reset = true;
                self.outer.extend_from_slice(b"\x1b[3J");
            }
            ('h' | 'l', b"?") => {
                for p in params.iter() {
                    if p.first() == Some(&2026) {
                        self.sync_until = (action == 'h').then(|| Instant::now() + SYNC_TIMEOUT);
                    }
                }
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        let Some(&num) = params.first() else { return };
        if !FORWARD_OSC.contains(&num) {
            return;
        }
        self.outer.extend_from_slice(b"\x1b]");
        for (i, p) in params.iter().enumerate() {
            if i > 0 {
                self.outer.push(b';');
            }
            self.outer.extend_from_slice(p);
        }
        self.outer.extend_from_slice(if bell_terminated { b"\x07" } else { b"\x1b\\" });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(kitty: bool, bytes: &[u8]) -> Scan {
        let mut s = Scan::new(kitty);
        s.feed(bytes);
        s
    }

    #[test]
    fn kitty_query_is_answered_in_place_when_the_terminal_speaks_it() {
        let mut s = scan(true, b"\x1b[>1u\x1b[?u");
        assert_eq!(s.take_reply(), b"\x1b[?1u");
        assert_eq!(s.take_outer(), b"\x1b[>1u");
        assert_eq!(s.restore(), b"\x1b[<1u");
    }

    #[test]
    fn kitty_stays_unanswered_and_unforwarded_otherwise() {
        let mut s = scan(false, b"\x1b[>1u\x1b[?u");
        assert!(s.take_reply().is_empty());
        assert!(s.take_outer().is_empty());
        assert!(s.restore().is_empty());
    }

    #[test]
    fn pops_never_go_below_what_we_pushed() {
        let mut s = scan(true, b"\x1b[>1u\x1b[<5u");
        assert_eq!(s.take_outer(), b"\x1b[>1u\x1b[<1u");
        assert!(s.restore().is_empty());
    }

    #[test]
    fn screen_sequences_are_never_forwarded() {
        let mut s = scan(true, b"\x1b[2J\x1b[J\x1b[H\x1b[1;5r\x1b[999;1Hx\x1b7\x1b8\x1b[?1049h");
        assert!(s.take_outer().is_empty());
        assert!(!s.take_history_reset());
    }

    #[test]
    fn scrollback_clear_and_notifications_are() {
        let mut s = scan(false, b"\x1b[3J\x1b]9;done\x07\x1b]0;title\x07\x1b]777;notify;a;b\x1b\\");
        assert!(s.take_history_reset());
        assert_eq!(s.take_outer(), b"\x1b[3J\x1b]9;done\x07\x1b]777;notify;a;b\x1b\\");
    }

    #[test]
    fn sync_opens_and_closes() {
        let mut s = scan(false, b"\x1b[?2026h");
        assert!(s.sync_until().is_some());
        s.feed(b"x\x1b[?2026l");
        assert!(s.sync_until().is_none());
    }
}
