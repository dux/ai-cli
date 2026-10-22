//! The rows a ! command leaves in the bar: where it ran, what ran, what it said.

use std::path::{Path, PathBuf};

use crate::text::{RESET, YELLOW, clip, dim, rule};

const LABEL: &str = "shell";

#[derive(Debug, Clone)]
pub struct Pane {
    pub cmd: String,
    pub pwd: PathBuf,
    pub out: Vec<String>,
    pub status: Option<i32>,
    pub note: Option<String>,
}

impl Pane {
    pub fn new(cmd: &str, out: Vec<String>, status: Option<i32>, note: Option<String>) -> Pane {
        Pane {
            cmd: cmd.to_string(),
            pwd: std::env::current_dir().unwrap_or_default(),
            out,
            status,
            note,
        }
    }

    pub fn pending(cmd: &str) -> Pane {
        Pane::new(cmd, Vec::new(), None, Some("running".into()))
    }

    /// Rule, pwd, the command as typed, then its output. `max` is everything
    /// the bar may take.
    pub fn lines(&self, cols: usize, max: usize) -> Vec<String> {
        let head = [
            dim(&clip(&tilde(&self.pwd), cols)),
            format!("{YELLOW}!{RESET} {}", clip(&self.cmd, cols.saturating_sub(2))),
        ];
        let body: Vec<String> = if self.out.is_empty() {
            vec![dim("(no output)")]
        } else {
            self.out.iter().map(|l| clip(l, cols)).collect()
        };
        let room = max as isize - 1 - head.len() as isize;
        let mut lines = vec![rule(cols, &self.label())];
        lines.extend(head);
        lines.extend(fit(body, room));
        lines
    }

    fn label(&self) -> String {
        let mut bits = vec![LABEL.to_string()];
        if let Some(s) = self.status.filter(|s| *s > 0) {
            bits.push(format!("exit {s}"));
        }
        if let Some(n) = &self.note {
            bits.push(n.clone());
        }
        bits.join(" - ")
    }
}

/// Long output is cut from the bottom.
fn fit(mut rows: Vec<String>, room: isize) -> Vec<String> {
    if rows.len() as isize <= room {
        return rows;
    }
    if room < 1 {
        return Vec::new();
    }
    let room = room as usize;
    let more = rows.len() - room + 1;
    rows.truncate(room - 1);
    rows.push(dim(&format!("… +{more} more lines")));
    rows
}

fn tilde(path: &Path) -> String {
    let p = path.to_string_lossy().into_owned();
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && (p == home || p.starts_with(&format!("{home}/"))) {
        format!("~{}", &p[home.len()..])
    } else {
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_sgr(s: &str) -> String {
        regex::Regex::new(r"\x1b\[[\d;]*m").unwrap().replace_all(s, "").into_owned()
    }

    fn pane(cmd: &str, out: &[&str], status: Option<i32>, note: Option<&str>) -> Pane {
        Pane::new(cmd, out.iter().map(|s| s.to_string()).collect(), status, note.map(Into::into))
    }

    fn rows(p: &Pane, cols: usize, max: usize) -> Vec<String> {
        p.lines(cols, max).iter().map(|l| strip_sgr(l)).collect()
    }

    #[test]
    fn pwd_then_command_then_output() {
        let p = pane("ls", &["one", "two"], None, None);
        let out = rows(&p, 40, 10);
        assert!(out[0].contains(" shell "));
        assert_eq!(out[1], clip(&tilde(&p.pwd), 40));
        assert_eq!(out[2], "! ls");
        assert_eq!(out[3..], ["one", "two"]);
    }

    #[test]
    fn a_failure_says_so_in_the_rule() {
        assert!(rows(&pane("false", &[], Some(1), None), 40, 10)[0].contains("shell - exit 1"));
        let t = pane("sleep 60", &[], None, Some("timed out after 10s"));
        assert!(rows(&t, 40, 10)[0].contains("timed out"));
    }

    #[test]
    fn silence_is_reported_rather_than_left_blank() {
        assert_eq!(rows(&pane("true", &[], Some(0), None), 40, 10).last().unwrap(), "(no output)");
    }

    #[test]
    fn long_output_is_cut_to_the_room_there_is() {
        let lines: Vec<String> = (1..=20).map(|i| i.to_string()).collect();
        let p = Pane::new("ls", lines, None, None);
        let out = rows(&p, 40, 6);
        assert_eq!(out.len(), 6);
        assert_eq!(out[3..5], ["1", "2"]);
        assert_eq!(out.last().unwrap(), "… +18 more lines");
    }

    #[test]
    fn rows_are_clipped_to_the_width() {
        let p = Pane::new("ls", vec!["x".repeat(200)], None, None);
        let out = rows(&p, 30, 10);
        let last = out.last().unwrap();
        assert_eq!(last.chars().count(), 30);
        assert!(last.ends_with('…'));
    }

    #[test]
    fn tilde_only_on_a_path_boundary() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(tilde(Path::new(&home)), "~");
        assert_eq!(tilde(Path::new(&format!("{home}/x"))), "~/x");
        assert_eq!(tilde(Path::new(&format!("{home}x"))), format!("{home}x"));
    }
}
