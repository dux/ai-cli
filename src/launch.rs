//! Picks the agent CLI by prefix and maps the generic -c / -f switches to what
//! that CLI takes. The AGENTS.md chain is injected by default; -b leaves it out.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// How a tool takes the AGENTS.md chain.
pub enum Agents {
    /// Flag followed by the combined text.
    Flag(&'static str),
    /// Flag followed by `prefix` + combined text.
    FlagPrefixed(&'static str, &'static str),
    /// Inline opencode config listing the file paths.
    OpencodeEnv,
}

pub struct Tool {
    pub name: &'static str,
    pub cont: &'static [&'static str],
    pub full: &'static [&'static str],
    pub update: &'static [&'static str],
    pub version: &'static [&'static str],
    /// None where the CLI has no listing command - skip, not "none".
    pub models: Option<&'static [&'static str]>,
    pub agents: Agents,
}

pub const TOOLS: &[Tool] = &[
    Tool {
        name: "claude",
        cont: &["-c"],
        full: &["--dangerously-skip-permissions"],
        update: &["claude", "update"],
        version: &["claude", "--version"],
        models: None,
        agents: Agents::Flag("--append-system-prompt"),
    },
    Tool {
        name: "codex",
        cont: &["resume", "--last"],
        full: &["--dangerously-bypass-approvals-and-sandbox"],
        update: &["codex", "update"],
        version: &["codex", "--version"],
        models: None,
        agents: Agents::FlagPrefixed("-c", "developer_instructions="),
    },
    Tool {
        name: "grok",
        cont: &["-c"],
        full: &["--always-approve"],
        update: &["grok", "update"],
        version: &["grok", "version"],
        models: Some(&["grok", "models"]),
        agents: Agents::Flag("--rules"),
    },
    Tool {
        name: "opencode",
        cont: &["-c"],
        full: &["--auto"],
        update: &["opencode", "upgrade"],
        version: &["opencode", "--version"],
        models: Some(&["opencode", "models"]),
        agents: Agents::OpencodeEnv,
    },
    Tool {
        name: "omp",
        cont: &["-c"],
        full: &["--auto-approve"],
        update: &["omp", "update"],
        version: &["omp", "--version"],
        models: Some(&["omp", "models"]),
        agents: Agents::Flag("--append-system-prompt"),
    },
];

/// Read AGENTS.md from the git root down on their own; only files above it are added.
const NATIVE_AGENTS: &[&str] = &["codex", "grok", "opencode"];
/// Walk AGENTS.md from cwd up to ~ on their own, so nothing is added.
const HOME_AGENTS: &[&str] = &["omp"];

pub fn names() -> Vec<String> {
    TOOLS.iter().map(|t| t.name.to_string()).collect()
}

pub fn spec(name: &str) -> &'static Tool {
    TOOLS.iter().find(|t| t.name == name).expect("known tool")
}

