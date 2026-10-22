//! `ai --update`: runs each agent CLI's own updater, reports old -> new, then
//! re-reads the model lists and shows what appeared or disappeared since the
//! last run (cache shared with the Ruby `llm wrap:run --update`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use regex::Regex;

use crate::launch;

const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const GRAY: &str = "\x1b[90m";
const RESET: &str = "\x1b[0m";

struct Row {
    tool: String,
    installed: bool,
    before: Option<String>,
    after: Option<String>,
    failed: bool,
}

fn say(color: &str, s: &str) {
    if color.is_empty() {
        println!("{s}");
    } else {
        println!("{color}{s}{RESET}");
    }
}

fn width() -> usize {
    launch::TOOLS.iter().map(|t| t.name.len()).max().unwrap_or(0)
}

/// Returns the exit status - non-zero if any updater failed.
pub fn run(tools: &[String]) -> i32 {
    let rows: Vec<Row> = tools.iter().map(|t| update_one(t)).collect();
    println!();
    rows.iter().for_each(report);
    let installed: Vec<String> = rows.iter().filter(|r| r.installed).map(|r| r.tool.clone()).collect();
    report_models(&installed);
    if rows.iter().any(|r| r.failed) { 1 } else { 0 }
}

fn update_one(tool: &str) -> Row {
    if !command_exists(tool) {
        return Row { tool: tool.into(), installed: false, before: None, after: None, failed: false };
    }
    let argv = launch::spec(tool).update;
    let before = version(tool);
    say(GRAY, &format!("$ {}", argv.join(" ")));
    let ok = Command::new(argv[0]).args(&argv[1..]).status().is_ok_and(|s| s.success());
    Row { tool: tool.into(), installed: true, before, after: version(tool), failed: !ok }
}

fn report(row: &Row) {
    let name = format!("{:<w$}", row.tool, w = width());
    if !row.installed {
        return say(GRAY, &format!("* {name}  not installed"));
    }
    if row.failed {
        return say(RED, &format!("* {name}  update failed"));
    }
    match (&row.before, &row.after) {
        (Some(b), Some(a)) if b != a => say(GREEN, &format!("* {name}  {b} -> {a}")),
        (_, Some(a)) => say("", &format!("* {name}  {a} (latest)")),
        (b, None) => say("", &format!("* {name}  {}", b.as_deref().unwrap_or("unknown version"))),
    }
}

fn report_models(tools: &[String]) {
    let tools: Vec<&String> = tools.iter().filter(|t| launch::spec(t).models.is_some()).collect();
    if tools.is_empty() {
        return;
    }
    let mut cache = cache_read();
    println!();
    println!("models");
    for tool in tools {
        let old = cache.get(tool.as_str()).cloned();
        let list = report_model_list(tool, old);
        cache.insert(tool.clone(), list);
    }
    cache_write(&cache);
}

fn report_model_list(tool: &str, old: Option<Vec<String>>) -> Vec<String> {
    let list = models(tool);
    let name = format!("{:<w$}", tool, w = width());
    if list.is_empty() {
        say(GRAY, &format!("  {name}  no models listed"));
        return old.unwrap_or_default();
    }
    say("", &format!("  {name}  {} models", list.len()));
    let Some(old) = old else {
        say(GRAY, "    (first run)");
        return list;
    };
    let (added, removed) = diff(&old, &list);
    if added.is_empty() && removed.is_empty() {
        say(GRAY, "    unchanged");
    }
    added.iter().for_each(|m| say(GREEN, &format!("    + {m}")));
    removed.iter().for_each(|m| say(GRAY, &format!("    - {m}")));
    list
}

fn version(tool: &str) -> Option<String> {
    parse_version(&capture(launch::spec(tool).version))
}

fn models(tool: &str) -> Vec<String> {
    match launch::spec(tool).models {
        Some(argv) => parse_models(tool, &capture(argv)),
        None => Vec::new(),
    }
}

/// `2.1.268 (Claude Code)`, `codex-cli 0.154.0`, `grok 1.0.25 (f7e67d6) [stable]`, `1.18.30`, `omp/18.12.5`
pub fn parse_version(out: &str) -> Option<String> {
    let first = out.lines().next()?;
    Regex::new(r"\d+\.\d+(?:\.\d+)*").unwrap().find(first).map(|m| m.as_str().to_string())
}

