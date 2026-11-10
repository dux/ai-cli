//! `ai TOOL MODEL`: MODEL is a fragment such as `5-2`, completed to the model
//! last used in that tool whose name holds it. Every agent keeps its own record
//! of the models it ran. These are private formats: a record that cannot be
//! read counts as empty, and the fragment then goes to the tool as typed.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use regex::bytes::Regex;

/// Newest claude session files read; a model last used before those is not found.
const CLAUDE_SESSIONS: usize = 100;

pub fn resolve(tool: &str, fragment: &str, home: &Path) -> String {
    pick(&used(tool, home), fragment).unwrap_or(fragment).to_string()
}

/// A full name wins, so it is never bent to a newer model that merely
/// contains it; otherwise the newest name holding the fragment. Case does not
/// count, and neither does `.` against `-`: `5.5` finds `claude-opus-5-5`.
fn pick<'a>(used: &'a [String], fragment: &str) -> Option<&'a str> {
    let plain = |s: &str| s.to_lowercase().replace('.', "-");
    let want = plain(fragment);
    let names = || used.iter().map(String::as_str);
    names().find(|m| plain(m) == want).or_else(|| names().find(|m| plain(m).contains(&want)))
}

/// Models the tool has run, newest first, repeats included.
fn used(tool: &str, home: &Path) -> Vec<String> {
    match tool {
        "claude" => claude(&home.join(".claude/projects")),
        "codex" => codex(&home.join(".codex")),
        "grok" => grok(&home.join(".grok/sessions")),
        "omp" => query(&home.join(".omp/agent/agent.db"), "select model_key from model_usage order by last_used_at desc"),
        _ => Vec::new(),
    }
}

