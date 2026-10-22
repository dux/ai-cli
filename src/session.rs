//! One wrapped run: the child on its own PTY, its output parsed into an
//! emulated screen exactly the size it was told it has, and that screen
//! painted into the rows above the bar.
//!
//! Nothing the child writes reaches the real terminal directly. That is what
//! keeps it inside its rows: there is no escape sequence it can send - a
//! clear screen, an absolute cursor move past its last row, a scroll region,
//! a saved cursor, the alternate screen - that touches a bar row, because the
//! bar rows are never addressed by anything but the renderer.

use std::cell::RefCell;
use std::fs::File;
use std::io::{self, Write as _};
use std::os::fd::{AsFd, AsRawFd};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{Processor, Timeout};

use crate::awake::Awake;
use crate::cli::RunOpts;
use crate::keys::{self, KeyBuffer};
use crate::pane::Pane;
use crate::probe::{self, Outer};
use crate::render::Renderer;
use crate::scan::Scan;
use crate::term::{self, RawMode, Size};
use crate::{bang, handoff};

/// The child keeps at least this many rows whatever the bar shows.
const MIN_CHILD: usize = 5;
/// After output, wait this long for more before painting...
const SETTLE: Duration = Duration::from_millis(3);
/// ...but never let a frame be older than this while output streams.
const FRAME: Duration = Duration::from_millis(16);
const READ_MAX: usize = 1 << 20;
const HISTORY: usize = 20_000;
/// How long F10 waits for the agent to go after SIGHUP before killing it.
const QUIT_GRACE: Duration = Duration::from_secs(1);

#[derive(Clone, Default)]
struct Events(Rc<RefCell<Vec<Event>>>);

impl EventListener for Events {
    fn send_event(&self, event: Event) {
        self.0.borrow_mut().push(event);
    }
}

/// The emulator buffers nothing for synchronized output; Scan holds the
/// paint back instead, so the screen and everything read from the stream
/// next to it always agree on where they are.
#[derive(Default)]
struct NoSync;

impl Timeout for NoSync {
    fn set_timeout(&mut self, _: Duration) {}
    fn clear_timeout(&mut self) {}
    fn pending_timeout(&self) -> bool {
        false
    }
}

