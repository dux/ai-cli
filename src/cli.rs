//! Command line: `ai [TOOL] [-c] [-f] [-b] [-a] [-u] [-l N] [-x] [-h] [-- extra...]`.

use std::path::{Path, PathBuf};

use crate::launch;

pub const DEFAULT_KEEP: usize = 3;
const MAX_KEEP: usize = 20;

#[derive(Debug, PartialEq)]
pub enum Command {
    Help,
    Update { tools: Vec<String> },
    Run(RunOpts),
}

#[derive(Debug, PartialEq)]
pub struct RunOpts {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub agent_files: Vec<PathBuf>,
    pub origin: Vec<String>,
    pub keep: usize,
    pub awake: bool,
}

#[derive(Default)]
struct Flags {
    cont: bool,
    full: bool,
    bare: bool,
    awake: bool,
    update: bool,
    exec: bool,
    help: bool,
    keep: Option<usize>,
}

fn parse_keep(v: &str) -> Result<usize, String> {
    v.parse::<usize>()
        .map(|n| n.clamp(1, MAX_KEEP))
        .map_err(|_| format!("--lines takes a number, got {v:?}"))
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    parse_in(args, &cwd, &home)
}

pub fn parse_in(args: &[String], cwd: &Path, home: &Path) -> Result<Command, String> {
    let mut f = Flags::default();
    let mut positional: Vec<String> = Vec::new();
    let mut passthrough: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--" {
            passthrough.extend(it.by_ref().cloned());
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            match long {
                "continue" => f.cont = true,
                "full" => f.full = true,
                "bare" => f.bare = true,
                "awake" => f.awake = true,
                "update" => f.update = true,
                "exec" => f.exec = true,
                "help" => f.help = true,
                "lines" => {
                    let v = it.next().ok_or("--lines needs a value")?;
                    f.keep = Some(parse_keep(v)?);
                }
                _ => match long.strip_prefix("lines=") {
                    Some(v) => f.keep = Some(parse_keep(v)?),
                    None => return Err(format!("unknown option {arg:?}")),
                },
            }
        } else if arg.len() > 1 && arg.starts_with('-') {
            let body = &arg[1..];
            for (i, c) in body.char_indices() {
                match c {
                    'c' => f.cont = true,
                    'f' => f.full = true,
                    'b' => f.bare = true,
                    'a' => f.awake = true,
                    'u' => f.update = true,
                    'x' => f.exec = true,
                    'h' => f.help = true,
                    'l' => {
                        let rest = &body[i + 1..];
                        let v = if rest.is_empty() {
                            it.next().ok_or("-l needs a value")?.as_str()
                        } else {
                            rest
                        };
                        f.keep = Some(parse_keep(v)?);
                        break;
                    }
                    _ => return Err(format!("unknown option \"-{c}\"")),
                }
            }
        } else {
            positional.push(arg.clone());
        }
    }

    if f.help {
        return Ok(Command::Help);
    }
    let keep = f.keep.unwrap_or(DEFAULT_KEEP);
    let lines_origin = |flag: &str| -> Vec<String> {
        if keep == DEFAULT_KEEP { vec![] } else { vec![flag.to_string(), keep.to_string()] }
    };

    if f.exec {
        if f.cont || f.full || f.bare {
            return Err("-x wraps a plain command; -c, -f and -b only apply to agents".into());
        }
        if f.update {
            return Err("-x and -u do not combine".into());
        }
        let argv: Vec<String> = positional.into_iter().chain(passthrough).collect();
        if argv.is_empty() {
            return Err("-x needs a command, e.g. ai -x -- htop".into());
        }
        let mut origin = vec!["ai".to_string(), "-x".to_string()];
        if f.awake {
            origin.push("-a".into());
        }
        origin.extend(lines_origin("-l"));
        origin.push("--".into());
        origin.extend(argv.iter().cloned());
        return Ok(Command::Run(RunOpts { argv, env: vec![], agent_files: vec![], origin, keep, awake: f.awake }));
    }

    let prefix = positional.first().cloned();
    let tool = launch::tool(prefix.as_deref()).ok_or_else(|| {
        format!("unknown tool {:?} - one of {}", prefix.clone().unwrap_or_default(), launch::names().join(", "))
    })?;
    if f.update {
        let tools = if prefix.is_some() { vec![tool.to_string()] } else { launch::names() };
        return Ok(Command::Update { tools });
    }

    let extra: Vec<String> = positional.into_iter().skip(usize::from(prefix.is_some())).chain(passthrough).collect();
    let sw = launch::Switches { cont: f.cont, full: f.full, bare: f.bare };
    let l = launch::build(tool, &extra, &sw, cwd, home);

    let mut origin = vec!["ai".to_string(), tool.to_string()];
    for (on, name) in [(f.cont, "--continue"), (f.full, "--full"), (f.bare, "--bare"), (f.awake, "--awake")] {
        if on {
            origin.push(name.into());
        }
    }
    origin.extend(lines_origin("--lines"));
    origin.push("--".into());
    origin.extend(extra);
    Ok(Command::Run(RunOpts { argv: l.argv, env: l.env, agent_files: l.files, origin, keep, awake: f.awake }))
}

