//! Asks the real terminal, once and before the child starts, the things the
//! child will want to know about it.
//!
//! The child's queries are answered by our emulator, synchronously and in the
//! order they were asked - a program that sends a query followed by a DA1 as a
//! sentinel (omp and Claude Code both do) must get the reply to the first one
//! before the DA1 reply, and a round trip to the real terminal for one of them
//! would reorder that. So the facts that only the real terminal knows (its
//! colours, whether it speaks the kitty keyboard protocol) are fetched up
//! front and answered from here.

use std::io;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use alacritty_terminal::vte::ansi::Rgb;
use regex::bytes::Regex;

use crate::term;

#[derive(Debug, Default, Clone)]
pub struct Outer {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub kitty: bool,
}

const QUERY: &[u8] = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[?u\x1b[c";
const WAIT: Duration = Duration::from_millis(300);

/// Answers what the terminal said, and whatever else arrived meanwhile -
/// keys typed while we waited, which belong to the child.
pub fn run() -> (Outer, Vec<u8>) {
    let stdin = io::stdin().as_raw_fd();
    term::write_all(io::stdout().as_raw_fd(), QUERY);

    let da1 = Regex::new(r"\x1b\[\?[\d;]*c").unwrap();
    let mut got = Vec::new();
    let till = Instant::now() + WAIT;
    let mut buf = [0u8; 1024];
    while !da1.is_match(&got) {
        let left = till.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd { fd: stdin, events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, left.as_millis().max(1) as i32) } <= 0 {
            continue;
        }
        let n = unsafe { libc::read(stdin, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            break;
        }
        got.extend_from_slice(&buf[..n as usize]);
    }
    parse(&got)
}

fn parse(got: &[u8]) -> (Outer, Vec<u8>) {
    let color = Regex::new(r"\x1b\](1[01]);rgb:([0-9a-fA-F]+)/([0-9a-fA-F]+)/([0-9a-fA-F]+)(?:\x07|\x1b\\)").unwrap();
    let kitty = Regex::new(r"\x1b\[\?\d*u").unwrap();
    let da1 = Regex::new(r"\x1b\[\?[\d;]*c").unwrap();

    let mut outer = Outer::default();
    for c in color.captures_iter(got) {
        let rgb = Rgb { r: channel(&c[2]), g: channel(&c[3]), b: channel(&c[4]) };
        if &c[1] == b"10" {
            outer.fg = Some(rgb);
        } else {
            outer.bg = Some(rgb);
        }
    }
    outer.kitty = kitty.is_match(got);

    let rest = color.replace_all(got, &b""[..]);
    let rest = kitty.replace_all(&rest, &b""[..]);
    let rest = da1.replace_all(&rest, &b""[..]);
    (outer, rest.into_owned())
}

/// `rgb:` channels come as 1-4 hex digits scaled to that many; keep the top byte.
fn channel(hex: &[u8]) -> u8 {
    let s = std::str::from_utf8(hex).unwrap_or("0");
    let v = u32::from_str_radix(s, 16).unwrap_or(0);
    match s.len() {
        1 => (v * 0x11) as u8,
        2 => v as u8,
        3 => (v >> 4) as u8,
        _ => (v >> 8) as u8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_colours_kitty_and_keeps_typed_keys() {
        let got = b"\x1b]10;rgb:cdcd/d6d6/f4f4\x1b\\hi\x1b]11;rgb:1a/1b/26\x07\x1b[?0u\x1b[?62;22c";
        let (o, rest) = parse(got);
        assert_eq!(o.fg, Some(Rgb { r: 0xcd, g: 0xd6, b: 0xf4 }));
        assert_eq!(o.bg, Some(Rgb { r: 0x1a, g: 0x1b, b: 0x26 }));
        assert!(o.kitty);
        assert_eq!(rest, b"hi");
    }

    #[test]
    fn silence_means_unknown() {
        let (o, rest) = parse(b"\x1b[?6c");
        assert!(o.fg.is_none() && o.bg.is_none() && !o.kitty);
        assert!(rest.is_empty());
    }
}
