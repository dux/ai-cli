//! `~/.config/ai-cli` (or `$XDG_CONFIG_HOME/ai-cli`): the defaults file, the
//! `--update` model cache and the prompt history.

use std::fs::File;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use crate::text::{GRAY, RESET};
use crate::{cli, launch, pane, term};

/// Past this size the history is cut back to its newest HISTORY_KEEP bytes.
const HISTORY_MAX: u64 = 1 << 20;
const HISTORY_KEEP: usize = 1 << 19;

pub fn dir() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME").unwrap_or_default();
    let base = if base.is_empty() {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config")
    } else {
        PathBuf::from(base)
    };
    base.join("ai-cli")
}

fn history_path() -> PathBuf {
    dir().join("history.txt")
}

/// Agent run defaults from `config`; command line switches add to them.
#[derive(Debug, Default, PartialEq)]
pub struct Defaults {
    pub tool: Option<&'static str>,
    pub full: bool,
    pub bare: bool,
    pub awake: bool,
    pub lines: Option<usize>,
}

/// A missing file is all defaults; a bad line is an error naming it.
pub fn defaults() -> Result<Defaults, String> {
    let path = dir().join("config");
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_defaults(&text).map_err(|e| format!("{}:{e}", pane::tilde(&path))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Defaults::default()),
        Err(e) => Err(format!("{}: {e}", pane::tilde(&path))),
    }
}

/// Keeps `tool` in the config file at the agent just started, so the next run
/// without a TOOL gets it. Every other line stays as written.
pub fn remember_tool(tool: &str) {
    let path = dir().join("config");
    let old = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(_) => return,
    };
    let new = with_tool(&old, tool);
    if new == old || std::fs::create_dir_all(dir()).is_err() {
        return;
    }
    let tmp = PathBuf::from(format!("{}.{}.tmp", path.display(), std::process::id()));
    if std::fs::write(&tmp, new).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn with_tool(text: &str, tool: &str) -> String {
    let set = format!("tool = {tool}");
    let mut found = false;
    let mut lines: Vec<&str> = text
        .lines()
        .map(|line| match uncommented(line).split_once('=') {
            Some((key, value)) if key.trim() == "tool" => {
                found = true;
                if value.trim() == tool { line } else { set.as_str() }
            }
            _ => line,
        })
        .collect();
    if !found {
        lines.push(&set);
    }
    lines.join("\n") + "\n"
}

fn uncommented(line: &str) -> &str {
    line.split('#').next().unwrap_or_default().trim()
}

fn parse_defaults(text: &str) -> Result<Defaults, String> {
    let mut d = Defaults::default();
    for (i, line) in text.lines().enumerate() {
        let line = uncommented(line);
        if line.is_empty() {
            continue;
        }
        let err = |msg: String| format!("{}: {msg}", i + 1);
        let Some((key, value)) = line.split_once('=').map(|(k, v)| (k.trim(), v.trim())) else {
            return Err(err(format!("expected key = value, got {line:?}")));
        };
        let flag = || match value {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(err(format!("{key} is true or false, got {value:?}"))),
        };
        match key {
            "tool" => {
                let tool = launch::tool(Some(value)).filter(|_| !value.is_empty());
                let names = launch::names().join(", ");
                d.tool = Some(tool.ok_or_else(|| err(format!("unknown tool {value:?} - one of {names}")))?);
            }
            "full" => d.full = flag()?,
            "bare" => d.bare = flag()?,
            "awake" => d.awake = flag()?,
            "lines" => d.lines = Some(cli::parse_keep(value).map_err(|_| err(format!("lines takes a number, got {value:?}")))?),
            _ => return Err(err(format!("unknown key {key:?}"))),
        }
    }
    Ok(d)
}