struct Dims {
    lines: usize,
    cols: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Input modes the real terminal must mirror for keys, mouse and paste to
/// arrive the way the child asked for them.
const FORWARD_MODES: &[(TermMode, &str, &str)] = &[
    (TermMode::APP_CURSOR, "\x1b[?1h", "\x1b[?1l"),
    (TermMode::APP_KEYPAD, "\x1b=", "\x1b>"),
    (TermMode::MOUSE_REPORT_CLICK, "\x1b[?1000h", "\x1b[?1000l"),
    (TermMode::MOUSE_DRAG, "\x1b[?1002h", "\x1b[?1002l"),
    (TermMode::MOUSE_MOTION, "\x1b[?1003h", "\x1b[?1003l"),
    (TermMode::UTF8_MOUSE, "\x1b[?1005h", "\x1b[?1005l"),
    (TermMode::SGR_MOUSE, "\x1b[?1006h", "\x1b[?1006l"),
    (TermMode::FOCUS_IN_OUT, "\x1b[?1004h", "\x1b[?1004l"),
    (TermMode::BRACKETED_PASTE, "\x1b[?2004h", "\x1b[?2004l"),
];

pub fn run(opts: RunOpts) -> i32 {
    let bare = if opts.awake { Awake::wrap(&opts.argv) } else { opts.argv.clone() };
    let stdin = io::stdin().as_raw_fd();
    let stdout = io::stdout().as_raw_fd();
    let size = if term::is_tty(stdin) && term::is_tty(stdout) { term::size_of(stdout) } else { None };
    let Some(size) = size.filter(|s| s.rows as usize >= opts.keep + 1 + MIN_CHILD) else {
        // No terminal to draw on, or too small for a bar: get out of the way.
        let err = term::exec(&bare, &opts.env);
        eprintln!("ai: {}: {err}", opts.argv[0]);
        return 127;
    };

    handoff::write(&opts.origin);
    let code = match Session::start(&opts, size) {
        Ok(mut s) => {
            let code = s.run();
            s.finish();
            code
        }
        Err(e) => {
            eprintln!("ai: {}: {e}", opts.argv[0]);
            127
        }
    };
    handoff::clear();
    code
}

struct Session {
    raw: Option<RawMode>,
    pty: term::Pty,
    term: Term<Events>,
    events: Events,
    proc: Processor<NoSync>,
    scan: Scan,
    render: Renderer,
    keys: KeyBuffer,
    pane: Option<Pane>,
    bang_id: u64,
    bang_tx: mpsc::Sender<(u64, Pane)>,
    bang_rx: mpsc::Receiver<(u64, Pane)>,
    awake: Option<Awake>,
    awake_rx: Option<mpsc::Receiver<bool>>,
    wake: term::Wake,
    outer: Outer,
    size: Size,
    modes: TermMode,
    to_child: Vec<u8>,
    dirty: bool,
    last_output: Instant,
    last_frame: Instant,
    exit: Option<i32>,
    trace: Option<File>,
    started: Instant,
}

impl Session {
    fn start(opts: &RunOpts, size: Size) -> io::Result<Session> {
        let wake = term::Wake::install()?;
        let raw = RawMode::enter()?;
        let (outer, typed) = probe::run();

        let bar_rows = opts.keep + 1;
        let child_rows = size.rows as usize - bar_rows;
        let pty = term::spawn(&opts.argv, &opts.env, size.with_rows(child_rows as u16), Some(raw.saved()))?;

        let events = Events::default();
        let config = Config { scrolling_history: HISTORY, kitty_keyboard: false, ..Config::default() };
        let term = Term::new(config, &Dims { lines: child_rows, cols: size.cols as usize }, events.clone());

        let mut keys = KeyBuffer::new(opts.keep);
        keys.set_child_rows(child_rows as u16);

        let (awake, awake_rx) = if opts.awake {
            let (tx, rx) = mpsc::channel();
            let a = Awake::start(Box::new(move |on| {
                let _ = tx.send(on);
                term::wake(term::WAKE_THREAD);
            }));
            (Some(a), Some(rx))
        } else {
            (None, None)
        };

        let trace = std::env::var_os("AI_WRAP_DEBUG")
            .filter(|p| !p.is_empty())
            .and_then(|p| File::options().create(true).append(true).open(p).ok());
        let (bang_tx, bang_rx) = mpsc::channel();

        let s = Session {
            raw: Some(raw),
            pty,
            term,
            events,
            proc: Processor::new(),
            scan: Scan::new(outer.kitty),
            render: Renderer::new(size.rows as usize, size.cols as usize, child_rows),
            keys,
            pane: None,
            bang_id: 0,
            bang_tx,
            bang_rx,
            awake,
            awake_rx,
            wake,
            outer,
            size,
            modes: TermMode::empty(),
            to_child: typed,
            dirty: true,
            last_output: Instant::now(),
            last_frame: Instant::now(),
            exit: None,
            trace,
            started: Instant::now(),
        };
        s.trace("RUN", &format!("{:?} {}x{} child={}x{} outer={:?}", opts.argv, size.rows, size.cols, child_rows, size.cols, s.outer));
        let mut out = String::from("\x1b[2J\x1b[H");
        out.push_str(&s.render.region());
        s.emit(out.as_bytes());
        Ok(s)
    }

    fn run(&mut self) -> i32 {
        let mut buf = vec![0u8; 65536];
        loop {
            if let Some(code) = self.exit {
                return code;
            }
            if let Ok(Some(status)) = self.pty.child.try_wait() {
                self.drain_child(&mut buf);
                return exit_code(status);
            }

            let timeout = self.poll_timeout();
            let master = self.pty.master.as_raw_fd();
            let mut fds = [
                libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: master, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: self.wake.read.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            ];
            if !self.to_child.is_empty() {
                fds[1].events |= libc::POLLOUT;
            }
            let ms = timeout.map_or(200, |t| t.as_millis().clamp(0, 200) as i32);
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
            if n < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                return 1;
            }

            if fds[2].revents & libc::POLLIN != 0 {
                for byte in self.wake.drain() {
                    self.on_wake(byte);
                }
            }
            self.collect_threads();

            if fds[1].revents & libc::POLLOUT != 0 {
                self.flush_child();
            }
            if fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 && !self.read_child(&mut buf) {
                let status = self.pty.child.wait().ok();
                return status.map_or(0, exit_code);
            }
            if fds[0].revents & libc::POLLIN != 0 {
                self.read_input(&mut buf);
            }
            if !self.to_child.is_empty() {
                self.flush_child();
            }

            self.maybe_paint();
        }
    }

