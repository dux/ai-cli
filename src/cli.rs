//! Command line: `ai [TOOL] [-c] [-f] [-b] [-a] [-u] [-l N] [-x] [-H] [-h] [-- extra...]`.

use std::path::{Path, PathBuf};

use crate::config::{self, Defaults};
use crate::launch;

pub const DEFAULT_KEEP: usize = 3;
const MAX_KEEP: usize = 20;

#[derive(Debug, PartialEq)]
pub enum Command {
    Help,
    Update { tools: Vec<String> },
    History { words: Vec<String> },
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
    /// Set for agents: the folder they started in, logged with each prompt.
    /// None for -x, which keeps no history and has no turns.
    pub agent_dir: Option<PathBuf>,
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
    history: bool,
    keep: Option<usize>,
}

pub fn parse_keep(v: &str) -> Result<usize, String> {
    v.parse::<usize>()
        .map(|n| n.clamp(1, MAX_KEEP))
        .map_err(|_| format!("--lines takes a number, got {v:?}"))
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    parse_in(args, &cwd, &home, config::defaults)
}

/// `defaults` is read only for a run: a broken config file must not stand in
/// the way of -h, -H or -u.
pub fn parse_in(
    args: &[String],
    cwd: &Path,
    home: &Path,
    defaults: impl FnOnce() -> Result<Defaults, String>,
) -> Result<Command, String> {
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
                "history" => f.history = true,
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
                    'H' => f.history = true,
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
    if f.history {
        if f.cont || f.full || f.bare || f.awake || f.update || f.exec || f.keep.is_some() {
            return Err("-H takes search words only".into());
        }
        return Ok(Command::History { words: positional.into_iter().chain(passthrough).collect() });
    }
    let prefix = positional.first().cloned();
    let unknown = || format!("unknown tool {:?} - one of {}", prefix.clone().unwrap_or_default(), launch::names().join(", "));
    if f.update && !f.exec {
        let tool = launch::tool(prefix.as_deref()).ok_or_else(unknown)?;
        let tools = if prefix.is_some() { vec![tool.to_string()] } else { launch::names() };
        return Ok(Command::Update { tools });
    }

    let d = defaults()?;
    let keep = f.keep.or(d.lines).unwrap_or(DEFAULT_KEEP);
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
        let opts = RunOpts { argv, env: vec![], agent_files: vec![], origin, keep, awake: f.awake, agent_dir: None };
        return Ok(Command::Run(opts));
    }

    let tool = launch::tool(prefix.as_deref().or(d.tool)).ok_or_else(unknown)?;

    f.full |= d.full;
    f.bare |= d.bare;
    f.awake |= d.awake;
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
    let agent_dir = Some(cwd.to_path_buf());
    Ok(Command::Run(RunOpts { argv: l.argv, env: l.env, agent_files: l.files, origin, keep, awake: f.awake, agent_dir }))
}

pub fn help() -> String {
    "\
Usage: ai [TOOL] [-c] [-f] [-b] [-a] [-l N] [-u] [-- args...]
       ai -x [-a] [-l N] -- COMMAND [args...]
       ai -H [WORDS...]

Start an agent CLI inside the ai wrapper, with the same switches whichever one it is.

TOOL is a prefix of claude, codex, grok, opencode or omp - first match wins, so `c` is
claude, `co` codex, `o` opencode and `om` omp. No tool means claude (or `tool` from
the config file below). The switches map to what each CLI actually takes:

  -c, --continue  continue the last session here  claude/grok/opencode/omp -c, codex resume --last
  -f, --full      full permissions, no prompts    --dangerously-skip-permissions, --dangerously-bypass-
                                                  approvals-and-sandbox, --always-approve, --auto,
                                                  --auto-approve
  -b, --bare      bare - no AGENTS.md injection   skips the default below
  -a, --awake     block Mac sleep while the agent is working
  -l, --lines N   prompt rows kept in the bar (1-20, default 3)
  -x, --exec      wrap an arbitrary command instead of an agent (no -c/-f/-b, no AGENTS.md)
  -u, --update    update the agent CLIs, then show model changes
  -H, --history   print past prompts holding every WORD (any case), newest last
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
While the agent works on a prompt the bar shows `working 1m12s`; the turn ends once
it has printed nothing for 3s. -a holds off sleep for exactly that (inside Herdr it
follows Herdr's own agent status instead).
AI_WRAP_DEBUG=<file> logs keys and events.

Files live in ~/.config/ai-cli (or $XDG_CONFIG_HOME/ai-cli):
  config       defaults for agent runs, `key = value` lines, # comments; the command
               line adds to them: tool = codex, full = true, bare = true,
               awake = true, lines = 5 (only lines applies to -x)
  history.txt  every prompt sent to an agent: `time : folder : prompt`, cut back to
               its newest half once past 1 MiB
  models.json  model lists from the last --update

Examples:
  ai
  ai g -cf
  ai cod -cb
  ai c -- --model opus
  ai -l 5 -a
  ai -x -- htop
  ai --update
  ai -H resize bar
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
        parse_in(&s(v), d, d, || Ok(Defaults::default()))
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
        assert!(r.agent_files.is_empty() && r.env.is_empty() && r.agent_dir.is_none());
        assert!(p(&["-x"]).is_err());
        assert!(p(&["-x", "--"]).is_err());
        assert!(p(&["-xc", "--", "ls"]).is_err());
        assert!(p(&["-x", "-f", "ls"]).is_err());
        assert!(p(&["-x", "--bare", "ls"]).is_err());
    }

    #[test]
    fn history_takes_words_only() {
        assert_eq!(p(&["-H", "fix", "bar"]).unwrap(), Command::History { words: s(&["fix", "bar"]) });
        assert_eq!(p(&["--history", "--", "-x"]).unwrap(), Command::History { words: s(&["-x"]) });
        assert_eq!(p(&["-H"]).unwrap(), Command::History { words: vec![] });
        assert!(p(&["-Hc"]).is_err());
        assert!(p(&["-H", "-l", "4"]).is_err());
    }

    #[test]
    fn config_defaults_sit_under_the_command_line() {
        let home = Path::new("/nonexistent-home");
        let d = || Ok(Defaults { tool: Some("codex"), full: true, bare: false, awake: true, lines: Some(6) });
        let run_d = |v: &[&str]| match parse_in(&s(v), home, home, d).unwrap() {
            Command::Run(r) => r,
            other => panic!("{other:?}"),
        };
        let r = run_d(&[]);
        assert_eq!(r.argv[..2], s(&["codex", "--dangerously-bypass-approvals-and-sandbox"]));
        assert_eq!((r.keep, r.awake), (6, true));
        let r = run_d(&["c", "-l2"]);
        assert_eq!(r.argv[0], "claude");
        assert_eq!(r.keep, 2);
        let r = run_d(&["-x", "--", "htop"]);
        assert_eq!(r.argv, s(&["htop"]));
        assert_eq!((r.keep, r.awake), (6, false), "-x takes only lines from the config");
        let broken = || Err("config:1: unknown key \"x\"".to_string());
        assert_eq!(parse_in(&s(&[]), home, home, broken).unwrap_err(), "config:1: unknown key \"x\"");
        assert_eq!(parse_in(&s(&["-H"]), home, home, broken).unwrap(), Command::History { words: vec![] });
        assert_eq!(parse_in(&s(&["-u"]), home, home, broken).unwrap(), Command::Update { tools: launch::names() });
    }
}