/// One line per prompt: `2026-10-22 14:03:11 : ~/dev/proj : the prompt`.
/// A single append, so wrappers running side by side never interleave lines.
pub fn append_history(folder: &str, prompt: &str) {
    let dir = dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let line = format!("{} : {folder} : {prompt}\n", now());
    if let Ok(mut f) = File::options().create(true).append(true).open(history_path()) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// `ai -H`: the history lines holding every word, any case, oldest first so
/// the newest ends up next to the prompt. Time and folder are gray on a tty.
pub fn print_history(words: &[String]) -> i32 {
    let text = std::fs::read_to_string(history_path()).unwrap_or_default();
    let color = term::is_tty(1);
    let mut out = String::new();
    for line in matching(&text, words) {
        match line.match_indices(" : ").nth(1) {
            Some((at, sep)) if color => {
                let (head, prompt) = line.split_at(at + sep.len());
                out.push_str(&format!("{GRAY}{head}{RESET}{prompt}\n"));
            }
            _ => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    // Piped into head, stdout closes early: not an error worth a panic.
    let _ = io::stdout().lock().write_all(out.as_bytes());
    0
}

fn matching<'a>(text: &'a str, words: &[String]) -> impl Iterator<Item = &'a str> {
    let words: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();
    text.lines().filter(move |line| {
        let line = line.to_lowercase();
        words.iter().all(|w| line.contains(w.as_str()))
    })
}

/// Called as an agent starts. A line another wrapper appends between the
/// read and the rename is lost; one prompt in a megabyte of them.
pub fn trim_history() {
    trim_in(&history_path(), HISTORY_MAX, HISTORY_KEEP);
}

fn trim_in(path: &Path, max: u64, keep: usize) {
    if std::fs::metadata(path).map_or(true, |m| m.len() <= max) {
        return;
    }
    let Ok(data) = std::fs::read(path) else { return };
    let mut start = data.len().saturating_sub(keep);
    if start > 0 && data[start - 1] != b'\n' {
        start = data[start..].iter().position(|&b| b == b'\n').map_or(data.len(), |i| start + i + 1);
    }
    let tmp = path.with_extension("txt.tmp");
    if std::fs::write(&tmp, &data[start..]).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Local time as `YYYY-MM-DD HH:MM:SS`.
fn now() -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        libc::localtime_r(&t, &mut tm);
    }
    let (y, mo, d, h, mi, s) = (tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse_with_comments_and_prefixes() {
        let d = parse_defaults("# mine\n\ntool = cod\nfull = true # always\nawake=false\nlines = 50\n").unwrap();
        assert_eq!(d, Defaults { tool: Some("codex"), full: true, bare: false, awake: false, lines: Some(20) });
    }

    #[test]
    fn remembered_tool_replaces_its_line_and_leaves_the_rest() {
        assert_eq!(with_tool("", "omp"), "tool = omp\n");
        assert_eq!(with_tool("# mine\nfull = true # always\n", "omp"), "# mine\nfull = true # always\ntool = omp\n");
        assert_eq!(with_tool("tool = cod # old\nlines = 5\n", "omp"), "tool = omp\nlines = 5\n");
        let same = "lines = 5\ntool = omp # mine\n";
        assert_eq!(with_tool(same, "omp"), same, "already there: not rewritten");
        assert_eq!(parse_defaults(&with_tool("# tool = grok\n", "omp")).unwrap().tool, Some("omp"));
    }

    #[test]
    fn defaults_errors_name_the_line() {
        assert_eq!(parse_defaults("full = true\nnope = 1").unwrap_err(), "2: unknown key \"nope\"");
        assert_eq!(parse_defaults("full = yes").unwrap_err(), "1: full is true or false, got \"yes\"");
        assert_eq!(parse_defaults("tool").unwrap_err(), "1: expected key = value, got \"tool\"");
        assert!(parse_defaults("tool = x").unwrap_err().starts_with("1: unknown tool \"x\""));
        assert!(parse_defaults("tool =").is_err());
        assert!(parse_defaults("lines = many").is_err());
    }

    #[test]
    fn search_needs_every_word_in_any_case() {
        let text = "t : ~/a : Fix the BAR\nt : ~/b : fix tests\nt : ~/a : bar resize\n";
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(matching(text, &words(&["bar", "fix"])).collect::<Vec<_>>(), ["t : ~/a : Fix the BAR"]);
        assert_eq!(matching(text, &words(&["~/a"])).count(), 2);
        assert_eq!(matching(text, &[]).count(), 3);
    }

    #[test]
    fn trim_keeps_the_newest_whole_lines() {
        let path = std::env::temp_dir().join(format!("ai-trim-{}.txt", std::process::id()));
        std::fs::write(&path, "aaaa\nbbbb\ncccc\n").unwrap();
        trim_in(&path, 100, 7);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "aaaa\nbbbb\ncccc\n", "under max: untouched");
        trim_in(&path, 10, 7);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "cccc\n", "cut mid-line: that line goes too");
        std::fs::write(&path, "aaaa\nbbbb\ncccc\n").unwrap();
        trim_in(&path, 10, 10);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "bbbb\ncccc\n", "cut on a line start: kept");
        let _ = std::fs::remove_file(&path);
    }
}
