//! Keeps the Mac awake while the agent in this pane is working (`--awake`).
//!
//! Herdr's `agent_status` for our own pane is asked over its socket; while it
//! reads `working` we hold a `caffeinate`, and let go once it has read anything
//! else RELEASE times in a row. Outside herdr the assertion is held for the
//! whole session. A thread, so a slow socket never stalls the pump.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const POLL: Duration = Duration::from_secs(3);
const TIMEOUT: Duration = Duration::from_secs(2);
const RELEASE: u32 = 2;

type OnChange = Box<dyn Fn(bool) + Send + 'static>;

struct Hold {
    child: Option<Child>,
    stopped: bool,
}

struct Inner {
    hold: Mutex<Hold>,
    /// Mutex only for Sync: the contract hands us a Send-only callback.
    on_change: Mutex<OnChange>,
}

impl Inner {
    /// -w ties caffeinate to us, so even a kill -9 of the wrapper lets go.
    fn hold(&self) {
        {
            let mut h = self.hold.lock().unwrap();
            if h.child.is_some() || h.stopped {
                return;
            }
            let spawned = Command::new("caffeinate")
                .args(["-i", "-s", "-w", &std::process::id().to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            match spawned {
                Ok(c) => h.child = Some(c),
                Err(_) => return,
            }
        }
        (self.on_change.lock().unwrap())(true);
    }

    fn release(&self) {
        let child = self.hold.lock().unwrap().child.take();
        let Some(mut child) = child else { return };
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(child.id() as i32),
            nix::sys::signal::Signal::SIGTERM,
        );
        let _ = child.wait();
        (self.on_change.lock().unwrap())(false);
    }
}

pub struct Awake {
    inner: Arc<Inner>,
    quit: Option<Sender<()>>,
}

impl Awake {
    pub fn start(on_change: OnChange) -> Awake {
        let inner = Arc::new(Inner {
            hold: Mutex::new(Hold { child: None, stopped: false }),
            on_change: Mutex::new(on_change),
        });
        let pane = std::env::var("HERDR_PANE_ID").unwrap_or_default();
        if pane.is_empty() {
            inner.hold();
            return Awake { inner, quit: None };
        }

        let socket = std::env::var("HERDR_SOCKET_PATH")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                format!("{}/.config/herdr/herdr.sock", std::env::var("HOME").unwrap_or_default())
            });
        let (tx, rx) = mpsc::channel();
        let worker = Arc::clone(&inner);
        std::thread::spawn(move || {
            let mut quiet = 0;
            loop {
                if status(&socket, &pane).as_deref() == Some("working") {
                    quiet = 0;
                    worker.hold();
                } else {
                    quiet += 1;
                    if quiet >= RELEASE {
                        worker.release();
                    }
                }
                match rx.recv_timeout(POLL) {
                    Err(RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
            }
        });
        Awake { inner, quit: Some(tx) }
    }

    pub fn stop(self) {
        self.inner.hold.lock().unwrap().stopped = true;
        if let Some(tx) = &self.quit {
            let _ = tx.send(());
        }
        self.inner.release();
    }

    /// The argv to exec instead, when there is no session to watch from.
    pub fn wrap(argv: &[String]) -> Vec<String> {
        let mut out: Vec<String> = ["caffeinate", "-i", "-s"].iter().map(|s| s.to_string()).collect();
        out.extend(argv.iter().cloned());
        out
    }
}

/// One NDJSON request, one line back. Anything going wrong reads as "not
/// working" - when in doubt, let the machine sleep.
fn status(socket: &str, pane: &str) -> Option<String> {
    let mut sock = UnixStream::connect(socket).ok()?;
    sock.set_read_timeout(Some(TIMEOUT)).ok()?;
    sock.set_write_timeout(Some(TIMEOUT)).ok()?;
    let req = serde_json::json!({
        "id": "llm-wrap-awake",
        "method": "pane.get",
        "params": { "pane_id": pane },
    });
    sock.write_all(format!("{req}\n").as_bytes()).ok()?;
    let mut line = String::new();
    BufReader::new(sock).read_line(&mut line).ok()?;
    let v: serde_json::Value = serde_json::from_str(&line).ok()?;
    v.pointer("/result/pane/agent_status")?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn wrap_prefixes_caffeinate() {
        let argv = vec!["claude".to_string(), "--x".to_string()];
        assert_eq!(Awake::wrap(&argv), ["caffeinate", "-i", "-s", "claude", "--x"]);
    }

    #[test]
    fn status_reads_agent_status_from_the_socket() {
        let path = std::env::temp_dir().join(format!("ai-awake-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut req = String::new();
            r.read_line(&mut req).unwrap();
            let v: serde_json::Value = serde_json::from_str(&req).unwrap();
            assert_eq!(v["method"], "pane.get");
            assert_eq!(v["params"]["pane_id"], "p1");
            (&s).write_all(b"{\"result\":{\"pane\":{\"agent_status\":\"working\"}}}\n").unwrap();
        });
        assert_eq!(status(path.to_str().unwrap(), "p1").as_deref(), Some("working"));
        server.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_socket_is_not_working() {
        assert_eq!(status("/no/such/herdr.sock", "p1"), None);
    }
}