    fn poll_timeout(&self) -> Option<Duration> {
        if !self.dirty {
            return None;
        }
        let now = Instant::now();
        let mut due = (self.last_output + SETTLE).min(self.last_frame + FRAME);
        if let Some(until) = self.scan.sync_until() {
            due = due.max(until);
        }
        Some(due.saturating_duration_since(now))
    }

    fn maybe_paint(&mut self) {
        if !self.dirty {
            return;
        }
        let now = Instant::now();
        if let Some(until) = self.scan.sync_until() {
            if now < until {
                return;
            }
            self.scan.end_sync();
        }
        if now >= self.last_output + SETTLE || now >= self.last_frame + FRAME {
            self.paint();
        }
    }

    fn paint(&mut self) {
        let bar = self.bar_lines();
        self.relayout_for_bar(bar.len());
        self.render.set_bar(bar);
        let reset = self.scan.take_history_reset();
        let frame = self.render.frame(&mut self.term, reset).to_owned();
        self.emit(frame.as_bytes());
        self.dirty = false;
        self.last_frame = Instant::now();
    }

    fn bar_lines(&self) -> Vec<String> {
        let cols = self.size.cols as usize;
        match &self.pane {
            Some(p) => p.lines(cols, self.max_bar()),
            None => self.keys.lines(cols),
        }
    }

    fn max_bar(&self) -> usize {
        (self.size.rows as usize).saturating_sub(MIN_CHILD).max(2)
    }

    /// The bar grows to show ! output and shrinks back after; the child's
    /// screen gives up or takes back the rows.
    fn relayout_for_bar(&mut self, wanted: usize) {
        let bar_rows = wanted.clamp(1, self.max_bar());
        if bar_rows != self.render.bar_rows() {
            self.relayout(self.size, bar_rows);
        }
    }

    fn relayout(&mut self, size: Size, bar_rows: usize) {
        let rows = size.rows as usize;
        let child_rows = rows.saturating_sub(bar_rows).max(1);
        self.size = size;
        self.term.resize(Dims { lines: child_rows, cols: size.cols as usize });
        term::resize(self.pty.master.as_fd(), size.with_rows(child_rows as u16));
        self.keys.set_child_rows(child_rows as u16);
        self.render.invalidate(&self.term, rows, size.cols as usize, child_rows);
        let region = self.render.region();
        self.emit(region.as_bytes());
        self.trace("SIZE", &format!("{}x{} child={child_rows}", rows, size.cols));
        self.dirty = true;
    }

    fn on_wake(&mut self, byte: u8) {
        match byte {
            term::WAKE_WINCH => {
                if let Some(size) = term::size_of(io::stdout().as_raw_fd())
                    && size != self.size
                {
                    let bar = self.render.bar_rows();
                    self.relayout(size, bar);
                }
            }
            term::WAKE_TERM | term::WAKE_HUP => {
                let sig = if byte == term::WAKE_HUP { libc::SIGHUP } else { libc::SIGTERM };
                unsafe { libc::kill(-(self.pty.child.id() as i32), sig) };
            }
            _ => {}
        }
    }

    fn collect_threads(&mut self) {
        while let Ok((id, pane)) = self.bang_rx.try_recv() {
            // Typing closed the pane while it ran, or a newer ! replaced it.
            if id == self.bang_id && self.pane.is_some() {
                self.trace("BANG", &format!("{:?} -> {:?} {:?}", pane.cmd, pane.status, pane.note));
                self.pane = Some(pane);
                self.dirty = true;
            }
        }
        if let Some(rx) = &self.awake_rx {
            let mut changed = None;
            while let Ok(on) = rx.try_recv() {
                changed = Some(on);
            }
            if let Some(on) = changed {
                self.keys.set_awake(on);
                self.trace("AWK", if on { "hold" } else { "release" });
                self.dirty = true;
            }
        }
    }

