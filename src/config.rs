//! `~/.config/ai` (or `$XDG_CONFIG_HOME/ai`): the defaults file, the
//! `--update` model cache and the prompt history, one file per folder.

use std::fs::File;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use crate::text::{GRAY, RESET, has_words};
use crate::{bang, cli, launch, pane, term};

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
    base.join("ai")
}

/// The prompt history of `folder`; every folder's file sits in `history/`.
pub fn history_file(folder: &Path) -> PathBuf {
    dir().join("history").join(history_name(&pane::tilde(folder)))
}

/// `~/dev/proj` is `dev__proj.txt`, `~` is `~.txt`, `/tmp/x` is `__tmp__x.txt`.
fn history_name(folder: &str) -> String {
    let rel = folder.strip_prefix("~/").unwrap_or(folder);
    format!("{}.txt", rel.replace('/', "__"))
}

/// Agent run defaults from `config`; command line switches add to them.
#[derive(Debug, Default, PartialEq)]
pub struct Defaults {
    pub tool: Option<&'static str>,
    pub full: bool,
    pub bare: bool,
    pub awake: bool,
    pub lines: Option<usize>,
    pub commands: Option<PathBuf>,
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
            "commands" if value.is_empty() => return Err(err("commands takes a folder".into())),
            "commands" => d.commands = Some(bang::expand(value)),
            _ => return Err(err(format!("unknown key {key:?}"))),
        }
    }
    Ok(d)
}

/// One line per prompt: `2026-10-22 14:03:11 : the prompt`.
/// A single append, so wrappers running side by side never interleave lines.
pub fn append_history(file: &Path, prompt: &str) {
    if file.parent().is_none_or(|dir| std::fs::create_dir_all(dir).is_err()) {
        return;
    }
    let line = format!("{} : {prompt}\n", now());
    if let Ok(mut f) = File::options().create(true).append(true).open(file) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// `ai -H`: this folder's history lines holding every word, any case, oldest
/// first so the newest ends up next to the prompt. The time is gray on a tty.
pub fn print_history(words: &[String]) -> i32 {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("ai: {e}");
            return 1;
        }
    };
    let text = std::fs::read_to_string(history_file(&cwd)).unwrap_or_default();
    let color = term::is_tty(1);
    let mut out = String::new();
    for line in matching(&text, words) {
        match line.split_once(" : ") {
            Some((time, prompt)) if color => out.push_str(&format!("{GRAY}{time} :{RESET} {prompt}\n")),
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

fn matching<'a>(text: &'a str, words: &'a [String]) -> impl Iterator<Item = &'a str> {
    text.lines().filter(move |line| has_words(line, words))
}

/// Called as an agent starts. A line another wrapper appends between the
/// read and the rename is lost; one prompt in a megabyte of them.
pub fn trim_history(file: &Path) {
    trim_in(file, HISTORY_MAX, HISTORY_KEEP);
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
        assert_eq!(d, Defaults { tool: Some("codex"), full: true, bare: false, awake: false, lines: Some(20), commands: None });
        assert_eq!(parse_defaults("commands = /x/y/../z").unwrap().commands, Some(PathBuf::from("/x/z")));
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
        let text = "t : Fix the BAR\nt : fix tests\nt : bar resize\n";
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(matching(text, &words(&["bar", "fix"])).collect::<Vec<_>>(), ["t : Fix the BAR"]);
        assert_eq!(matching(text, &words(&["bar"])).count(), 2);
        assert_eq!(matching(text, &[]).count(), 3);
    }

    #[test]
    fn history_names_flatten_the_folder() {
        assert_eq!(history_name("~/dev/tools/ai"), "dev__tools__ai.txt");
        assert_eq!(history_name("~"), "~.txt");
        assert_eq!(history_name("/tmp/x"), "__tmp__x.txt");
        assert_eq!(history_name("~/dev/a.b"), "dev__a.b.txt");
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