/// First tool whose name starts with the prefix, in TOOLS order. No prefix = claude.
pub fn tool(prefix: Option<&str>) -> Option<&'static str> {
    match prefix {
        None | Some("") => Some("claude"),
        Some(p) => TOOLS.iter().find(|t| t.name.starts_with(p)).map(|t| t.name),
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct Launch {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub files: Vec<PathBuf>,
}

pub struct Switches {
    pub cont: bool,
    pub full: bool,
    pub bare: bool,
}

pub fn build(tool: &str, extra: &[String], sw: &Switches, cwd: &Path, home: &Path) -> Launch {
    let spec = spec(tool);
    let mut argv = vec![tool.to_string()];
    if sw.cont {
        argv.extend(spec.cont.iter().map(|s| s.to_string()));
    }
    if sw.full {
        argv.extend(spec.full.iter().map(|s| s.to_string()));
    }
    let files = if sw.bare { Vec::new() } else { agents_files(tool, cwd, home) };
    let mut env = Vec::new();
    if !files.is_empty() {
        match spec.agents {
            Agents::Flag(f) => argv.extend([f.to_string(), agents_text(&files)]),
            Agents::FlagPrefixed(f, p) => argv.extend([f.to_string(), format!("{p}{}", agents_text(&files))]),
            Agents::OpencodeEnv => {
                let paths: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
                let json = serde_json::json!({ "instructions": paths }).to_string();
                env.push(("OPENCODE_CONFIG_CONTENT".to_string(), json));
            }
        }
    }
    argv.extend(extra.iter().cloned());
    Launch { argv, env, files }
}

/// AGENTS.md from home down to cwd, outermost first. For NATIVE_AGENTS tools the
/// walk stops above the git root - or above cwd outside git.
pub fn agents_files(tool: &str, cwd: &Path, home: &Path) -> Vec<PathBuf> {
    let mut stop: Option<String> = None;
    if NATIVE_AGENTS.contains(&tool) {
        let top = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["rev-parse", "--show-toplevel"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        stop = Some(if top.is_empty() { cwd.display().to_string() } else { top });
    }
    if HOME_AGENTS.contains(&tool) {
        stop = Some(home.display().to_string());
    }
    let home = home.display().to_string();
    let mut out: Vec<PathBuf> = cwd
        .ancestors()
        .filter(|d| {
            let s = d.display().to_string();
            s.starts_with(&home) && !stop.as_ref().is_some_and(|st| s.starts_with(st.as_str()))
        })
        .map(|d| d.join("AGENTS.md"))
        .filter(|f| f.is_file())
        .collect();
    out.reverse();
    out
}

pub fn agents_text(files: &[PathBuf]) -> String {
    let body: Vec<String> = files
        .iter()
        .map(|f| {
            let text = std::fs::read_to_string(f).unwrap_or_default();
            format!("## {}\n\n{}\n", f.display(), text.trim())
        })
        .collect();
    format!("Project instructions (AGENTS.md), broad to specific:\n\n{}", body.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sw(cont: bool, full: bool, bare: bool) -> Switches {
        Switches { cont, full, bare }
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    struct Tree {
        home: PathBuf,
        cwd: PathBuf,
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    fn tree(tag: &str, git: bool) -> Tree {
        let base = std::env::temp_dir().join(format!("ai-launch-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let home = base.canonicalize().unwrap();
        let cwd = home.join("dev/proj");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(home.join("AGENTS.md"), "home rule").unwrap();
        std::fs::write(home.join("dev/AGENTS.md"), "dev rule").unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "root rule").unwrap();
        if git {
            Command::new("git").args(["init", "-q"]).arg(&cwd).status().unwrap();
        }
        Tree { home, cwd }
    }

    fn rel(files: &[PathBuf], home: &Path) -> Vec<String> {
        files.iter().map(|f| f.strip_prefix(home).unwrap().display().to_string()).collect()
    }

    #[test]
    fn tool_prefix_first_match_wins() {
        assert_eq!(tool(None), Some("claude"));
        assert_eq!(tool(Some("c")), Some("claude"));
        assert_eq!(tool(Some("co")), Some("codex"));
        assert_eq!(tool(Some("g")), Some("grok"));
        assert_eq!(tool(Some("o")), Some("opencode"));
        assert_eq!(tool(Some("om")), Some("omp"));
        assert_eq!(tool(Some("x")), None);
    }

    #[test]
    fn switch_mapping_per_tool() {
        let d = Path::new("/");
        let b = |t| build(t, &[], &sw(true, true, true), d, d).argv;
        assert_eq!(b("claude"), s(&["claude", "-c", "--dangerously-skip-permissions"]));
        assert_eq!(b("codex"), s(&["codex", "resume", "--last", "--dangerously-bypass-approvals-and-sandbox"]));
        assert_eq!(b("grok"), s(&["grok", "-c", "--always-approve"]));
        assert_eq!(b("opencode"), s(&["opencode", "-c", "--auto"]));
        assert_eq!(b("omp"), s(&["omp", "-c", "--auto-approve"]));
    }

    #[test]
    fn every_tool_declares_update_and_version_argv() {
        for t in TOOLS {
            assert_eq!(t.update[0], t.name);
            assert_eq!(t.version[0], t.name);
            if let Some(m) = t.models {
                assert_eq!(m[0], t.name);
            }
        }
    }

    #[test]
    fn extra_args_pass_through_last() {
        let d = Path::new("/");
        let l = build("claude", &s(&["--model", "opus"]), &sw(true, false, true), d, d);
        assert_eq!(l.argv, s(&["claude", "-c", "--model", "opus"]));
        assert!(l.env.is_empty());
        assert!(l.files.is_empty());
    }

    #[test]
    fn agents_injected_home_to_cwd_skip_git_root_for_native() {
        let t = tree("git", true);
        let (home, cwd) = (&t.home, &t.cwd);
        let none = sw(false, false, false);
        let claude = build("claude", &[], &none, cwd, home);
        assert_eq!(rel(&claude.files, home), s(&["AGENTS.md", "dev/AGENTS.md", "dev/proj/AGENTS.md"]));
        assert_eq!(claude.argv[1], "--append-system-prompt");
        assert!(claude.argv[2].contains(&format!("## {}/dev/AGENTS.md", home.display())));
        assert!(claude.argv[2].contains("root rule"));

        let codex = build("codex", &[], &none, cwd, home);
        assert_eq!(rel(&codex.files, home), s(&["AGENTS.md", "dev/AGENTS.md"]));
        assert_eq!(codex.argv[1], "-c");
        assert!(codex.argv[2].starts_with("developer_instructions=Project instructions"));

        let oc = build("opencode", &[], &none, cwd, home);
        assert_eq!(oc.argv, s(&["opencode"]));
        assert_eq!(oc.env[0].0, "OPENCODE_CONFIG_CONTENT");
        let v: serde_json::Value = serde_json::from_str(&oc.env[0].1).unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "instructions": [
                format!("{}/AGENTS.md", home.display()),
                format!("{}/dev/AGENTS.md", home.display())
            ] })
        );

        let omp = build("omp", &[], &none, cwd, home);
        assert_eq!(omp.argv, s(&["omp"]));
        assert!(omp.files.is_empty());
    }

    #[test]
    fn bare_skips_agents() {
        let t = tree("bare", true);
        let l = build("claude", &s(&["--model", "opus"]), &sw(false, false, true), &t.cwd, &t.home);
        assert_eq!(l.argv, s(&["claude", "--model", "opus"]));
        assert!(l.env.is_empty() && l.files.is_empty());
        assert!(build("opencode", &[], &sw(false, false, true), &t.cwd, &t.home).env.is_empty());
    }

    #[test]
    fn agents_outside_git_stop_above_cwd_for_native() {
        let t = tree("nogit", false);
        let none = sw(false, false, false);
        assert_eq!(
            rel(&build("claude", &[], &none, &t.cwd, &t.home).files, &t.home),
            s(&["AGENTS.md", "dev/AGENTS.md", "dev/proj/AGENTS.md"])
        );
        assert_eq!(rel(&build("grok", &[], &none, &t.cwd, &t.home).files, &t.home), s(&["AGENTS.md", "dev/AGENTS.md"]));
    }

    #[test]
    fn nothing_found_adds_nothing() {
        let base = std::env::temp_dir().join(format!("ai-launch-{}-empty", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let l = build("claude", &[], &sw(false, false, false), &base, &base);
        assert_eq!(l.argv, s(&["claude"]));
        let _ = std::fs::remove_dir_all(&base);
    }
}