    /// Reads what the child wrote and runs it through the emulator. False
    /// once the child side of the PTY is gone.
    fn read_child(&mut self, buf: &mut [u8]) -> bool {
        let fd = self.pty.master.as_raw_fd();
        let mut total = 0;
        while total < READ_MAX {
            match term::read(fd, buf) {
                term::Read::Data(n) => {
                    total += n;
                    self.feed_child_output(n, buf);
                }
                term::Read::WouldBlock => break,
                term::Read::Closed => return false,
            }
        }
        true
    }

    fn drain_child(&mut self, buf: &mut [u8]) {
        let fd = self.pty.master.as_raw_fd();
        while let term::Read::Data(n) = term::read(fd, buf) {
            self.feed_child_output(n, buf);
        }
    }

    fn feed_child_output(&mut self, n: usize, buf: &[u8]) {
        let data = &buf[..n];
        self.scan.feed(data);
        self.proc.advance(&mut self.term, data);

        let mut outer = self.scan.take_outer();
        let reply = self.scan.take_reply();
        self.to_child.extend_from_slice(&reply);
        self.handle_events(&mut outer);
        self.forward_modes(&mut outer);
        if !outer.is_empty() {
            self.trace("FWD", &format!("{:?}", String::from_utf8_lossy(&outer)));
            self.emit(&outer);
        }

        self.dirty = true;
        self.last_output = Instant::now();
    }

    fn handle_events(&mut self, outer: &mut Vec<u8>) {
        let events: Vec<Event> = std::mem::take(&mut *self.events.0.borrow_mut());
        for event in events {
            match event {
                Event::PtyWrite(s) => self.to_child.extend_from_slice(s.as_bytes()),
                Event::Title(t) => {
                    let t: String = t.chars().filter(|c| !c.is_control()).collect();
                    outer.extend_from_slice(format!("\x1b]2;{t}\x1b\\").as_bytes());
                }
                Event::ResetTitle => outer.extend_from_slice(b"\x1b]2;\x1b\\"),
                Event::ClipboardStore(_, text) => {
                    outer.extend_from_slice(format!("\x1b]52;c;{}\x1b\\", base64(text.as_bytes())).as_bytes());
                }
                Event::ColorRequest(index, format) => {
                    let set = self.term.colors()[index];
                    let known = match index {
                        256 | 258 => self.outer.fg,
                        257 => self.outer.bg,
                        _ => None,
                    };
                    if let Some(rgb) = set.or(known) {
                        self.to_child.extend_from_slice(format(rgb).as_bytes());
                    }
                }
                Event::TextAreaSizeRequest(format) => {
                    let s = self.size;
                    let child_rows = self.render.child_rows as u16;
                    let ws = WindowSize {
                        num_lines: child_rows,
                        num_cols: s.cols,
                        cell_width: s.xpix.checked_div(s.cols).unwrap_or(0),
                        cell_height: s.ypix.checked_div(s.rows).unwrap_or(0),
                    };
                    self.to_child.extend_from_slice(format(ws).as_bytes());
                }
                Event::Bell => outer.push(0x07),
                _ => {}
            }
        }
    }

    fn forward_modes(&mut self, outer: &mut Vec<u8>) {
        let now = *self.term.mode();
        for (mode, on, off) in FORWARD_MODES {
            let was = self.modes.contains(*mode);
            let is = now.contains(*mode);
            if was != is {
                outer.extend_from_slice(if is { on.as_bytes() } else { off.as_bytes() });
            }
        }
        self.modes = now;
    }

