//! The `:` picker: prompt commands from a folder of Markdown files, filtered
//! as you type. The chosen file's text goes to the agent as one paste, and
//! your prompt follows it.

use std::path::Path;

use crate::pane::tilde;
use crate::text::{BOLD, CYAN, RESET, REV, clip, dim, has_words, rule, width};

const LABEL: &str = "commands";
/// List rows at most. The bar keeps one height while the picker is open, so
/// the agent is not resized on every keystroke.
const ROWS: usize = 10;
const NAME_MAX: usize = 16;

pub struct Command {
    pub name: String,
    desc: String,
    text: String,
}

impl Command {
    fn new(name: String, text: String) -> Command {
        let desc = text.lines().map(|l| l.trim_start_matches('#').trim()).find(|l| !l.is_empty()).unwrap_or_default();
        Command { desc: desc.to_string(), name, text }
    }

    /// `<dir>/<name>.md`, or None when it cannot be read.
    pub fn load(dir: &Path, name: &str) -> Option<Command> {
        let text = std::fs::read_to_string(dir.join(format!("{name}.md"))).ok()?;
        Some(Command::new(name.to_string(), text))
    }

    /// The text as a bracketed paste with a blank line after it, so what is
    /// typed next starts below.
    pub fn paste(&self) -> String {
        format!("\x1b[200~{}\n\n\x1b[201~", self.body())
    }

    /// The text as a bracketed paste with a blank line before it, for after
    /// what was typed.
    pub fn paste_after(&self) -> String {
        format!("\x1b[200~\n\n{}\x1b[201~", self.body())
    }

    /// Control characters go: none may end the paste early.
    fn body(&self) -> String {
        self.text.trim_end().chars().filter(|&c| !c.is_control() || c == '\n' || c == '\t').collect()
    }
}

pub struct Picker {
    dir: String,
    all: Vec<Command>,
    query: String,
    /// Indexes into `all`, in the order shown.
    shown: Vec<usize>,
    at: usize,
}

impl Picker {
    /// Every `*.md` in `dir`, by name. A missing folder is an empty list.
    pub fn open(dir: &Path) -> Picker {
        let mut all: Vec<Command> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                if path.extension()? != "md" {
                    return None;
                }
                Command::load(dir, path.file_stem()?.to_str()?)
            })
            .collect();
        all.sort_by(|a, b| a.name.cmp(&b.name));
        let mut p = Picker { dir: tilde(dir), all, query: String::new(), shown: Vec::new(), at: 0 };
        p.refilter();
        p
    }

    /// Every word must be in the name or the description, any case; names
    /// starting with the first word lead.
    pub fn search(&mut self, query: &str) {
        if query != self.query {
            self.query = query.to_string();
            self.refilter();
        }
    }

    fn refilter(&mut self) {
        let words: Vec<String> = self.query.split_whitespace().map(str::to_string).collect();
        let all = &self.all;
        self.shown = (0..all.len()).filter(|&i| has_words(&format!("{} {}", all[i].name, all[i].desc), &words)).collect();
        if let Some(first) = words.first().map(|w| w.to_lowercase()) {
            self.shown.sort_by_key(|&i| !all[i].name.to_lowercase().starts_with(&first));
        }
        self.at = 0;
    }

    /// Moves the selection one row, wrapping at either end.
    pub fn step(&mut self, down: bool) {
        let n = self.shown.len();
        if n > 0 {
            self.at = if down { (self.at + 1) % n } else { (self.at + n - 1) % n };
        }
    }

    pub fn chosen(&self) -> Option<&Command> {
        self.shown.get(self.at).map(|&i| &self.all[i])
    }

    /// Rule, the search row with its cursor, then the list. `max` is
    /// everything the bar may take.
    pub fn lines(&self, cols: usize, max: usize) -> Vec<String> {
        let rows = ROWS.min(self.all.len().max(1)).min(max.saturating_sub(2).max(1));
        let label = format!("{LABEL} · {} of {}", self.shown.len(), self.all.len());
        let mut out = vec![rule(cols, &label), format!("{CYAN}:{RESET} {}{REV} {RESET}", clip(&self.query, cols.saturating_sub(4)))];

        if self.all.is_empty() {
            out.push(dim(&clip(&format!("(no commands in {})", self.dir), cols)));
        } else if self.shown.is_empty() {
            out.push(dim("(no command matches)"));
        }
        let pad = self.all.iter().map(|c| width(&c.name)).max().unwrap_or(0).min(NAME_MAX);
        let top = self.at.saturating_sub(rows - 1);
        for (k, &i) in self.shown.iter().enumerate().skip(top).take(rows) {
            let c = &self.all[i];
            let name = clip(&c.name, pad);
            let name = format!("{name}{}", " ".repeat(pad - width(&name)));
            let desc = clip(&c.desc, cols.saturating_sub(pad + 4));
            out.push(if k == self.at {
                format!("{CYAN}❯{RESET} {BOLD}{name}{RESET}  {desc}")
            } else {
                format!("  {name}  {}", dim(&desc))
            });
        }
        out.resize(rows + 2, String::new());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn folder(files: &[(&str, &str)]) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("ai-picker-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, text) in files {
            std::fs::write(dir.join(name), text).unwrap();
        }
        dir
    }

    fn names(p: &Picker) -> Vec<&str> {
        p.shown.iter().map(|&i| p.all[i].name.as_str()).collect()
    }

    #[test]
    fn lists_markdown_by_name_with_the_heading_as_description() {
        let dir = folder(&[("plan.md", "# planning mode\n\nbody\n"), ("commit.md", "\n## commit all\n"), ("notes.txt", "x")]);
        let p = Picker::open(&dir);
        assert_eq!(names(&p), ["commit", "plan"]);
        assert_eq!(p.all[1].desc, "planning mode");
        assert_eq!(p.all[0].desc, "commit all");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn search_needs_every_word_and_puts_name_prefixes_first() {
        let dir = folder(&[("feature.md", "# plan a feature\n"), ("plan.md", "# planning mode\n"), ("wt.md", "# worktree\n")]);
        let mut p = Picker::open(&dir);
        p.search("PLAN");
        assert_eq!(names(&p), ["plan", "feature"]);
        p.search("plan feat");
        assert_eq!(names(&p), ["feature"]);
        p.search("zzz");
        assert!(p.chosen().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn selection_wraps_and_resets_on_a_new_search() {
        let dir = folder(&[("a.md", "a"), ("b.md", "b"), ("c.md", "c")]);
        let mut p = Picker::open(&dir);
        p.step(false);
        assert_eq!(p.chosen().unwrap().name, "c");
        p.step(true);
        assert_eq!(p.chosen().unwrap().name, "a");
        p.step(true);
        p.search("b");
        assert_eq!(p.chosen().unwrap().name, "b");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_bar_keeps_its_height_while_filtering() {
        let dir = folder(&[("a.md", "a"), ("b.md", "b"), ("c.md", "c")]);
        let mut p = Picker::open(&dir);
        assert_eq!(p.lines(60, 40).len(), 5);
        p.search("b");
        assert_eq!(p.lines(60, 40).len(), 5);
        assert_eq!(p.lines(60, 3).len(), 3, "a short screen shows fewer rows");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn paste_is_bracketed_and_cannot_end_early() {
        let c = Command::new("x".into(), "# x\nbody\x1b[201~more\r\n\n".into());
        assert_eq!(c.paste(), "\x1b[200~# x\nbody[201~more\n\n\x1b[201~");
        assert_eq!(c.paste_after(), "\x1b[200~\n\n# x\nbody[201~more\x1b[201~");
    }
}
