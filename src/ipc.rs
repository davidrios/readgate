//! How the app talks to the server: JSON lines over a Unix socket in the
//! runtime folder, one socket per config file, so each config has its own
//! server.
//!
//! The app sends one command per connection. `subscribe` keeps the
//! connection open and gets the mappings' status, then every answer as
//! it's given. `shutdown` stops the server, which closes the connection as
//! it exits.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::access_log::LogEntry;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Subscribe,
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Every mapping's, as the server mounted them.
    Status { mappings: Vec<MappingStatus> },
    Answer {
        // Boxed: an entry is much bigger than a status.
        entry: Box<LogEntry>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MappingStatus {
    pub id: u64,
    /// Why it isn't mounted, or `None` if it is.
    pub error: Option<String>,
}

/// The server's socket for `config`.
pub fn socket_path(config: &Path) -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join(format!("readgate-{}.sock", key(config)))
}

/// A short, stable name for a config file's path (FNV-1a).
fn key(config: &Path) -> String {
    let config = config.canonicalize().unwrap_or_else(|_| config.to_owned());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in config.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

pub fn write_line(stream: &mut impl Write, value: &impl Serialize) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    stream.write_all(&line)
}

pub use platform::*;

#[cfg(unix)]
mod platform {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use anyhow::Context;

    use super::*;

    pub fn is_running(config: &Path) -> bool {
        UnixStream::connect(socket_path(config)).is_ok()
    }

    /// The server's socket, taking over one left by a server that died.
    pub fn listen(config: &Path) -> Result<UnixListener> {
        let path = socket_path(config);
        if is_running(config) {
            bail!("a readgate server for {} is already running", config.display());
        }
        let _ = std::fs::remove_file(&path);
        UnixListener::bind(&path).with_context(|| format!("listening on {}", path.display()))
    }

    /// Starts the server, unless it's running, and waits for it to listen.
    pub fn ensure_running(config: &Path) -> Result<()> {
        if is_running(config) {
            return Ok(());
        }
        start_server(config)?;
        wait_until(Duration::from_secs(5), || is_running(config))
            .then_some(())
            .with_context(|| format!("the server didn't start; its output is in {}", server_log().display()))
    }

    /// The server outlives the app: its own session (no terminal, no
    /// hangup when the app's goes), its output to a file.
    fn start_server(config: &Path) -> Result<()> {
        let log = server_log();
        if let Some(dir) = log.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let out = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command.arg("server").env("READGATE_CONFIG", config).stdin(Stdio::null()).stdout(out.try_clone()?).stderr(out);
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = command.spawn().context("starting the server")?;
        // Reaped if it exits while the app runs; adopted by init after.
        std::thread::spawn(move || child.wait());
        Ok(())
    }

    /// Stops the server, and waits for it to be gone.
    pub fn shutdown(config: &Path) -> Result<()> {
        let Ok(mut stream) = UnixStream::connect(socket_path(config)) else { return Ok(()) };
        write_line(&mut stream, &Command::Shutdown)?;
        // It closes the connection as it exits.
        let _ = std::io::Read::read_to_end(&mut stream, &mut Vec::new());
        if wait_until(Duration::from_secs(10), || !is_running(config)) {
            Ok(())
        } else {
            bail!("the server didn't stop")
        }
    }

    /// Calls `on_event` for every event, until the server goes.
    pub fn subscribe(config: &Path, mut on_event: impl FnMut(Event)) -> Result<()> {
        let mut stream = UnixStream::connect(socket_path(config)).context("connecting to the server")?;
        write_line(&mut stream, &Command::Subscribe)?;
        for line in BufReader::new(stream).lines() {
            match serde_json::from_str(&line?) {
                Ok(event) => on_event(event),
                Err(e) => eprintln!("readgate: an event from the server didn't parse: {e}"),
            }
        }
        Ok(())
    }

    fn wait_until(limit: Duration, done: impl Fn() -> bool) -> bool {
        let end = Instant::now() + limit;
        while Instant::now() < end {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        done()
    }

    fn server_log() -> PathBuf {
        crate::config::data_dir().join("server.log")
    }
}

#[cfg(not(unix))]
mod platform {
    use super::*;

    // TODO: a named pipe on Windows.
    pub fn is_running(_config: &Path) -> bool {
        false
    }

    pub fn ensure_running(_config: &Path) -> Result<()> {
        bail!("the server doesn't run on this platform yet")
    }

    pub fn shutdown(_config: &Path) -> Result<()> {
        Ok(())
    }

    pub fn subscribe(_config: &Path, _on_event: impl FnMut(Event)) -> Result<()> {
        bail!("the server doesn't run on this platform yet")
    }
}