/// How each CLI prints its list: grok bullets under a preamble and marks the
/// default, opencode prints bare provider/model lines, omp boxes tables whose
/// first column is the model id (the `model` header row is skipped).
pub fn model_re(tool: &str) -> Option<Regex> {
    let re = match tool {
        "grok" => r"^\s*[*-]\s+(\S+)",
        "opencode" => r"^(\S+/\S+)$",
        // Ruby uses a negative lookahead for the header; filtered in parse_models.
        "omp" => r"^│ (\S+)\s+│",
        _ => return None,
    };
    Some(Regex::new(re).unwrap())
}

pub fn parse_models(tool: &str, out: &str) -> Vec<String> {
    let Some(re) = model_re(tool) else { return Vec::new() };
    let mut list: Vec<String> = out
        .lines()
        .filter_map(|l| re.captures(l.trim_end()).map(|c| c[1].to_string()))
        .filter(|m| !(tool == "omp" && m == "model"))
        .collect();
    list.sort();
    list.dedup();
    list
}

pub fn diff(old: &[String], new: &[String]) -> (Vec<String>, Vec<String>) {
    (
        new.iter().filter(|m| !old.contains(m)).cloned().collect(),
        old.iter().filter(|m| !new.contains(m)).cloned().collect(),
    )
}

fn cache_path() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".cache/llm/models.json")
}

fn cache_read() -> BTreeMap<String, Vec<String>> {
    std::fs::read_to_string(cache_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn cache_write(data: &BTreeMap<String, Vec<String>>) {
    let path = cache_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = PathBuf::from(format!("{}.{}.tmp", path.display(), std::process::id()));
    let Ok(json) = serde_json::to_string_pretty(data) else { return };
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// stdout+stderr merged, "" on failure.
fn capture(argv: &[&str]) -> String {
    let Ok(out) = Command::new(argv[0]).args(&argv[1..]).stdin(Stdio::null()).output() else {
        return String::new();
    };
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    s
}

fn command_exists(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p).any(|d| {
            std::fs::metadata(d.join(name)).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GROK: &str = "You are not authenticated.\n\nDefault model: grok-4.6\n\nAvailable models:\n  * grok-4.6 (default)\n  - grok-4.5\n  - grok-code-fast-1\n";
    const OPENCODE: &str = "opencode/claude-opus-5\nopencode/gemini-3.1-pro\nanthropic/claude-sonnet-5\n";
    const OMP: &str = "anthropic (2)\n┌──────────────────┬─────────┐\n│ model            │ context │\n├──────────────────┼─────────┤\n│ claude-opus-5    │      1M │\n│ claude-sonnet-5  │      1M │\n└──────────────────┴─────────┘\n";

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parse_version_every_shape() {
        assert_eq!(parse_version("2.1.268 (Claude Code)\n").as_deref(), Some("2.1.268"));
        assert_eq!(parse_version("codex-cli 0.154.0\n").as_deref(), Some("0.154.0"));
        assert_eq!(parse_version("grok 1.0.25 (f7e67d6988e2) [stable]\n").as_deref(), Some("1.0.25"));
        assert_eq!(parse_version("1.18.30\n").as_deref(), Some("1.18.30"));
        assert_eq!(parse_version("omp/18.12.5\n").as_deref(), Some("18.12.5"));
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("command not found\n"), None);
    }

    #[test]
    fn parse_models_per_tool() {
        assert_eq!(parse_models("grok", GROK), s(&["grok-4.5", "grok-4.6", "grok-code-fast-1"]));
        assert_eq!(
            parse_models("opencode", OPENCODE),
            s(&["anthropic/claude-sonnet-5", "opencode/claude-opus-5", "opencode/gemini-3.1-pro"])
        );
        assert_eq!(parse_models("omp", OMP), s(&["claude-opus-5", "claude-sonnet-5"]));
        assert!(parse_models("grok", "").is_empty());
        assert!(parse_models("opencode", "no providers configured\n").is_empty());
        assert!(parse_models("claude", "anything").is_empty());
    }

    #[test]
    fn every_model_tool_has_a_parser() {
        for t in launch::TOOLS {
            if t.models.is_some() {
                assert!(model_re(t.name).is_some(), "{}", t.name);
            }
        }
    }

    #[test]
    fn diff_added_removed() {
        assert_eq!(diff(&s(&["a", "b"]), &s(&["b", "c"])), (s(&["c"]), s(&["a"])));
        assert_eq!(diff(&s(&["a", "b"]), &s(&["a", "b"])), (vec![], vec![]));
        assert_eq!(diff(&[], &s(&["a", "b"])), (s(&["a", "b"]), vec![]));
    }
}
