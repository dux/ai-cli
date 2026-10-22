//! Rebuilds the line you are typing from the raw byte stream on its way to the
//! child, and keeps the last `keep` submitted lines.
//!
//! Reads can split a UTF-8 character or an escape sequence, so bytes are
//! accumulated raw and only decoded when rendered.

use std::sync::LazyLock;

use regex::Regex;
use regex::bytes::Regex as BytesRegex;

use crate::text::{CYAN, DIM, RESET, REV, YELLOW, clip, rule, scrub, strip};

const MAX_LEN: usize = 4096;
const NO_PROMPTS: &str = "(nothing typed yet)";
const LABEL: &str = "prompt history";

const BANG: u8 = 0x21;
const CR: u8 = 0x0d;
const LF: u8 = 0x0a;
const ESC: u8 = 0x1b;
const TAB: u8 = 0x09;
const BS: u8 = 0x08;
const BEL: u8 = 0x07;
const DEL: u8 = 0x7f;
const CTRL_C: u8 = 0x03;
const CTRL_U: u8 = 0x15;
const CTRL_W: u8 = 0x17;

/// Introducers of string sequences (OSC, DCS, SOS, PM, APC): run to BEL or ST.
const STRING_INTRO: [u8; 5] = [0x5d, 0x50, 0x58, 0x5e, 0x5f];

// Modifier bits as both keyboard protocols report them (wire value is +1).
const SHIFT: i64 = 1;
const ALT: i64 = 2;
const CTRL: i64 = 4;
const SUPER: i64 = 8;

/// CSI <code>[:<alternates>][;<mods>[:<event>]][;<text codepoints>] u
static KITTY_KEY: LazyLock<BytesRegex> = LazyLock::new(|| {
    BytesRegex::new(r"\A\x1b\[(\d+)(?::\d+(?::\d+)?)?(?:;(\d+)(?::(\d+))?)?(?:;([\d:]+))?u\z").unwrap()
});
/// CSI 27 ; <mods> ; <code> ~   (xterm modifyOtherKeys)
static XTERM_KEY: LazyLock<BytesRegex> =
    LazyLock::new(|| BytesRegex::new(r"\A\x1b\[27;(\d+);(\d+)~\z").unwrap());
/// CSI < b ; x ; y (M|m)   (SGR mouse report)
static SGR_MOUSE: LazyLock<BytesRegex> =
    LazyLock::new(|| BytesRegex::new(r"\A\x1b\[<\d+;\d+;(\d+)[Mm]\z").unwrap());
/// CSI 21 [; <mods> [: <event>]] ~   (F10, legacy and kitty alike)
static F10_KEY: LazyLock<BytesRegex> =
    LazyLock::new(|| BytesRegex::new(r"\x1b\[21(?:;(\d*)(?::(\d+))?)?~").unwrap());

/// The event of an unmodified F10 in `m` (1 press, 2 repeat, 3 release), or
/// None when it carries a modifier and is the child's to have.
fn f10_event(m: &regex::bytes::Captures) -> Option<i64> {
    let mods: i64 = parse(m.get(1)).unwrap_or(1);
    let bits = (mods - 1).max(0) & (SHIFT | ALT | CTRL | SUPER);
    (bits == 0).then(|| parse(m.get(2)).unwrap_or(1))
}

/// Whether raw input holds an F10 press: for when keys are not being read
/// through a KeyBuffer at all (a password prompt), and F10 must still quit.
pub fn has_f10(data: &[u8]) -> bool {
    F10_KEY.captures_iter(data).any(|m| f10_event(&m).is_some_and(|e| e != 3))
}

static HSPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t\r\x0b\x0c]+").unwrap());
static BREAKS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" ?\n+ ?").unwrap());

pub struct KeyBuffer {
    keep: usize,
    /// Newest first.
    prompts: Vec<String>,
    buf: Vec<u8>,
    esc: Option<Vec<u8>>,
    /// Raw bytes still owed to an X10 mouse report (`ESC [ M` + 3).
    x10: u8,
    paste: bool,
    bang: bool,
    bangs: Vec<String>,
    pass: Vec<u8>,
    hold: Vec<u8>,
    /// The keystroke that ended a ! line is ours, not the child's.
    swallow: bool,
    /// The sequence in `hold` is a mouse report aimed at the bar.
    drop: bool,
    awake: bool,
    child_rows: u16,
    /// F10 was pressed: the wrapper is to quit.
    exit: bool,
}

