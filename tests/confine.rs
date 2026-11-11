//! End to end: `ai -x` runs inside a PTY whose other side is a real terminal
//! emulator (alacritty's), standing in for the user's terminal. What that
//! emulator ends up showing is what the user would see.

use std::cell::RefCell;
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Config;
use alacritty_terminal::vte::ansi::{Processor, Rgb};
use nix::pty::{Winsize, openpty};

const ROWS: u16 = 24;
const COLS: u16 = 80;
/// How long a test waits for something it expects to see.
const PATIENCE: Duration = Duration::from_secs(10);

#[derive(Clone, Default)]
struct Events(Rc<RefCell<Vec<Event>>>);

impl EventListener for Events {
    fn send_event(&self, e: Event) {
        self.0.borrow_mut().push(e);
    }
}

struct Dims(usize, usize);

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.0
    }
    fn screen_lines(&self) -> usize {
        self.0
    }
    fn columns(&self) -> usize {
        self.1
    }
}

struct Screen {
    term: Term<Events>,
    events: Events,
    proc: Processor,
    master: OwnedFd,
    child: Child,
}

impl Screen {
    fn start(args: &[&str]) -> Screen {
        let ws = Winsize { ws_row: ROWS, ws_col: COLS, ws_xpixel: 0, ws_ypixel: 0 };
        let pty = openpty(Some(&ws), None).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ai"));
        cmd.args(args)
            .env("HOME", std::env::temp_dir())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("HERDR_PANE_ID")
            .stdin(Stdio::from(pty.slave.try_clone().unwrap()))
            .stdout(Stdio::from(pty.slave.try_clone().unwrap()))
            .stderr(Stdio::from(pty.slave.try_clone().unwrap()));
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        drop(pty.slave);
        let fd = pty.master.as_raw_fd();
        unsafe { libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK) };
        let events = Events::default();
        let config = Config { kitty_keyboard: true, ..Config::default() };
        let term = Term::new(config, &Dims(ROWS as usize, COLS as usize), events.clone());
        Screen { term, events, proc: Processor::new(), master: pty.master, child }
    }

    /// Runs the terminal side for `dur`, calling `check` after every chunk
    /// of output it shows.
    fn pump(&mut self, dur: Duration, mut check: impl FnMut(&Term<Events>)) {
        let till = Instant::now() + dur;
        let mut buf = [0u8; 65536];
        while Instant::now() < till {
            let n = unsafe { libc::read(self.master.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n > 0 {
                // Feed in small pieces: a check between any two of them is a
                // moment the user could have seen.
                for piece in buf[..n as usize].chunks(64) {
                    self.proc.advance(&mut self.term, piece);
                    self.answer();
                    check(&self.term);
                }
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn answer(&mut self) {
        let events: Vec<Event> = std::mem::take(&mut *self.events.0.borrow_mut());
        for e in events {
            let reply = match e {
                Event::PtyWrite(s) => s,
                Event::ColorRequest(_, f) => f(Rgb { r: 0x10, g: 0x20, b: 0x30 }),
                _ => continue,
            };
            self.type_in(reply.as_bytes());
        }
    }

    fn type_in(&self, bytes: &[u8]) {
        let mut f = std::fs::File::from(self.master.try_clone().unwrap());
        f.write_all(bytes).unwrap();
    }

    /// The user's terminal changes size: it resizes its own screen, which
    /// resets its scroll region, then tells the PTY.
    fn resize(&mut self, rows: u16) {
        self.term.resize(Dims(rows as usize, COLS as usize));
        let ws = libc::winsize { ws_row: rows, ws_col: COLS, ws_xpixel: 0, ws_ypixel: 0 };
        unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
    }

    fn row(&self, line: i32) -> String {
        row(&self.term, line)
    }

    fn bar(&self) -> Vec<String> {
        bar(&self.term)
    }

    /// The child's rows that hold text.
    fn shown(&self) -> Vec<String> {
        (0..ROWS as i32 - 4).map(|l| self.row(l)).filter(|l| !l.is_empty()).collect()
    }

    /// Pumps until `done` holds, giving up after `PATIENCE`. A fixed wait
    /// fails on a busy machine and on the first run of a fresh build.
    fn wait(&mut self, done: impl Fn(&Screen) -> bool) -> bool {
        let till = Instant::now() + PATIENCE;
        while !done(self) && Instant::now() < till {
            self.pump(Duration::from_millis(20), |_| {});
        }
        done(self)
    }

    /// The bar is up and the child has set its terminal mode and said READY.
    fn ready(&mut self) {
        let up = self.wait(|s| s.bar()[0].contains("prompt history") && s.shown().contains(&"READY".to_string()));
        assert!(up, "never ready: {:?} {:?}", self.shown(), self.bar());
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn row(term: &Term<Events>, line: i32) -> String {
    let row = &term.grid()[Line(line)];
    let s: String = (0..term.columns()).map(|c| row[Column(c)].c).collect();
    s.trim_end().to_string()
}

/// The bottom four rows: a rule and three prompt rows with the default -l 3.
fn bar(term: &Term<Events>) -> Vec<String> {
    (ROWS as i32 - 4..ROWS as i32).map(|l| row(term, l)).collect()
}

fn sh(script: &str) -> Vec<String> {
    ["-x", "--", "sh", "-c", script].iter().map(|s| s.to_string()).collect()
}

#[test]
fn nothing_the_child_writes_reaches_the_bar() {
    // Everything a full-screen program can do to rows it does not own.
    let script = r#"
        printf 'hello\n'; sleep 0.4
        printf '\033[2J\033[Hafter clear'; sleep 0.2
        printf '\033[3;1H\033[J'; sleep 0.2
        printf '\033[999;1HBOTTOM'; sleep 0.2
        printf '\033[r\033[24;1H\n\n\nSCROLLED'; sleep 0.2
        printf '\0337\033[23;1HSAVED\0338'; sleep 0.2
        printf '\033[s\033[24;1HSCO\033[u'; sleep 0.2
        printf '\033[?1049h\033[2J\033[HALT\033[30;1HALTBOTTOM'; sleep 0.3
        printf '\033[?1049l'; sleep 0.2
        printf '\033[1;30r\033[30;1H\n\nREGION'; sleep 0.2
        printf '\033[?2048h\033[H\033[2J\033[3J\033[24;1HPROMPT'; sleep 2
    "#;
    let args = sh(script);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut s = Screen::start(&args);

    let mut settled: Option<Vec<String>> = None;
    let mut seen_bottom = false;
    let mut check = |term: &Term<Events>| {
        let bar = bar(term);
        if row(term, 19) == "BOTTOM" {
            seen_bottom = true;
        }
        match &settled {
            None if bar[0].contains("prompt history") && bar[3].contains("nothing typed yet") => settled = Some(bar),
            None => {}
            Some(want) => assert_eq!(&bar, want, "the bar moved"),
        }
    };
    let started = Instant::now();
    while s.row(19) != "PROMPT" && started.elapsed() < PATIENCE {
        s.pump(Duration::from_millis(50), &mut check);
    }
    s.pump(Duration::from_millis(300), &mut check);

    let bar = settled.expect("the bar was painted");
    assert!(bar[0].contains("prompt history"), "{bar:?}");
    assert!(seen_bottom, "an absolute move past the last row lands on the child's own last row");
    assert_eq!(s.row(19), "PROMPT", "the child still draws its own last row");
}

#[test]
fn scrolled_lines_reach_the_real_scrollback_in_order() {
    let args = sh("seq 1 300; sleep 2");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut s = Screen::start(&args);
    s.wait(|s| s.row(18) == "300");

    let grid = s.term.grid();
    let top = -(grid.history_size() as i32);
    let lines: Vec<String> = (top..ROWS as i32 - 4).map(|l| s.row(l)).filter(|l| !l.is_empty()).collect();
    let numbers: Vec<u32> = lines.iter().filter_map(|l| l.parse().ok()).collect();
    assert_eq!(numbers, (1..=300).collect::<Vec<_>>());
}

#[test]
fn scrollback_survives_a_resize_that_ends_at_the_old_size() {
    // Waking from sleep or leaving a fullscreen game: the terminal goes
    // through another size and back before the wrapper reads it.
    let args = sh("stty -echo -icanon; echo READY; read l; seq 1 100; sleep 2");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut s = Screen::start(&args);
    s.ready();
    s.resize(ROWS + 6);
    s.resize(ROWS);
    s.pump(Duration::from_millis(200), |_| {});
    s.type_in(b"\r");
    s.wait(|s| s.row(18) == "100");

    let top = -(s.term.grid().history_size() as i32);
    let lines: Vec<String> = (top..ROWS as i32 - 4).map(|l| s.row(l)).filter(|l| !l.is_empty()).collect();
    assert!(lines.iter().all(|l| !l.contains("prompt history")), "the bar was scrolled into history: {lines:?}");
    let numbers: Vec<u32> = lines.iter().filter_map(|l| l.parse().ok()).collect();
    assert_eq!(numbers, (1..=100).collect::<Vec<_>>());
    assert!(s.bar()[0].contains("prompt history"), "bar kept: {:?}", s.bar());
}

#[test]
fn scrollback_replayed_on_leaving_the_alt_screen_is_kept() {
    // omp on resize: a moment in the alt screen, then one write that leaves
    // it, clears screen and scrollback and replays the whole transcript.
    let replay: String = (101..=150).map(|i| format!("{i}\\n")).collect();
    let script = format!(
        "seq 1 50; printf '\\033[?1049hALT'; sleep 0.4; printf '\\033[?1049l\\033[H\\033[2J\\033[3J{replay}'; sleep 2"
    );
    let args = sh(&script);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut s = Screen::start(&args);
    s.wait(|s| s.row(18) == "150");
    s.pump(Duration::from_millis(200), |_| {});

    let top = -(s.term.grid().history_size() as i32);
    let lines: Vec<String> = (top..ROWS as i32 - 4).map(|l| s.row(l)).filter(|l| !l.is_empty()).collect();
    let numbers: Vec<u32> = lines.iter().filter_map(|l| l.parse().ok()).collect();
    assert_eq!(numbers, (101..=150).collect::<Vec<_>>(), "{lines:?}");
}

#[test]
fn typed_prompt_is_pinned_and_reaches_the_child() {
    let args = sh("stty -echo -icanon; echo READY; while read l; do printf 'got:%s\\n' \"$l\"; done");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut s = Screen::start(&args);
    s.ready();
    s.type_in(b"hello world\r");
    s.wait(|s| s.bar()[3].contains("hello world") && s.shown().contains(&"got:hello world".to_string()));

    assert!(s.bar()[3].contains("hello world"), "{:?}", s.bar());
    let screen: Vec<String> = (0..20).map(|l| s.row(l)).collect();
    assert!(screen.iter().any(|l| l == "got:hello world"), "{screen:?}");
}

#[test]
fn bang_line_runs_here_and_never_reaches_the_child() {
    let args = sh("stty -echo -icanon; echo READY; while read l; do printf 'got:%s\\n' \"$l\"; done");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut s = Screen::start(&args);
    s.ready();
    s.type_in(b"!echo from-shell\r");
    s.wait(|s| (0..ROWS as i32).any(|l| s.row(l) == "from-shell"));
    s.pump(Duration::from_millis(300), |_| {});

    let bottom: Vec<String> = (0..ROWS as i32).map(|l| s.row(l)).collect();
    assert!(bottom.iter().any(|l| l == "from-shell"), "{bottom:?}");
    assert!(bottom.iter().any(|l| l.contains("shell")), "{bottom:?}");
    assert!(!bottom.iter().any(|l| l.starts_with("got:")), "the child saw the ! line: {bottom:?}");
}

#[test]
fn mouse_clicks_on_the_bar_never_reach_the_child() {
    let args = sh("stty raw -echo; printf 'READY\\r\\n'; dd bs=1 count=10 2>/dev/null | od -c; sleep 2");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut s = Screen::start(&args);
    s.ready();
    // Row 22 is a bar row, row 5 is the child's.
    s.type_in(b"\x1b[<0;3;22M\x1b[<0;3;5Mz");
    s.wait(|s| s.shown().iter().any(|l| l.contains("5   M")));
    let screen: String = (0..20).map(|l| s.row(l)).collect::<Vec<_>>().join("\n");
    assert!(screen.contains("5   M"), "the child row click arrives: {screen}");
    assert!(!screen.contains("2   2"), "the bar click must not: {screen}");
}

#[test]
fn f10_quits_the_wrapper_and_the_agent_and_clears_the_bar() {
    let args = sh("trap 'echo got-hup; exit 7' HUP; stty raw -echo; printf 'READY\\r\\n'; while :; do sleep 0.05; done");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut s = Screen::start(&args);
    s.ready();

    let pressed = Instant::now();
    s.type_in(b"\x1b[21~");
    let mut status = None;
    while status.is_none() && pressed.elapsed() < Duration::from_secs(3) {
        s.pump(Duration::from_millis(50), |_| {});
        status = s.child.try_wait().unwrap();
    }
    s.pump(Duration::from_millis(100), |_| {});

    let status = status.expect("ai exits on F10");
    assert_eq!(status.code(), Some(7), "the agent got SIGHUP and its exit code is ours");
    assert!(pressed.elapsed() < Duration::from_secs(2));
    assert!(s.bar().iter().all(|r| !r.contains("prompt history")), "bar erased: {:?}", s.bar());
}

#[test]
fn sigurg_clears_screen_and_scrollback_but_keeps_the_bar() {
    let args = sh("seq 1 100; sleep 1.2; printf AFTER; sleep 2");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut s = Screen::start(&args);
    s.wait(|s| s.row(18) == "100");
    assert!(s.term.grid().history_size() > 0, "seq reached the real scrollback");
    assert_eq!(s.row(18), "100", "the last line, above the cursor row");

    unsafe { libc::kill(s.child.id() as i32, libc::SIGURG) };
    s.wait(|s| s.term.grid().history_size() == 0 && s.shown().is_empty());
    assert_eq!(s.term.grid().history_size(), 0, "real scrollback cleared");
    let screen: Vec<String> = (0..20).map(|l| s.row(l)).collect();
    assert!(screen.iter().all(String::is_empty), "child rows cleared: {screen:?}");
    assert!(s.bar()[0].contains("prompt history"), "bar kept: {:?}", s.bar());

    s.wait(|s| !s.shown().is_empty());
    assert_eq!(s.shown(), ["AFTER"], "only new output");
}