/// `<project>/<session>.jsonl`: every reply line names its model. `<synthetic>`
/// marks replies claude made up itself.
fn claude(projects: &Path) -> Vec<String> {
    let mut sessions: Vec<(SystemTime, PathBuf)> = entries(projects)
        .iter()
        .flat_map(|project| entries(project))
        .filter(|f| f.extension().is_some_and(|e| e == "jsonl"))
        .filter_map(|f| Some((f.metadata().ok()?.modified().ok()?, f)))
        .collect();
    sessions.sort_by(|a, b| b.0.cmp(&a.0));
    let model = Regex::new(r#""model":"([^"<][^"]*)""#).unwrap();
    let mut out = Vec::new();
    for (_, file) in sessions.iter().take(CLAUDE_SESSIONS) {
        let data = std::fs::read(file).unwrap_or_default();
        let mut found: Vec<String> = model.captures_iter(&data).map(|c| String::from_utf8_lossy(&c[1]).into_owned()).collect();
        found.reverse();
        found.dedup();
        out.extend(found);
    }
    out
}

/// `state_<n>.sqlite`, n being the schema version: the highest is the live one.
fn codex(dir: &Path) -> Vec<String> {
    let live = entries(dir)
        .into_iter()
        .filter_map(|f| {
            let n: u32 = f.file_name()?.to_str()?.strip_prefix("state_")?.strip_suffix(".sqlite")?.parse().ok()?;
            Some((n, f))
        })
        .max_by_key(|(n, _)| *n);
    match live {
        Some((_, db)) => {
            query(&db, "select model from threads where model is not null group by model order by max(updated_at) desc")
        }
        None => Vec::new(),
    }
}

/// `<folder>/<session>/summary.json`. `updated_at` is UTC text, so its order
/// is time order.
fn grok(sessions: &Path) -> Vec<String> {
    let mut seen: Vec<(String, String)> = entries(sessions)
        .iter()
        .flat_map(|folder| entries(folder))
        .filter_map(|session| {
            let summary = std::fs::read(session.join("summary.json")).ok()?;
            let v: serde_json::Value = serde_json::from_slice(&summary).ok()?;
            Some((v["updated_at"].as_str()?.to_string(), v["current_model_id"].as_str()?.to_string()))
        })
        .collect();
    seen.sort_by(|a, b| b.0.cmp(&a.0));
    seen.into_iter().map(|(_, model)| model).collect()
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default()
}

/// One value per line; nothing when the database or sqlite3 itself is missing.
fn query(db: &Path, sql: &str) -> Vec<String> {
    let out = Command::new("sqlite3").arg("-readonly").arg(db).arg(sql).stdin(Stdio::null()).stderr(Stdio::null()).output();
    match out {
        Ok(out) => String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.is_empty()).map(str::to_string).collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Home(PathBuf);
    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn home(tag: &str) -> Home {
        let dir = std::env::temp_dir().join(format!("ai-model-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Home(dir)
    }

    fn put(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn sql(db: &Path, script: &str) {
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        assert!(Command::new("sqlite3").arg(db).arg(script).status().unwrap().success());
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn newest_fit_wins_but_a_full_name_is_kept() {
        let used = s(&["anthropic/claude-fable-5-2", "anthropic/claude-fable-5", "anthropic/claude-opus-5-5"]);
        assert_eq!(pick(&used, "5-2"), Some("anthropic/claude-fable-5-2"));
        assert_eq!(pick(&used, "OPUS"), Some("anthropic/claude-opus-5-5"));
        assert_eq!(pick(&used, "fable"), Some("anthropic/claude-fable-5-2"));
        assert_eq!(pick(&used, "anthropic/claude-fable-5"), Some("anthropic/claude-fable-5"));
        assert_eq!(pick(&used, "gpt"), None);
        assert_eq!(pick(&used, "5.2"), Some("anthropic/claude-fable-5-2"));
        assert_eq!(pick(&s(&["gpt-6.1-sol"]), "6-1"), Some("gpt-6.1-sol"));
    }

    #[test]
    fn no_record_passes_the_fragment_on() {
        let h = home("none");
        for tool in ["claude", "codex", "grok", "omp"] {
            assert_eq!(resolve(tool, "opus", &h.0), "opus", "{tool}");
        }
    }

    #[test]
    fn claude_reads_newest_session_first_and_its_last_reply_first() {
        let h = home("claude");
        let projects = h.0.join(".claude/projects");
        let reply = |m: &str| format!("{{\"type\":\"assistant\",\"message\":{{\"model\":\"{m}\",\"content\":[]}}}}\n");
        let old = projects.join("-a/old.jsonl");
        put(&old, &reply("claude-opus-5-5"));
        let age = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&old).unwrap().set_modified(age).unwrap();
        let new = [reply("claude-haiku-5-5"), reply("claude-fable-5-2"), reply("<synthetic>")].concat();
        put(&projects.join("-b/new.jsonl"), &new);
        put(&projects.join("-b/notes.txt"), &reply("claude-stray-9"));
        assert_eq!(used("claude", &h.0), s(&["claude-fable-5-2", "claude-haiku-5-5", "claude-opus-5-5"]));
        assert_eq!(resolve("claude", "5-5", &h.0), "claude-haiku-5-5");
    }

    #[test]
    fn codex_reads_the_highest_state_version() {
        let h = home("codex");
        let schema = "create table threads (id text, model text, updated_at integer);";
        sql(&h.0.join(".codex/state_4.sqlite"), &format!("{schema} insert into threads values ('a', 'gpt-5-old', 900);"));
        let rows = "insert into threads values ('a', 'gpt-6-sol', 100), ('b', 'gpt-6.1-sol', 300), ('c', 'gpt-6-sol', 200), ('d', null, 400);";
        sql(&h.0.join(".codex/state_5.sqlite"), &format!("{schema} {rows}"));
        assert_eq!(used("codex", &h.0), s(&["gpt-6.1-sol", "gpt-6-sol"]));
    }

    #[test]
    fn grok_orders_sessions_by_updated_at() {
        let h = home("grok");
        let sessions = h.0.join(".grok/sessions");
        let summary = |at: &str, m: &str| format!("{{\"updated_at\":\"{at}\",\"current_model_id\":\"{m}\"}}");
        put(&sessions.join("%2Fa/1/summary.json"), &summary("2026-10-22T12:51:13.5Z", "grok-4.6"));
        put(&sessions.join("%2Fb/2/summary.json"), &summary("2026-10-23T08:00:00.1Z", "grok-4.7"));
        put(&sessions.join("%2Fb/3/summary.json"), "not json");
        put(&sessions.join("%2Fb/prompt_history.jsonl"), "{}");
        assert_eq!(used("grok", &h.0), s(&["grok-4.7", "grok-4.6"]));
    }

    #[test]
    fn omp_orders_by_last_use() {
        let h = home("omp");
        let rows = "insert into model_usage values ('anthropic/claude-opus-5-5', 100), ('anthropic/claude-fable-5-2', 200);";
        sql(&h.0.join(".omp/agent/agent.db"), &format!("create table model_usage (model_key text primary key, last_used_at integer); {rows}"));
        assert_eq!(used("omp", &h.0), s(&["anthropic/claude-fable-5-2", "anthropic/claude-opus-5-5"]));
        assert_eq!(resolve("omp", "5-2", &h.0), "anthropic/claude-fable-5-2");
    }
}
