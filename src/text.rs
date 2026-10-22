//! Drawing shared by everything that puts a row in the bar: the prompt history
//! and the ! output pane.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const CYAN: &str = "\x1b[36m";
pub const YELLOW: &str = "\x1b[33m";
pub const DIM: &str = "\x1b[2m";
pub const GRAY: &str = "\x1b[38;5;245m";
pub const REV: &str = "\x1b[7m";
pub const RESET: &str = "\x1b[0m";

/// Dashed, and deliberately not the solid U+2500 "─": a pane watcher (Herdr)
/// hangs its prompt-box and dialog regions off the last solid rule on screen,
/// and the bar sits below everything.
pub const RULE: &str = "┄";

pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Cut to `cells` columns, ending in '…' when anything was cut.
pub fn clip(s: &str, cells: usize) -> String {
    if cells == 0 {
        return String::new();
    }
    if width(s) <= cells {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = char_width(c);
        if used + w > cells - 1 {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// A full-width rule with the label centred in it.
pub fn rule(cols: usize, label: &str) -> String {
    if cols == 0 {
        return String::new();
    }
    let label = format!(" {label} ");
    let lw = width(&label);
    if lw + 2 > cols {
        return format!("{GRAY}{}{RESET}", RULE.repeat(cols));
    }
    let pad = cols - lw;
    let left = pad / 2;
    format!("{GRAY}{}{label}{}{RESET}", RULE.repeat(left), RULE.repeat(pad - left))
}

pub fn dim(s: &str) -> String {
    format!("{DIM}{s}{RESET}")
}

/// Ruby's `scrub('')`: invalid UTF-8 is dropped, not replaced.
pub fn scrub(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
    }
    out
}

/// Ruby's `String#strip`: ASCII whitespace and NUL.
pub fn strip(s: &str) -> &str {
    s.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r' | '\0'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_marks_a_cut() {
        assert_eq!(clip("hello", 5), "hello");
        assert_eq!(clip("hello", 4), "hel…");
        assert_eq!(clip("hello", 0), "");
        assert_eq!(clip("漢漢漢", 4), "漢…");
    }

    #[test]
    fn rule_centres_the_label() {
        let r = rule(20, "x");
        assert_eq!(r, format!("{GRAY}{} x {}{RESET}", RULE.repeat(8), RULE.repeat(9)));
        assert_eq!(rule(3, "x"), format!("{GRAY}{}{RESET}", RULE.repeat(3)));
    }
}
