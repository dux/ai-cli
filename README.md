# ai

One launcher for agent CLIs (claude, codex, grok, opencode, omp) with the same switches whichever one it is.
The agent runs inside a wrapper that confines it to the upper rows of the terminal and keeps a bar at the bottom showing your last prompts.

## Install

```sh
cargo install --path .
```

## Usage

```
ai [TOOL] [-c] [-f] [-b] [-a] [-l N] [-u] [-- args...]
ai -x [-a] [-l N] -- COMMAND [args...]
```

`TOOL` is a prefix of `claude`, `codex`, `grok`, `opencode` or `omp`; the first match wins.
So `c` is claude, `co` codex, `o` opencode and `om` omp.
No tool means claude.

| Switch | Meaning |
| --- | --- |
| `-c`, `--continue` | continue the last session in this directory |
| `-f`, `--full` | full permissions, no approval prompts |
| `-b`, `--bare` | do not inject the AGENTS.md chain |
| `-a`, `--awake` | block Mac sleep while the agent is working |
| `-l`, `--lines N` | prompt rows kept in the bar (1-20, default 3) |
| `-x`, `--exec` | wrap an arbitrary command instead of an agent |
| `-u`, `--update` | update the agent CLIs, then show model changes |
| `-h`, `--help` | full help |

Short switches combine: `-cf`, `-cfb`, `-l5`.
Anything after `--` is passed to the tool untouched.

```sh
ai
ai g -cf
ai cod -cb
ai c -- --model opus
ai -l 5 -a
ai -x -- htop
ai --update
```

## Switch mapping

Each generic switch becomes whatever the chosen CLI takes, e.g. `-c` is `claude -c` but `codex resume --last`, and `-f` is `--dangerously-skip-permissions` for claude.
See `./src/launch.rs` for the full table.

## AGENTS.md injection

By default every AGENTS.md from `~` down to the current directory is handed to the tool:

* claude: `--append-system-prompt`, the whole chain
* codex: `-c developer_instructions`, only the files above the git root
* grok: `--rules`, only the files above the git root
* opencode: inline config, only the files above the git root
* omp: nothing, it walks AGENTS.md itself

codex, grok and opencode already read AGENTS.md from the git root down, so they only get the part they would miss.
`-b` skips the injection.

## The wrapper

The child runs on its own PTY and its output is parsed into an emulated screen (alacritty_terminal) sized to the rows above the bar.
Only the renderer writes to the real terminal, so nothing the agent prints (clear screen, cursor moves, scroll regions, alternate screen) can reach the bar.
The bar is only redrawn when its own content changes.

* The bar shows your last prompts, rebuilt from keystrokes; history recall and autocomplete are not seen.
* A line starting with `!` never reaches the agent: it runs in `$SHELL` and the bar grows to show its output until you type again.
  `!!` sends a literal `!`, a bare `!` closes the output.
* F10 quits the wrapper and the agent (SIGHUP, so it can save its session).
* Keyboard protocol changes, cursor shape, scrollback clears, OSC notifications and synchronized output are forwarded to the real terminal.
* Terminal colours and kitty keyboard support are probed once at startup, so the child's queries are answered in order.
* The process renames itself to the agent's name, so pane watchers see `claude` or `omp` rather than `ai`.

## Herdr integration

* `--awake` holds `caffeinate` only while Herdr reports the pane as `working` (via `HERDR_PANE_ID` and `HERDR_SOCKET_PATH`, default `~/.config/herdr/herdr.sock`).
  Outside Herdr it is held for the whole session.
* The original command line is written to `$XDG_STATE_HOME/llm-wrap/<pid>` (default `~/.local/state/llm-wrap`) so Herdr's clone-tab can reopen the same wrapper.

## Update

`ai --update` runs each CLI's own updater (`claude/codex/grok/omp update`, `opencode upgrade`) and reports old -> new versions.
It then re-reads the model lists and shows what appeared or disappeared since the last run.
The model cache is `~/.cache/llm/models.json`.
A TOOL prefix limits the update to that one tool.

## Debugging

Set `AI_WRAP_DEBUG=<file>` to log keys and events.

## Development

```sh
cargo build
cargo test
```

`./tests/confine.rs` runs `ai -x` against a real terminal emulator (alacritty) and checks what the user would see.