    fn read_input(&mut self, buf: &mut [u8]) {
        let data = match term::read(0, buf) {
            term::Read::Data(n) => buf[..n].to_vec(),
            term::Read::WouldBlock => return,
            term::Read::Closed => {
                self.exit = Some(0);
                return;
            }
        };

        // Nothing is captured while the child asks for a password: echo off
        // in canonical mode. A raw-mode TUI has echo off too, but not ICANON.
        let capture = term::line_mode(self.pty.master.as_fd()).is_none_or(|(echo, canon)| echo || !canon);
        let quit = if capture {
            if self.keys.feed(&data) {
                self.dirty = true;
            }
            self.to_child.extend_from_slice(self.keys.pass());
            for cmd in self.keys.take_bangs() {
                self.run_bang(cmd);
            }
            if self.keys.typing() && self.pane.is_some() {
                self.pane = None;
                self.dirty = true;
            }
            self.keys.take_exit()
        } else {
            self.to_child.extend_from_slice(&data);
            keys::has_f10(&data)
        };
        if self.trace.is_some() {
            let line = format!("{:?} take={capture} buf={:?} pass={:?}", String::from_utf8_lossy(&data), self.keys.pending(), String::from_utf8_lossy(self.keys.pass()));
            self.trace("IN", &line);
        }
        if quit {
            self.quit(buf);
        }
    }

    /// F10: end the agent the way closing the terminal would - SIGHUP to its
    /// process group, which lets it save its session - and insist if it is
    /// still there a second later. Its last words still go through the
    /// emulator so the final frame shows them.
    fn quit(&mut self, buf: &mut [u8]) {
        self.trace("QUIT", "F10");
        let group = -(self.pty.child.id() as i32);
        unsafe { libc::kill(group, libc::SIGHUP) };
        let till = Instant::now() + QUIT_GRACE;
        while Instant::now() < till {
            self.drain_child(buf);
            if let Ok(Some(status)) = self.pty.child.try_wait() {
                self.exit = Some(exit_code(status));
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        unsafe { libc::kill(group, libc::SIGKILL) };
        self.exit = Some(self.pty.child.wait().map_or(1, exit_code));
    }

    /// A ! line never went to the child. It runs on a thread so the child
    /// keeps its keyboard and screen meanwhile; a bare ! just closes the pane.
    fn run_bang(&mut self, cmd: String) {
        self.dirty = true;
        if cmd.is_empty() {
            self.pane = None;
            return;
        }
        self.bang_id += 1;
        self.pane = Some(Pane::pending(&cmd));
        let (id, tx) = (self.bang_id, self.bang_tx.clone());
        std::thread::spawn(move || {
            let pane = bang::run(&cmd);
            let _ = tx.send((id, pane));
            term::wake(term::WAKE_THREAD);
        });
    }

    fn flush_child(&mut self) {
        match term::write_some(self.pty.master.as_raw_fd(), &self.to_child) {
            Ok(n) => {
                self.to_child.drain(..n);
            }
            Err(_) => self.to_child.clear(),
        }
    }

    fn emit(&self, bytes: &[u8]) {
        term::write_all(io::stdout().as_raw_fd(), bytes);
    }

    /// Last frame, then hand the terminal back the way we found it: full
    /// screen region, autowrap, input modes off, the bar erased and the
    /// cursor parked where it was.
    fn finish(&mut self) {
        if self.dirty {
            self.paint();
        }
        let mut out = Vec::new();
        for (mode, _, off) in FORWARD_MODES {
            if self.modes.contains(*mode) {
                out.extend_from_slice(off.as_bytes());
            }
        }
        out.extend_from_slice(&self.scan.restore());
        let child_rows = self.render.child_rows;
        let cursor = self.term.grid().cursor.point.line.0.max(0) as usize;
        let park = (cursor + 2).min(child_rows + 1);
        out.extend_from_slice(format!("\x1b[r\x1b[?7h\x1b[{};1H\x1b[0m\x1b[J\x1b[?25h", child_rows + 1).as_bytes());
        out.extend_from_slice(format!("\x1b[{park};1H").as_bytes());
        self.emit(&out);
        if let Some(a) = self.awake.take() {
            a.stop();
        }
        self.raw.take();
        let _ = io::stdout().flush();
    }

    fn trace(&self, kind: &str, text: &str) {
        if let Some(mut f) = self.trace.as_ref() {
            let _ = writeln!(f, "{:9.3} {kind:<4} {text}", self.started.elapsed().as_secs_f64());
        }
    }
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_pads() {
        assert_eq!(base64(b"hi"), "aGk=");
        assert_eq!(base64(b"hey"), "aGV5");
        assert_eq!(base64(b"h"), "aA==");
    }
}