fn parse<T: std::str::FromStr>(m: Option<regex::bytes::Match>) -> Option<T> {
    std::str::from_utf8(m?.as_bytes()).ok()?.parse().ok()
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

impl KeyBuffer {
    pub fn new(keep: usize) -> Self {
        KeyBuffer {
            keep,
            prompts: Vec::new(),
            buf: Vec::new(),
            esc: None,
            x10: 0,
            paste: false,
            bang: false,
            bangs: Vec::new(),
            pass: Vec::new(),
            hold: Vec::new(),
            swallow: false,
            drop: false,
            awake: false,
            child_rows: u16::MAX,
            exit: false,
        }
    }

    /// Bytes the child may see from the last feed: everything, until a line
    /// starts with !, and then nothing until that line is done with.
    pub fn pass(&self) -> &[u8] {
        &self.pass
    }

    /// ! lines submitted in the last feed, in order. "" is a bare !.
    pub fn take_bangs(&mut self) -> Vec<String> {
        std::mem::take(&mut self.bangs)
    }

    /// True once after F10 was pressed. The key itself never reaches the child.
    pub fn take_exit(&mut self) -> bool {
        std::mem::take(&mut self.exit)
    }

    /// There is a line on the go, so the screen is wanted for it.
    pub fn typing(&self) -> bool {
        !self.buf.is_empty()
    }

    #[cfg(test)]
    pub fn bang(&self) -> bool {
        self.bang
    }

    /// The line being typed right now, for the debug trace.
    pub fn pending(&self) -> String {
        decode(&self.buf)
    }

    pub fn set_awake(&mut self, on: bool) {
        self.awake = on;
    }

    /// Rows the child owns; mouse reports below them point at the bar.
    pub fn set_child_rows(&mut self, rows: u16) {
        self.child_rows = rows;
    }

    /// Returns true when what the bar shows changed.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        self.pass.clear();
        self.bangs.clear();
        let mut changed = false;

        for &b in bytes {
            self.hold.push(b);
            let was = self.bang;
            let hit = if self.esc.is_some() { self.escape(b) } else { self.plain(b) };
            // A ! line redraws on every keystroke: the bar is the only place
            // it is shown.
            if hit || self.bang || was {
                changed = true;
            }
            if self.esc.is_none() {
                self.release();
            }
        }

        // A sequence still arriving goes to the child now, as in Ruby - unless
        // it may be a mouse report, which must stay whole so a bar-aimed one
        // can still be dropped once its tail arrives.
        if !self.must_hold() {
            self.release();
        }
        changed
    }

    /// The whole bar: the labelled rule, then the pinned prompts oldest first
    /// so the newest lands on the last line.
    pub fn lines(&self, cols: usize) -> Vec<String> {
        let mut rows: Vec<String> = (0..self.keep)
            .map(|slot| {
                let age = self.keep - 1 - slot;
                match self.prompts.get(age) {
                    None if age == 0 => format!("{DIM}{}{RESET}", clip(NO_PROMPTS, cols)),
                    None => String::new(),
                    Some(t) if age == 0 => format!("{CYAN}❯{RESET} {}", clip(t, cols.saturating_sub(2))),
                    Some(t) => format!("{DIM}❯ {}{RESET}", clip(t, cols.saturating_sub(2))),
                }
            })
            .collect();

        if self.bang
            && let Some(last) = rows.last_mut()
        {
            *last = self.typing_row(cols);
        }
        let label = if self.awake { format!("{LABEL} · awake") } else { LABEL.to_string() };
        let mut out = vec![rule(cols, &label)];
        out.extend(rows);
        out
    }

    /// The ! line is not shown anywhere else, so this row is it, cursor
    /// included.
    fn typing_row(&self, cols: usize) -> String {
        let text = decode(&self.buf);
        let text = text.strip_prefix('!').unwrap_or(&text).trim_start();
        format!("{YELLOW}!{RESET} {}{REV} {RESET}", clip(text, cols.saturating_sub(4)))
    }

    /// A sequence that may yet turn into something the child must not see -
    /// a mouse report aimed at the bar, or F10 - stays whole across reads.
    fn must_hold(&self) -> bool {
        match &self.esc {
            Some(e) => {
                matches!(e.as_slice(), b"\x1b[" | b"\x1b[2")
                    || e.starts_with(b"\x1b[<")
                    || e.starts_with(b"\x1b[M")
                    || e.starts_with(b"\x1b[21")
            }
            None => false,
        }
    }

    fn release(&mut self) {
        if self.hold.is_empty() {
            return;
        }
        if !(self.bang || self.swallow || self.drop) {
            self.pass.extend_from_slice(&self.hold);
        }
        self.swallow = false;
        self.drop = false;
        self.hold.clear();
    }

    fn plain(&mut self, b: u8) -> bool {
        match b {
            ESC => self.esc = Some(vec![b]),
            CR | LF => {
                if !self.paste {
                    return self.submit();
                }
                self.append("\n");
            }
            DEL | BS => self.chop_char(),
            CTRL_U | CTRL_C => self.clear_line(),
            CTRL_W => self.drop_word(),
            TAB => {} // completion text is inserted by the app
            _ if b >= 0x20 => self.append_byte(b),
            _ => {}
        }
        false
    }

    /// Collect an escape sequence, then decide. Only "newline, don't submit"
    /// forms, the paste markers and keyboard-protocol keys mean anything.
    fn escape(&mut self, b: u8) -> bool {
        // X10 mouse: three raw bytes after ESC [ M, whatever their value.
        if self.x10 > 0 {
            let esc = self.esc.as_mut().unwrap();
            esc.push(b);
            self.x10 -= 1;
            if self.x10 == 0 {
                let y = esc[5].saturating_sub(32);
                self.esc = None;
                if u16::from(y) > self.child_rows {
                    self.drop = true;
                }
            }
            return false;
        }

        let esc = self.esc.as_mut().unwrap();
        esc.push(b);
        let intro = esc[1];
        let len = esc.len();

        // String sequences carry a printable payload (terminal replies such as
        // an OSC colour answer): dropped whole. Before the resync, since an ESC
        // here starts the ST.
        if len > 2 && STRING_INTRO.contains(&intro) {
            if b == BEL || (b == 0x5c && esc[len - 2] == ESC) || len > 1024 {
                self.esc = None;
            }
            return false;
        }

        // An ESC part-way through always starts a new sequence: a bare Escape
        // keypress followed by another key's sequence.
        if b == ESC {
            self.esc = Some(vec![b]);
            return false;
        }

        if len == 2 {
            match b {
                0x5b | 0x4f => return false, // CSI / SS3
                _ if STRING_INTRO.contains(&b) => return false,
                CR | LF => {
                    self.esc = None; // Option+Enter
                    self.append("\n");
                    return false;
                }
                _ => {
                    self.esc = None;
                    return false;
                }
            }
        }

        if intro == 0x4f {
            self.esc = None; // SS3 is always three bytes
            return false;
        }

        if (0x40..=0x7e).contains(&b) {
            if esc.as_slice() == b"\x1b[M" {
                self.x10 = 3;
                return false;
            }
            let seq = self.esc.take().unwrap();
            return self.csi(&seq);
        }

        // Lost the thread: keep swallowing to the final byte.
        if len > 32 {
            esc.truncate(2);
        }
        false
    }

    /// Kitty and modifyOtherKeys report keys as CSI sequences; everything else
    /// (arrows, focus, mouse, device reports) is not typed text.
    fn csi(&mut self, seq: &[u8]) -> bool {
        match seq {
            b"\x1b[200~" => self.paste = true,
            b"\x1b[201~" => self.paste = false,
            _ => {
                if let Some(m) = F10_KEY.captures(seq).filter(|m| m.get(0).unwrap().len() == seq.len())
                    && let Some(event) = f10_event(&m)
                {
                    // Press, repeat and release are all ours.
                    self.drop = true;
                    if event != 3 {
                        self.exit = true;
                    }
                    return false;
                }
                if let Some(m) = KITTY_KEY.captures(seq) {
                    let code = parse(m.get(1)).unwrap_or(0);
                    let mods = parse(m.get(2)).unwrap_or(0);
                    let event = parse(m.get(3)).unwrap_or(0);
                    let text = m.get(4).map(|t| String::from_utf8_lossy(t.as_bytes()).into_owned());
                    return self.key(code, mods, event, text.as_deref());
                }
                if let Some(m) = XTERM_KEY.captures(seq) {
                    let mods = parse(m.get(1)).unwrap_or(0);
                    let code = parse(m.get(2)).unwrap_or(0);
                    return self.key(code, mods, 0, None);
                }
                if let Some(m) = SGR_MOUSE.captures(seq) {
                    let y: u64 = parse(m.get(1)).unwrap_or(0);
                    if y > u64::from(self.child_rows) {
                        self.drop = true;
                    }
                }
            }
        }
        false
    }

    /// One decoded keypress; `mods` is the 1-based bitfield, lock bits masked.
    fn key(&mut self, code: i64, mods: i64, event: i64, text: Option<&str>) -> bool {
        if event == 3 {
            return false; // release, not a press
        }
        let bits = (mods - 1).max(0) & (SHIFT | ALT | CTRL | SUPER);

        match code {
            13 | 10 => {
                if bits == 0 {
                    return self.submit();
                }
                self.append("\n");
                return false;
            }
            127 | 8 => {
                self.chop_char();
                return false;
            }
            9 | 27 => return false,
            _ => {}
        }

        // Ctrl-<key> is a command, never text (Ctrl-V pastes an image).
        if bits & CTRL != 0 {
            match code {
                117 | 99 => self.clear_line(),
                119 => self.drop_word(),
                _ => {}
            }
            return false;
        }
        if bits & (ALT | SUPER) != 0 || code < 0x20 {
            return false;
        }

        // The kitty associated-text field wins; otherwise shift is applied by
        // hand to the unshifted key code.
        match text {
            Some(text) => {
                for cp in text.split(':') {
                    let Some(c) = cp.parse::<u32>().ok().and_then(char::from_u32) else {
                        return false;
                    };
                    self.append(c.encode_utf8(&mut [0; 4]));
                }
            }
            None => {
                let Some(c) = u32::try_from(code).ok().and_then(char::from_u32) else {
                    return false;
                };
                if bits & SHIFT != 0 {
                    let s: String = c.to_uppercase().collect();
                    self.append(&s);
                } else {
                    self.append(c.encode_utf8(&mut [0; 4]));
                }
            }
        }
        false
    }

    fn append_byte(&mut self, b: u8) {
        if b == BANG && !self.bang_char() {
            return;
        }
        if self.buf.len() < MAX_LEN {
            self.buf.push(b);
        }
    }

    fn append(&mut self, s: &str) {
        if s == "!" && !self.bang_char() {
            return;
        }
        if self.buf.len() < MAX_LEN {
            self.buf.extend_from_slice(s.as_bytes());
        }
    }

    /// A ! in the first column is ours; typed twice it is handed back, so the
    /// agent gets one. False when the character must not be appended.
    fn bang_char(&mut self) -> bool {
        if self.bang && self.buf == b"!" {
            self.bang = false;
            return false;
        }
        if self.buf.is_empty() && !self.paste {
            self.bang = true;
        }
        true
    }

    fn clear_line(&mut self) {
        self.end_bang();
        self.buf.clear();
    }

    /// Ending a ! line takes the keystroke that ended it along.
    fn end_bang(&mut self) {
        if !self.bang {
            return;
        }
        self.bang = false;
        self.swallow = true;
    }

    /// Drop one whole character, continuation bytes included.
    fn chop_char(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let mut i = self.buf.len() - 1;
        while i > 0 && self.buf[i] & 0xc0 == 0x80 {
            i -= 1;
        }
        self.buf.truncate(i);
        if self.buf.is_empty() {
            self.end_bang();
        }
    }

    /// Readline's unix-word-rubout: "one two" -> "one ".
    fn drop_word(&mut self) {
        while self.buf.last().is_some_and(|&b| is_space(b)) {
            self.buf.pop();
        }
        while self.buf.last().is_some_and(|&b| !is_space(b)) {
            self.buf.pop();
        }
        if self.buf.is_empty() {
            self.end_bang();
        }
    }

    fn submit(&mut self) -> bool {
        let text = decode(&self.buf);
        let bang = self.bang;
        let raw = std::mem::take(&mut self.buf);
        self.clear_line();

        // A ! line was never said to the agent: not history.
        if bang {
            self.bangs.push(command(&raw));
            return true;
        }
        if text.is_empty() || self.prompts.first() == Some(&text) {
            return false;
        }
        self.prompts.insert(0, text);
        self.prompts.truncate(self.keep);
        true
    }
}

