//! Runs a ! line, here, instead of handing it to the agent.

use std::io::Read;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Component, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use regex::Regex;

use crate::pane::Pane;
use crate::text::scrub;

/// The pump is stopped while a ! line runs; anything slower wants a real
/// terminal.
const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_OUT: usize = 65_536;
const READ: usize = 4096;

/// `cd` on its own, with nothing that would need a shell to work out.
static CD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\Acd(?:(?-u:\s)+(?<path>[^|&;<>()`$\n]*\S))?\z").unwrap());

/// Where `cd -` goes back to.
static BACK: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Yours, non-interactive: ~/.zshrc is not read.
fn shell() -> String {
    std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into())
}

pub fn run(cmd: &str) -> Pane {
    let cmd = crate::text::strip(cmd);
    match CD.captures(cmd) {
        Some(m) => chdir(cmd, m.name("path").map(|p| p.as_str())),
        None => capture(cmd),
    }
}

fn chdir(cmd: &str, path: Option<&str>) -> Pane {
    let result = std::env::current_dir().and_then(|prev| {
        std::env::set_current_dir(target(path))?;
        *BACK.lock().unwrap() = Some(prev);
        Ok(())
    });
    match result {
        Ok(()) => Pane::new(cmd, Vec::new(), Some(0), None),
        Err(e) => Pane::new(cmd, vec![e.to_string()], Some(1), None),
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

fn target(path: Option<&str>) -> PathBuf {
    match path {
        None => home(),
        Some("-") => BACK
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
        Some(p) => expand(p),
    }
}

/// Ruby's File.expand_path: ~ and relative paths made absolute, `.`/`..`
/// resolved lexically.
fn expand(p: &str) -> PathBuf {
    let raw = if p == "~" {
        home()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home().join(rest)
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    let mut out = PathBuf::from("/");
    for c in raw.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(s) => out.push(s),
            _ => {}
        }
    }
    out
}

fn capture(cmd: &str) -> Pane {
    match spawn_and_read(cmd) {
        Ok((out, status, note)) => Pane::new(cmd, rows(&out), status, note),
        Err(e) => Pane::new(cmd, vec![e.to_string()], Some(1), Some("did not run".into())),
    }
}

type Captured = (Vec<u8>, Option<i32>, Option<String>);

fn spawn_and_read(cmd: &str) -> std::io::Result<Captured> {
    let (reader, writer) = nix::unistd::pipe()?;
    let err: OwnedFd = writer.try_clone()?;
    // No keyboard: our stdin is the terminal in raw mode.
    let mut child = Command::new(shell())
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(writer))
        .stderr(Stdio::from(err))
        .process_group(0)
        .spawn()?;
    // The Command (and its copies of the write end) is gone, so EOF can come.
    let mut file = std::fs::File::from(reader);
    let mut out = Vec::new();
    let mut note = None;
    let till = Instant::now() + TIMEOUT;
    let mut chunk = [0u8; READ];

    loop {
        // Waiting on the pipe, not the process: `something &` holds it open
        // until the deadline, and is then stopped like anything else.
        let left = till.saturating_duration_since(Instant::now());
        let ready = !left.is_zero() && {
            let ms = PollTimeout::try_from(left.as_millis().max(1) as i32).unwrap_or(PollTimeout::MAX);
            let mut fds = [PollFd::new(file.as_fd(), PollFlags::POLLIN)];
            loop {
                match poll(&mut fds, ms) {
                    Err(nix::errno::Errno::EINTR) => continue,
                    r => break r.map(|n| n > 0).unwrap_or(false),
                }
            }
        };
        if !ready {
            note = Some(format!("timed out after {}s", TIMEOUT.as_secs()));
            break;
        }
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
        if out.len() >= MAX_OUT {
            note = Some("output cut".into());
            break;
        }
    }

    if note.is_some() {
        stop(child.id());
    }
    drop(file);
    // Like Ruby's exitstatus: nothing when a signal ended it.
    let status = child.wait().ok().and_then(|s| s.code());
    Ok((out, status, note))
}

/// It does not get to outlive the line that started it: TERM, then KILL, to
/// the whole group.
fn stop(pid: u32) {
    let pg = Pid::from_raw(pid as i32);
    let _ = killpg(pg, Signal::SIGTERM);
    std::thread::sleep(Duration::from_millis(50));
    let _ = killpg(pg, Signal::SIGKILL);
}

static ESCAPES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b[\]P^_X].*?(?:\x07|\x1b\\)|\x1b.").unwrap()
});
static CONTROL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\x00-\x1f\x7f]").unwrap());

/// Output as bar rows: no escape sequences, tabs expanded, control bytes
/// gone, trailing blank rows dropped.
pub fn rows(bytes: &[u8]) -> Vec<String> {
    let text = scrub(bytes);
    let text = ESCAPES.replace_all(&text, "");
    let mut out: Vec<String> = text
        .split('\n')
        .map(|line| CONTROL.replace_all(&untab(line), "").into_owned())
        .collect();
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

fn untab(line: &str) -> String {
    if !line.contains('\t') {
        return line.to_string();
    }
    let mut out = String::new();
    let mut len = 0;
    for c in line.chars() {
        if c == '\t' {
            let n = 8 - len % 8;
            out.extend(std::iter::repeat_n(' ', n));
            len += n;
        } else {
            out.push(c);
            len += 1;
        }
    }
    out
}

/// Tests anywhere that read or move the process cwd take this.
#[cfg(test)]
pub static CWD_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn output_and_exit_status_are_kept() {
        let _g = lock();
        let p = run("echo hi");
        assert_eq!(p.out, ["hi"]);
        assert_eq!(p.status, Some(0));
    }

    #[test]
    fn stderr_is_output_too() {
        let _g = lock();
        assert_eq!(run("echo bad >&2").out, ["bad"]);
    }

    #[test]
    fn a_failure_keeps_its_status() {
        let _g = lock();
        assert_eq!(run("exit 3").status, Some(3));
    }

    #[test]
    fn a_command_gets_no_keyboard() {
        let _g = lock();
        assert!(run("cat").out.is_empty());
    }

    #[test]
    fn output_is_capped() {
        let _g = lock();
        let p = run("yes");
        assert_eq!(p.note.as_deref(), Some("output cut"));
        assert!(p.out.len() >= MAX_OUT / 2 - 1);
    }

    #[test]
    fn cd_moves_the_wrapper_and_nothing_else() {
        let _g = lock();
        let here = std::env::current_dir().unwrap();
        let dir = std::env::temp_dir().join(format!("ai-bang-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let real = std::fs::canonicalize(&dir).unwrap();
        assert_eq!(run(&format!("cd {}", dir.display())).status, Some(0));
        assert_eq!(std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap(), real);
        assert_eq!(run("cd -").pwd, here, "the pane says where it ran");
        assert_eq!(std::env::current_dir().unwrap(), here);
        std::env::set_current_dir(&here).unwrap();
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn a_bad_cd_is_reported_not_raised() {
        let _g = lock();
        let here = std::env::current_dir().unwrap();
        let p = run("cd /no/such/place/at/all");
        assert_eq!(p.status, Some(1));
        assert_eq!(std::env::current_dir().unwrap(), here);
    }

    #[test]
    fn cd_regex_leaves_shell_work_to_the_shell() {
        assert!(CD.is_match("cd"));
        assert!(CD.is_match("cd ~/x y"));
        assert!(!CD.is_match("cd x && ls"));
        assert!(!CD.is_match("cd $HOME"));
        assert!(!CD.is_match("cdx"));
    }

    #[test]
    fn expand_is_lexical() {
        let _g = lock();
        assert_eq!(expand("/a/b/../c/./d"), PathBuf::from("/a/c/d"));
        assert_eq!(expand("~"), home());
        assert_eq!(expand("~/x"), home().join("x"));
    }

    #[test]
    fn escape_sequences_and_control_bytes_are_stripped() {
        assert_eq!(rows(b"\x1b[31mred\x1b[0m\n"), ["red"]);
        assert_eq!(rows(b"a\x07b\n"), ["ab"]);
        assert_eq!(rows(b"\x1b]0;title\x07x\n"), ["x"]);
    }

    #[test]
    fn tabs_land_on_tab_stops() {
        assert_eq!(rows(b"a\tb\n"), ["a       b"]);
    }

    #[test]
    fn the_trailing_newline_is_not_a_blank_row() {
        assert_eq!(rows(b"a\nb\n"), ["a", "b"]);
        assert_eq!(rows(b"a\n\nb\n"), ["a", "", "b"]);
    }
}
