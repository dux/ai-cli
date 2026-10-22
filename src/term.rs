//! The OS side of the wrapper: the real terminal's mode and size, the child's
//! PTY, and a self-pipe that turns signals and thread wake-ups into a readable
//! fd the main loop can poll.

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};

use nix::pty::{Winsize, openpty};
use nix::sys::termios::{self, LocalFlags, SetArg, Termios};

/// Rows, columns and pixel size of a terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub rows: u16,
    pub cols: u16,
    pub xpix: u16,
    pub ypix: u16,
}

impl Size {
    /// The same terminal cut down to `rows`, with the pixel height scaled to
    /// match, so a child asking for pixels gets its own share and not ours.
    pub fn with_rows(self, rows: u16) -> Size {
        let ypix = if self.rows == 0 { 0 } else { (self.ypix as u32 * rows as u32 / self.rows as u32) as u16 };
        Size { rows, cols: self.cols, xpix: self.xpix, ypix }
    }

    fn winsize(self) -> Winsize {
        Winsize { ws_row: self.rows, ws_col: self.cols, ws_xpixel: self.xpix, ws_ypixel: self.ypix }
    }
}

pub fn is_tty(fd: RawFd) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

pub fn size_of(fd: RawFd) -> Option<Size> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } != 0 || ws.ws_row == 0 || ws.ws_col == 0 {
        return None;
    }
    Some(Size { rows: ws.ws_row, cols: ws.ws_col, xpix: ws.ws_xpixel, ypix: ws.ws_ypixel })
}

/// Puts stdin in raw mode and puts it back when dropped - including on panic,
/// so a crash never leaves the shell without echo.
pub struct RawMode {
    saved: Termios,
}

impl RawMode {
    pub fn enter() -> io::Result<RawMode> {
        let stdin = io::stdin();
        let saved = termios::tcgetattr(stdin.as_fd())?;
        let mut raw = saved.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(stdin.as_fd(), SetArg::TCSANOW, &raw)?;
        Ok(RawMode { saved })
    }

    /// The terminal as the user had it, which is what the child's PTY should
    /// start out as (erase key, flow control and so on).
    pub fn saved(&self) -> &Termios {
        &self.saved
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(io::stdin().as_fd(), SetArg::TCSANOW, &self.saved);
    }
}

pub struct Pty {
    pub master: OwnedFd,
    pub child: Child,
}

/// Starts `argv` on a fresh PTY as the leader of its own session, with the
/// PTY as its controlling terminal - what a terminal emulator does for a shell.
pub fn spawn(argv: &[String], env: &[(String, String)], size: Size, modes: Option<&Termios>) -> io::Result<Pty> {
    let pty = openpty(Some(&size.winsize()), modes).map_err(io::Error::from)?;
    set_cloexec(pty.master.as_raw_fd())?;
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).env_remove(REEXEC_ENV);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::from(pty.slave.try_clone()?));
    cmd.stdout(Stdio::from(pty.slave.try_clone()?));
    cmd.stderr(Stdio::from(pty.slave.try_clone()?));
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    drop(pty.slave);
    set_nonblocking(pty.master.as_raw_fd())?;
    Ok(Pty { master: pty.master, child })
}

pub fn resize(master: BorrowedFd, size: Size) {
    let ws = size.winsize();
    unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
}

/// [echo, canonical] of the child's line discipline, read through the master.
/// Echo off while still canonical is a password prompt.
pub fn line_mode(master: BorrowedFd) -> Option<(bool, bool)> {
    let t = termios::tcgetattr(master).ok()?;
    Some((t.local_flags.contains(LocalFlags::ECHO), t.local_flags.contains(LocalFlags::ICANON)))
}

pub fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Replaces this process with `argv`. Only returns on failure.
pub fn exec(argv: &[String], env: &[(String, String)]) -> io::Error {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.exec()
}

/// Re-runs this binary with `name` as argv[0], so whatever reads the pane's
/// foreground process sees the agent and not `ai`. The pid stays the same.
pub fn exec_as(name: &str, args: &[OsString]) -> io::Error {
    match std::env::current_exe() {
        Ok(exe) => Command::new(exe).arg0(name).args(args).env(REEXEC_ENV, "1").exec(),
        Err(e) => e,
    }
}

pub const REEXEC_ENV: &str = "AI_WRAP_REEXEC";

// -- self-pipe ------------------------------------------------------------

/// What woke the loop up.
pub const WAKE_WINCH: u8 = b'W';
pub const WAKE_CHILD: u8 = b'C';
pub const WAKE_TERM: u8 = b'T';
pub const WAKE_HUP: u8 = b'H';
pub const WAKE_THREAD: u8 = b'X';

static WAKE_FD: AtomicI32 = AtomicI32::new(-1);

pub struct Wake {
    pub read: OwnedFd,
}

/// Wakes the main loop from any thread.
pub fn wake(byte: u8) {
    let fd = WAKE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        unsafe { libc::write(fd, &byte as *const u8 as *const libc::c_void, 1) };
    }
}

extern "C" fn on_signal(sig: libc::c_int) {
    let byte = match sig {
        libc::SIGWINCH => WAKE_WINCH,
        libc::SIGCHLD => WAKE_CHILD,
        libc::SIGHUP => WAKE_HUP,
        _ => WAKE_TERM,
    };
    wake(byte);
}

impl Wake {
    pub fn install() -> io::Result<Wake> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        for fd in fds {
            set_nonblocking(fd)?;
            set_cloexec(fd)?;
        }
        WAKE_FD.store(fds[1], Ordering::Relaxed);
        for sig in [libc::SIGWINCH, libc::SIGCHLD, libc::SIGTERM, libc::SIGHUP, libc::SIGINT, libc::SIGQUIT] {
            unsafe {
                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
                sa.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut sa.sa_mask);
                libc::sigaction(sig, &sa, std::ptr::null_mut());
            }
        }
        Ok(Wake { read: unsafe { OwnedFd::from_raw_fd(fds[0]) } })
    }

    pub fn drain(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            let n = unsafe { libc::read(self.read.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        out
    }
}

// -- plain fd io ----------------------------------------------------------

pub enum Read {
    Data(usize),
    WouldBlock,
    Closed,
}

pub fn read(fd: RawFd, buf: &mut [u8]) -> Read {
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n > 0 {
            return Read::Data(n as usize);
        }
        if n == 0 {
            return Read::Closed;
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => return Read::WouldBlock,
            _ => return Read::Closed, // EIO: the child side is gone
        }
    }
}

/// Writes as much as the fd takes without blocking; answers how much that was.
pub fn write_some(fd: RawFd, data: &[u8]) -> io::Result<usize> {
    loop {
        let n = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => return Ok(0),
            _ => return Err(err),
        }
    }
}

/// Blocking write of everything to the real terminal.
pub fn write_all(fd: RawFd, mut data: &[u8]) {
    while !data.is_empty() {
        let n = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
        if n > 0 {
            data = &data[n as usize..];
            continue;
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => {
                let mut pfd = libc::pollfd { fd, events: libc::POLLOUT, revents: 0 };
                unsafe { libc::poll(&mut pfd, 1, 100) };
            }
            _ => return,
        }
    }
}