/// The line as a shell should see it: newlines survive.
fn command(bytes: &[u8]) -> String {
    let s = scrub(bytes);
    let s = strip(&s);
    strip(s.strip_prefix('!').unwrap_or(s)).to_string()
}

/// One bar row: whitespace runs collapsed, line breaks shown as `\ `.
fn decode(bytes: &[u8]) -> String {
    let s = scrub(bytes);
    let s = strip(&s);
    if s.is_empty() {
        return String::new();
    }
    let s = HSPACE.replace_all(s, " ");
    BREAKS.replace_all(&s, regex::NoExpand("\\ ")).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::RULE;

    fn kitty(s: &str) -> String {
        s.chars().map(|c| format!("\x1b[{}u", c as u32)).collect()
    }
    fn xterm(s: &str) -> String {
        s.chars().map(|c| format!("\x1b[27;1;{}~", c as u32)).collect()
    }

    fn buffer() -> KeyBuffer {
        KeyBuffer::new(2)
    }

    fn feed(mut kb: KeyBuffer, chunks: &[&[u8]]) -> KeyBuffer {
        for c in chunks {
            kb.feed(c);
        }
        kb
    }

    macro_rules! f {
        ($kb:expr $(, $c:expr)* $(,)?) => { feed($kb, &[$(AsRef::<[u8]>::as_ref(&$c)),*]) };
    }

    fn strip_sgr(s: &str) -> String {
        Regex::new(r"\x1b\[[\d;]*m").unwrap().replace_all(s, "").into_owned()
    }
    fn bar(kb: &KeyBuffer, cols: usize) -> Vec<String> {
        kb.lines(cols).iter().map(|l| strip_sgr(l)).collect()
    }
    fn newest(kb: &KeyBuffer) -> String {
        newest_at(kb, 60)
    }
    fn newest_at(kb: &KeyBuffer, cols: usize) -> String {
        bar(kb, cols).last().unwrap().clone()
    }
    fn older(kb: &KeyBuffer) -> String {
        let b = bar(kb, 60);
        b[b.len() - 2].clone()
    }
    fn rule_of(cols: usize) -> String {
        bar(&buffer(), cols)[0].clone()
    }
    fn typed(text: &str) -> String {
        newest(&f!(buffer(), text, "\r"))
    }

    // -- layout

    #[test]
    fn rule_on_top_prompts_below_newest_last() {
        let kb = f!(buffer(), "first\r", "second\r");
        let b = bar(&kb, 60);
        assert_eq!(b.len(), 3);
        assert_eq!(b[0], format!("{} prompt history {}", RULE.repeat(22), RULE.repeat(22)));
        assert_eq!(older(&kb), "❯ first");
        assert_eq!(newest(&kb), "❯ second");
    }

    #[test]
    fn rule_fills_the_width_exactly() {
        for cols in [20, 33, 60, 120] {
            assert_eq!(rule_of(cols).chars().count(), cols);
        }
        assert!(rule_of(40).contains(" prompt history "));
        assert_eq!(rule_of(10), RULE.repeat(10));
        assert!(!rule_of(40).contains('─'));
    }

    #[test]
    fn awake_shows_in_the_rule() {
        let mut kb = buffer();
        kb.set_awake(true);
        assert!(bar(&kb, 60)[0].contains("prompt history · awake"));
    }

    #[test]
    fn oldest_falls_off_the_ring() {
        let kb = f!(buffer(), "a\r", "b\r", "c\r");
        assert_eq!([older(&kb), newest(&kb)], ["❯ b", "❯ c"]);
    }

    #[test]
    fn placeholder_sits_on_the_last_line() {
        assert_eq!(newest(&buffer()), "(nothing typed yet)");
        assert_eq!(older(&buffer()), "");
    }

    // -- submitting

    #[test]
    fn blank_enter_does_not_submit() {
        assert_eq!(newest(&f!(buffer(), "x\r", "\r", "   \r")), "❯ x");
    }

    #[test]
    fn repeat_of_the_last_prompt_is_not_pinned_twice() {
        let kb = f!(buffer(), "same\r", "same\r");
        assert_eq!([older(&kb), newest(&kb)], ["", "❯ same"]);
    }

    #[test]
    fn feed_reports_only_real_changes() {
        let mut kb = buffer();
        assert!(!kb.feed(b"abc"));
        assert!(kb.feed(b"\r"));
    }

    // -- line editing

    #[test]
    fn backspace() {
        assert_eq!(newest(&f!(buffer(), "helo", "\x7f", "lo\r")), "❯ hello");
        assert_eq!(newest(&f!(buffer(), "ab", "\x08", "c\r")), "❯ ac");
    }

    #[test]
    fn backspace_removes_a_whole_character_not_a_byte() {
        assert_eq!(newest(&f!(buffer(), "naï", "\x7f", "\r")), "❯ na");
        assert_eq!(newest(&f!(buffer(), "ok 🎉", "\x7f", "\r")), "❯ ok");
    }

    #[test]
    fn ctrl_u_and_ctrl_c_clear_the_line() {
        assert_eq!(newest(&f!(buffer(), "junk\x15kept\r")), "❯ kept");
        assert_eq!(newest(&f!(buffer(), "junk\x03kept\r")), "❯ kept");
    }

    #[test]
    fn ctrl_w_drops_a_word_leaving_the_separator() {
        assert_eq!(newest(&f!(buffer(), "one two\x17three\r")), "❯ one three");
    }

    #[test]
    fn tab_is_ignored() {
        assert_eq!(typed("ab\tcd"), "❯ abcd");
    }

    // -- reassembly

    #[test]
    fn escape_sequence_split_across_reads() {
        assert_eq!(newest(&f!(buffer(), "ab", "\x1b", "[", "A", "cd\r")), "❯ abcd");
    }

    #[test]
    fn utf8_character_split_across_reads() {
        let b = "héllo".as_bytes();
        assert_eq!(newest(&f!(buffer(), b[..2], b[2..], "\r")), "❯ héllo");
    }

    #[test]
    fn escape_sequence_after_a_dangling_escape() {
        assert_eq!(newest(&f!(buffer(), "deplo", "\x1b", "\x1b[27;1:3u", "y\r")), "❯ deploy");
        assert_eq!(newest(&f!(buffer(), "deplo", "\x1b\x1b[27;1:3u", "y\r")), "❯ deploy");
        assert_eq!(newest(&f!(buffer(), "ab", "\x1b", "\x1b[127u", "\r")), "❯ a");
    }

    #[test]
    fn runaway_csi_does_not_spill() {
        let seq = format!("\x1b[{}m", "1;".repeat(200));
        assert_eq!(newest(&f!(buffer(), "ab", seq, "cd\r")), "❯ abcd");
    }

    // -- kitty

    #[test]
    fn kitty_plain_typing() {
        assert_eq!(newest(&f!(buffer(), kitty("test 2"), "\x1b[13u")), "❯ test 2");
    }

    #[test]
    fn kitty_shift_and_text_field() {
        assert_eq!(newest(&f!(buffer(), "\x1b[116;2u\x1b[13u")), "❯ T");
        assert_eq!(newest(&f!(buffer(), "\x1b[116;2;84u\x1b[13u")), "❯ T");
    }

    #[test]
    fn kitty_key_release_ignored_but_repeat_kept() {
        assert_eq!(newest(&f!(buffer(), "\x1b[97u\x1b[98;1:3u\x1b[13u")), "❯ a");
        assert_eq!(newest(&f!(buffer(), "\x1b[97u\x1b[98;1:2u\x1b[13u")), "❯ ab");
    }

    #[test]
    fn kitty_backspace_and_unicode() {
        assert_eq!(newest(&f!(buffer(), kitty("abc"), "\x1b[127u", "\x1b[13u")), "❯ ab");
        assert_eq!(newest(&f!(buffer(), "\x1b[269u\x1b[13u")), "❯ č");
    }

    #[test]
    fn kitty_ctrl_alt_tab_are_not_text() {
        assert_eq!(newest(&f!(buffer(), kitty("ab"), "\x1b[118;5u", "\x1b[13u")), "❯ ab");
        assert_eq!(newest(&f!(buffer(), kitty("ab"), "\x1b[99;3u", "\x1b[13u")), "❯ ab");
        assert_eq!(newest(&f!(buffer(), kitty("ab"), "\x1b[9u", "\x1b[13u")), "❯ ab");
    }

    #[test]
    fn kitty_ctrl_u_and_ctrl_w_still_edit() {
        assert_eq!(newest(&f!(buffer(), kitty("junk"), "\x1b[117;5u", kitty("kept"), "\x1b[13u")), "❯ kept");
        assert_eq!(
            newest(&f!(buffer(), kitty("one two"), "\x1b[119;5u", kitty("three"), "\x1b[13u")),
            "❯ one three"
        );
    }

    #[test]
    fn kitty_enter_submits_only_unmodified() {
        assert_eq!(newest(&f!(buffer(), kitty("done"), "\x1b[13u")), "❯ done");
        assert_eq!(newest(&f!(buffer(), "a", "\x1b[13;2u", "b", "\r")), "❯ a\\ b");
        assert_eq!(newest(&f!(buffer(), "a", "\x1b[13;5u", "b", "\r")), "❯ a\\ b");
    }

    // -- xterm

    #[test]
    fn xterm_keys() {
        assert_eq!(newest(&f!(buffer(), xterm("test 3"), "\x1b[27;1;13~")), "❯ test 3");
        assert_eq!(newest(&f!(buffer(), "\x1b[27;2;84~\x1b[13u")), "❯ T");
        assert_eq!(newest(&f!(buffer(), xterm("ab"), "\x1b[27;5;118~", "\x1b[13u")), "❯ ab");
        assert_eq!(newest(&f!(buffer(), "a", "\x1b[27;2;13~", "b", "\r")), "❯ a\\ b");
    }

    // -- the terminal talking back

    const OSC_FG: &str = "\x1b]10;rgb:cdcd/d6d6/f4f4\x1b\\";
    const OSC_BG: &str = "\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\";

    #[test]
    fn string_sequences_are_not_typing() {
        assert_eq!(newest(&f!(buffer(), OSC_FG, OSC_BG, "test 1 - ignore", "\r")), "❯ test 1 - ignore");
        assert_eq!(newest(&f!(buffer(), "\x1b]11;rgb:1e1e/1e1e/2e2e\x07", "ab\r")), "❯ ab");
        assert_eq!(newest(&f!(buffer(), "\x1b]10;rgb:cd", "cd/d6d6/f4f4\x1b\\", "ab\r")), "❯ ab");
        assert_eq!(newest(&f!(buffer(), "te", OSC_BG, "st\r")), "❯ test");
        assert_eq!(newest(&f!(buffer(), "\x1bP>|WezTerm 20240203\x1b\\", "ab\r")), "❯ ab");
        assert_eq!(newest(&f!(buffer(), "\x1b_Gi=1,a=T\x1b\\", "ab\r")), "❯ ab");
    }

    #[test]
    fn unterminated_osc_is_abandoned_not_fatal() {
        let osc = format!("\x1b]10;{}", "x".repeat(1100));
        let kb = f!(buffer(), osc, "junk\r");
        assert!(newest_at(&kb, 200).ends_with("junk"));
        assert_eq!(newest(&f!(kb, "after\r")), "❯ after");
    }

    #[test]
    fn mouse_focus_arrow_and_device_reports_are_not_typing() {
        assert_eq!(newest(&f!(buffer(), "ab", "\x1b[<35;10;20M\x1b[<35;11;21m", "\r")), "❯ ab");
        assert_eq!(newest(&f!(buffer(), "ab", "\x1b[I\x1b[O", "\r")), "❯ ab");
        assert_eq!(newest(&f!(buffer(), "ab", "\x1b[A\x1b[D", "\r")), "❯ ab");
        assert_eq!(newest(&f!(buffer(), "ab", "\x1b[1~\x1b[3~", "\r")), "❯ ab");
        assert_eq!(newest(&f!(buffer(), "ab", "\x1b[?1;2c\x1b[24;80R", "\r")), "❯ ab");
        assert_eq!(newest(&f!(buffer(), "ab", "\x1bOA", "cd\r")), "❯ abcd");
    }

    #[test]
    fn a_whole_sentence_survives_every_encoding() {
        let s = "test 1 - dont do anything";
        let want = format!("❯ {s}");
        assert_eq!(newest(&f!(buffer(), kitty(s), "\x1b[13u")), want);
        assert_eq!(newest(&f!(buffer(), xterm(s), "\x1b[13u")), want);
        assert_eq!(typed(s), want);
    }

    // -- multi-line and paste

    #[test]
    fn paste_is_one_prompt_with_visible_line_breaks() {
        let kb = f!(buffer(), "\x1b[200~one\ntwo\nthree\x1b[201~\r");
        assert_eq!(newest(&kb), "❯ one\\ two\\ three");
        assert_eq!(older(&kb), "");
    }

    #[test]
    fn paste_details() {
        assert_eq!(newest(&f!(buffer(), "\x1b[200~pasted\x1b[201~", " typed\r")), "❯ pasted typed");
        assert_eq!(newest(&f!(buffer(), "\x1b[200~pasted\n\x1b[201~\r")), "❯ pasted");
        assert_eq!(newest(&f!(buffer(), "line one\x1b\rline two\r")), "❯ line one\\ line two");
        assert_eq!(typed("  a   b  "), "❯ a b");
    }

    // -- clipping

    #[test]
    fn clipping() {
        let line = newest_at(&f!(buffer(), "x".repeat(200), "\r"), 30);
        assert_eq!(line.chars().count(), 30);
        assert!(line.ends_with('…'));

        let line = newest_at(&f!(buffer(), "漢".repeat(40), "\r"), 20);
        let cells: usize = line.trim_start_matches("❯ ").chars().map(|c| if c == '…' { 1 } else { 2 }).sum();
        assert!(cells + 2 <= 20);

        assert!(newest_at(&f!(buffer(), "y".repeat(9000), "\r"), 100).chars().count() <= 100);
    }

    // -- ! lines: what the child sees

    struct Run {
        kb: KeyBuffer,
        passed: Vec<u8>,
        bangs: Vec<String>,
    }

    impl Run {
        fn new() -> Run {
            Run { kb: buffer(), passed: Vec::new(), bangs: Vec::new() }
        }
        fn feed(&mut self, chunks: &[&str]) -> &mut Run {
            for c in chunks {
                self.kb.feed(c.as_bytes());
                self.passed.extend_from_slice(self.kb.pass());
                self.bangs = self.kb.take_bangs();
            }
            self
        }
        fn passed(&self) -> String {
            String::from_utf8_lossy(&self.passed).into_owned()
        }
    }

    #[test]
    fn a_bang_line_never_reaches_the_child() {
        let mut r = Run::new();
        r.feed(&["!git status -sb", "\r"]);
        assert_eq!(r.passed(), "", "the Enter is ours too");
        assert_eq!(r.bangs, ["git status -sb"]);
    }

    #[test]
    fn everything_else_still_reaches_the_child_untouched() {
        let mut r = Run::new();
        r.feed(&["hello", "\r"]);
        assert_eq!(r.passed(), "hello\r");
        assert!(r.bangs.is_empty());
    }

    #[test]
    fn bang_only_counts_in_the_first_column() {
        let mut r = Run::new();
        r.feed(&["wow", "!", "\r"]);
        assert_eq!(r.passed(), "wow!\r");
        assert!(r.bangs.is_empty());
        assert_eq!(newest(&r.kb), "❯ wow!");
    }

    #[test]
    fn a_second_bang_hands_the_line_back_to_the_agent() {
        let mut r = Run::new();
        r.feed(&["!!ls"]);
        assert_eq!(r.passed(), "!ls");
        r.feed(&["\r"]);
        assert!(r.bangs.is_empty());
        assert_eq!(newest(&r.kb), "❯ !ls");
    }

    #[test]
    fn a_leading_space_is_the_other_way_out() {
        let mut r = Run::new();
        r.feed(&[" !ls", "\r"]);
        assert_eq!(r.passed(), " !ls\r");
        assert!(r.bangs.is_empty());
    }

    #[test]
    fn a_bare_bang_is_a_command_to_close_the_output() {
        let mut r = Run::new();
        r.feed(&["!", "\r"]);
        assert_eq!(r.bangs, [""]);
    }

    #[test]
    fn bang_is_recognised_under_the_kitty_protocol() {
        let mut r = Run::new();
        r.feed(&["\x1b[33u", &kitty("ls"), "\x1b[13u"]);
        assert_eq!(r.passed(), "");
        assert_eq!(r.bangs, ["ls"]);
    }

    #[test]
    fn a_pasted_bang_is_not_one() {
        let mut r = Run::new();
        r.feed(&["\x1b[200~!ls\x1b[201~", "\r"]);
        assert!(r.bangs.is_empty());
        assert_eq!(newest(&r.kb), "❯ !ls");
        assert!(r.passed().contains("!ls"));
    }

    #[test]
    fn backspacing_the_bang_away_gives_the_keyboard_back() {
        let mut r = Run::new();
        r.feed(&["!ab", "\x7f\x7f\x7f"]);
        assert_eq!(r.passed(), "");
        r.feed(&["x"]);
        assert_eq!(r.passed(), "x");
    }

    #[test]
    fn ctrl_c_cancels_a_bang_line_without_reaching_the_child() {
        let mut r = Run::new();
        r.feed(&["!rm -rf /", "\x03"]);
        assert_eq!(r.passed(), "");
        assert!(r.bangs.is_empty());
        assert!(!r.kb.bang());
    }

    #[test]
    fn a_bang_line_is_not_a_prompt() {
        let mut r = Run::new();
        r.feed(&["!ls\r"]);
        assert_eq!(newest(&r.kb), "(nothing typed yet)");
    }

    #[test]
    fn the_line_being_typed_is_shown() {
        let mut r = Run::new();
        r.feed(&["first", "\r", "!git st"]);
        assert!(r.kb.bang());
        assert_eq!(newest(&r.kb), "! git st ");
        assert!(r.kb.feed(b"a"), "every keystroke redraws it");
    }

    #[test]
    fn the_newest_prompt_comes_back_when_the_line_is_dropped() {
        let mut r = Run::new();
        r.feed(&["first\r", "!ls"]);
        assert_eq!(newest(&r.kb), "! ls ");
        assert!(r.kb.feed(b"\x15"));
        assert_eq!(newest(&r.kb), "❯ first");
    }

    #[test]
    fn typing_reports_a_line_on_the_go() {
        let mut r = Run::new();
        r.feed(&["!ls\r"]);
        assert!(!r.kb.typing());
        r.feed(&["w"]);
        assert!(r.kb.typing());
        r.feed(&["\x7f"]);
        assert!(!r.kb.typing());
        r.feed(&["!"]);
        assert!(r.kb.typing());
    }

    #[test]
    fn inner_spacing_survives_into_the_command() {
        let mut r = Run::new();
        r.feed(&["!ls   -la\r"]);
        assert_eq!(r.bangs, ["ls   -la"]);
    }

    // -- mouse reports and the bar (new)

    fn rows24() -> Run {
        let mut r = Run::new();
        r.kb.set_child_rows(20);
        r
    }

    #[test]
    fn sgr_mouse_on_the_bar_is_dropped() {
        let mut r = rows24();
        r.feed(&["a\x1b[<0;10;21Mb"]);
        assert_eq!(r.passed(), "ab");
        r.feed(&["\x1b[<0;10;23m"]);
        assert_eq!(r.passed(), "ab");
    }

    #[test]
    fn sgr_mouse_on_the_child_passes() {
        let mut r = rows24();
        r.feed(&["\x1b[<0;10;20M"]);
        assert_eq!(r.passed(), "\x1b[<0;10;20M");
    }

    #[test]
    fn sgr_mouse_split_across_reads_is_still_dropped() {
        let mut r = rows24();
        // A lone ESC must reach the child at once (the Escape key), so the
        // hold starts from `ESC [`.
        r.feed(&["\x1b[", "<0;10;", "22M", "x"]);
        assert_eq!(r.passed(), "x");
    }

    #[test]
    fn no_filtering_by_default() {
        let mut r = Run::new();
        r.feed(&["\x1b[<0;10;900M"]);
        assert_eq!(r.passed(), "\x1b[<0;10;900M");
    }

    #[test]
    fn x10_mouse_bytes_are_not_typing() {
        let mut r = Run::new();
        // b=0+32, x=10+32 '*', y=5+32 '%'
        r.feed(&["ab", "\x1b[M *%", "\r"]);
        assert_eq!(newest(&r.kb), "❯ ab");
        assert_eq!(r.passed(), "ab\x1b[M *%\r");
    }

    #[test]
    fn x10_mouse_on_the_bar_is_dropped_on_the_child_passes() {
        let mut r = rows24();
        let bar = [0x1b, b'[', b'M', 32, 42, 32 + 21];
        let child = [0x1b, b'[', b'M', 32, 42, 32 + 20];
        r.kb.feed(&bar);
        assert!(r.kb.pass().is_empty());
        r.kb.feed(&bar[..4]);
        assert!(r.kb.pass().is_empty());
        r.kb.feed(&bar[4..]);
        assert!(r.kb.pass().is_empty());
        r.kb.feed(&child);
        assert_eq!(r.kb.pass(), child);
        r.kb.feed(b"z\r");
        assert_eq!(newest(&r.kb), "❯ z");
    }

    #[test]
    fn f10_quits_and_never_reaches_the_child() {
        for press in ["\x1b[21~", "\x1b[21;1~", "\x1b[21;1:1~", "\x1b[21;129~"] {
            let mut r = Run::new();
            r.feed(&["ab", press, "c"]);
            assert!(r.kb.take_exit(), "{press:?}");
            assert!(!r.kb.take_exit(), "only once");
            assert_eq!(r.passed(), "abc", "{press:?}");
        }
    }

    #[test]
    fn f10_release_is_swallowed_without_quitting() {
        let mut r = Run::new();
        r.feed(&["\x1b[21;1:3~"]);
        assert!(!r.kb.take_exit());
        assert_eq!(r.passed(), "");
    }

    #[test]
    fn modified_f10_and_other_function_keys_belong_to_the_child() {
        for key in ["\x1b[21;2~", "\x1b[21;5~", "\x1b[20~", "\x1b[23~", "\x1b[2~"] {
            let mut r = Run::new();
            r.feed(&[key]);
            assert!(!r.kb.take_exit(), "{key:?}");
            assert_eq!(r.passed(), key);
        }
    }

    #[test]
    fn f10_split_across_reads_still_quits_and_leaks_nothing() {
        let mut r = Run::new();
        r.feed(&["\x1b[2", "1", "~x"]);
        assert!(r.kb.take_exit());
        assert_eq!(r.passed(), "x");
    }

    #[test]
    fn raw_input_scan_finds_only_an_unmodified_press() {
        assert!(has_f10(b"pass\x1b[21~word"));
        assert!(has_f10(b"\x1b[21;1:2~"));
        assert!(!has_f10(b"\x1b[21;1:3~"));
        assert!(!has_f10(b"\x1b[21;2~"));
        assert!(!has_f10(b"\x1b[2~1~"));
    }
}