pub fn help() -> String {
    "\
Usage: ai [TOOL] [-c] [-f] [-b] [-a] [-l N] [-u] [-- args...]
       ai -x [-a] [-l N] -- COMMAND [args...]

Start an agent CLI inside the ai wrapper, with the same switches whichever one it is.

TOOL is a prefix of claude, codex, grok, opencode or omp - first match wins, so `c` is
claude, `co` codex, `o` opencode and `om` omp. No tool means claude. The switches map
to what each CLI actually takes:

  -c, --continue  continue the last session here  claude/grok/opencode/omp -c, codex resume --last
  -f, --full      full permissions, no prompts    --dangerously-skip-permissions, --dangerously-bypass-
                                                  approvals-and-sandbox, --always-approve, --auto,
                                                  --auto-approve
  -b, --bare      bare - no AGENTS.md injection   skips the default below
  -a, --awake     block Mac sleep while the agent is working
  -l, --lines N   prompt rows kept in the bar (1-20, default 3)
  -x, --exec      wrap an arbitrary command instead of an agent (no -c/-f/-b, no AGENTS.md)
  -u, --update    update the agent CLIs, then show model changes
  -h, --help      this text

Short switches combine: -cf, -cfb, -l5.

--update runs each CLI's own updater (claude/codex/grok/omp update, opencode upgrade),
reports old -> new, then re-reads the model lists and shows what appeared or
disappeared since the last run. A TOOL prefix scopes it to that one tool; no prefix
updates all five.

By default every AGENTS.md from ~ down to cwd is handed to the tool: claude
--append-system-prompt, codex -c developer_instructions, grok --rules, opencode
inline config. codex, grok and opencode read AGENTS.md from the git root down by
themselves, so they only get the files above the repo; claude gets the whole chain.
omp walks AGENTS.md from cwd up to ~ by itself, so it gets nothing added.
Anything after `--` is handed to the tool untouched.

The wrapper: the agent runs inside an emulated screen confined to the rows above a
bar at the bottom of the terminal. Nothing the agent prints can reach the bar; the
bar only changes when its own content does. It shows your last prompts, rebuilt from
keystrokes (history recall and autocomplete are not seen). A line starting with !
never reaches the agent: it runs in $SHELL inside the wrapper and the bar grows to
show its output until you type again (!! sends a literal !, a bare ! closes it).
F10 quits the wrapper and the agent with it (SIGHUP, so it can save its session).
AI_WRAP_DEBUG=<file> logs keys and events.

Examples:
  ai
  ai g -cf
  ai cod -cb
  ai c -- --model opus
  ai -l 5 -a
  ai -x -- htop
  ai --update
"
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn p(v: &[&str]) -> Result<Command, String> {
        let d = Path::new("/nonexistent-home");
        parse_in(&s(v), d, d)
    }

    fn run(v: &[&str]) -> RunOpts {
        match p(v).unwrap() {
            Command::Run(r) => r,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn default_is_claude() {
        let r = run(&[]);
        assert_eq!(r.argv, s(&["claude"]));
        assert_eq!(r.origin, s(&["ai", "claude", "--"]));
        assert_eq!(r.keep, 3);
        assert!(!r.awake);
    }

    #[test]
    fn combined_short_flags() {
        let r = run(&["g", "-cf"]);
        assert_eq!(r.argv, s(&["grok", "-c", "--always-approve"]));
        assert_eq!(r.origin, s(&["ai", "grok", "--continue", "--full", "--"]));
        let r = run(&["cod", "-cfba"]);
        assert_eq!(r.argv, s(&["codex", "resume", "--last", "--dangerously-bypass-approvals-and-sandbox"]));
        assert_eq!(r.origin, s(&["ai", "codex", "--continue", "--full", "--bare", "--awake", "--"]));
        assert!(r.awake);
    }

    #[test]
    fn flags_anywhere_before_dashdash() {
        let r = run(&["-c", "om", "-f"]);
        assert_eq!(r.argv, s(&["omp", "-c", "--auto-approve"]));
    }

    #[test]
    fn lines_forms() {
        assert_eq!(run(&["-l5"]).keep, 5);
        assert_eq!(run(&["-l", "7"]).keep, 7);
        assert_eq!(run(&["--lines", "2"]).keep, 2);
        assert_eq!(run(&["--lines=9"]).keep, 9);
        assert_eq!(run(&["-cl4"]).keep, 4);
        assert_eq!(run(&["-l", "0"]).keep, 1);
        assert_eq!(run(&["-l99"]).keep, 20);
        assert_eq!(run(&["-l5"]).origin, s(&["ai", "claude", "--lines", "5", "--"]));
        assert!(p(&["-l"]).is_err());
        assert!(p(&["-lx"]).is_err());
    }

    #[test]
    fn passthrough() {
        let r = run(&["c", "-b", "--", "--model", "opus", "-c"]);
        assert_eq!(r.argv, s(&["claude", "--model", "opus", "-c"]));
        assert_eq!(r.origin, s(&["ai", "claude", "--bare", "--", "--model", "opus", "-c"]));
        let r = run(&["c", "foo", "-b", "--", "bar"]);
        assert_eq!(r.argv, s(&["claude", "foo", "bar"]));
    }

    #[test]
    fn errors() {
        assert_eq!(p(&["x"]).unwrap_err(), "unknown tool \"x\" - one of claude, codex, grok, opencode, omp");
        assert!(p(&["-z"]).is_err());
        assert!(p(&["--nope"]).is_err());
        assert!(p(&["-cz"]).is_err());
    }

    #[test]
    fn help_and_update() {
        assert_eq!(p(&["-h"]).unwrap(), Command::Help);
        assert_eq!(p(&["--help", "x"]).unwrap(), Command::Help);
        assert_eq!(p(&["-u"]).unwrap(), Command::Update { tools: launch::names() });
        assert_eq!(p(&["o", "--update"]).unwrap(), Command::Update { tools: s(&["opencode"]) });
        assert!(p(&["q", "-u"]).is_err());
    }

    #[test]
    fn exec_rules() {
        let r = run(&["-x", "--", "htop", "-d", "5"]);
        assert_eq!(r.argv, s(&["htop", "-d", "5"]));
        assert_eq!(r.origin, s(&["ai", "-x", "--", "htop", "-d", "5"]));
        let r = run(&["-xal", "6", "vim", "--", "-c"]);
        assert_eq!(r.argv, s(&["vim", "-c"]));
        assert_eq!(r.origin, s(&["ai", "-x", "-a", "-l", "6", "--", "vim", "-c"]));
        assert!(r.agent_files.is_empty() && r.env.is_empty());
        assert!(p(&["-x"]).is_err());
        assert!(p(&["-x", "--"]).is_err());
        assert!(p(&["-xc", "--", "ls"]).is_err());
        assert!(p(&["-x", "-f", "ls"]).is_err());
        assert!(p(&["-x", "--bare", "ls"]).is_err());
    }
}
