//! Taking the child's name costs us our own command line, which Herdr's
//! clone-tab reads back to reopen the same wrapper. So leave it in a file named
//! after our pid, one argument per line; it goes away when we do.

use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;

/// Read at call time: `$XDG_STATE_HOME/llm-wrap`, or `~/.local/state/llm-wrap`.
/// The name is shared with herdr clone-tab; keep it.
pub fn dir() -> PathBuf {
    let state = std::env::var("XDG_STATE_HOME").unwrap_or_default();
    let state = if state.is_empty() {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state")
    } else {
        PathBuf::from(state)
    };
    state.join("llm-wrap")
}

fn path_in(dir: &Path) -> PathBuf {
    dir.join(std::process::id().to_string())
}

pub fn write(origin: &[String]) {
    write_in(&dir(), origin);
}

pub fn clear() {
    clear_in(&dir());
}

fn write_in(dir: &Path, origin: &[String]) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    sweep_in(dir);
    let _ = std::fs::write(path_in(dir), origin.join("\n"));
}

fn clear_in(dir: &Path) {
    let _ = std::fs::remove_file(path_in(dir));
}

/// A crash leaves its file behind and pids come round again: drop entries
/// whose process is gone. EPERM means alive and not ours.
fn sweep_in(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.parse::<i32>().ok()).filter(|p| *p > 0) else {
            continue;
        };
        if kill(Pid::from_raw(pid), None) == Err(Errno::ESRCH) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ai-handoff-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.join("llm-wrap")
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn writes_one_argument_per_line_named_after_our_pid() {
        let d = tmp("write");
        write_in(&d, &args(&["llm", "wrap", "--lines", "3", "--", "claude"]));
        let p = path_in(&d);
        assert_eq!(p.file_name().unwrap().to_str().unwrap(), std::process::id().to_string());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "llm\nwrap\n--lines\n3\n--\nclaude");
        clear_in(&d);
    }

    #[test]
    fn clear_removes_it_and_is_fine_when_already_gone() {
        let d = tmp("clear");
        write_in(&d, &args(&["llm", "wrap", "claude"]));
        clear_in(&d);
        assert!(!path_in(&d).exists());
        clear_in(&d);
    }

    #[test]
    fn sweep_drops_entries_whose_process_is_gone() {
        let d = tmp("sweep");
        let mut c = std::process::Command::new("true").spawn().unwrap();
        let dead = c.id();
        c.wait().unwrap();
        std::fs::create_dir_all(&d).unwrap();
        let dead_p = d.join(dead.to_string());
        let live = path_in(&d);
        std::fs::write(&dead_p, "llm\nwrap\nclaude").unwrap();
        std::fs::write(&live, "llm\nwrap\nclaude").unwrap();
        sweep_in(&d);
        assert!(!dead_p.exists(), "a stale pid can be reused by anyone");
        assert!(live.exists());
        clear_in(&d);
    }

    #[test]
    fn sweep_ignores_files_that_are_not_pids() {
        let d = tmp("junk");
        std::fs::create_dir_all(&d).unwrap();
        let junk = d.join("README");
        std::fs::write(&junk, "not a pid").unwrap();
        sweep_in(&d);
        assert!(junk.exists());
    }

    #[test]
    fn dir_follows_xdg_state_home_name() {
        assert!(dir().ends_with("llm-wrap"));
    }
}
