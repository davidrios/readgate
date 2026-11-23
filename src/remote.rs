//! The socket terminals answer through: `readgate attach`, `readgate stop`
//! and `readgate remount` talk to the running readgate over it, the app or
//! `readgate serve` alike.
//!
//! It's a Unix socket in the user's runtime folder, readable by the user
//! alone, and the server drops any connection from another user: whoever
//! can connect can allow reads. One JSON object per line each way.

use std::path::PathBuf;
use std::sync::mpsc;

use serde::{Deserialize, Serialize};

use crate::server::Question;
use crate::service::{Answer, GrantChoice};

/// Which readgate is listening.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The desktop app.
    App,
    /// `readgate serve`, with no window.
    Serve,
}

/// From the running readgate to a terminal.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToClient {
    /// First thing on every connection.
    Hello { kind: Kind },
    /// A question to answer; only after `attach`.
    Ask(WireQuestion),
    /// The question with this id is answered or timed out.
    Done { id: u64 },
    /// How a `remount` or `stop` went, or why a line was refused.
    Reply { ok: bool, message: String },
}

/// What the prompt shows, with the time left in place of a deadline.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireQuestion {
    pub id: u64,
    pub title: String,
    pub summary: String,
    pub details: String,
    pub seconds_left: f64,
    /// The reader's parent, for Allow for 1 Minute: unset, no such answer.
    pub grant: Option<String>,
    /// The programs further up the chain it can go to instead.
    pub grant_choices: Vec<GrantChoice>,
}

impl WireQuestion {
    fn of(question: &Question) -> WireQuestion {
        let r = &question.request;
        WireQuestion {
            id: question.id,
            title: r.title.clone(),
            summary: r.summary.clone(),
            details: r.details.clone(),
            seconds_left: r.deadline.saturating_duration_since(std::time::Instant::now()).as_secs_f64(),
            grant: r.grant.clone(),
            grant_choices: r.grant_choices.clone(),
        }
    }
}

/// From a terminal to the running readgate.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToServer {
    /// Send me the questions.
    Attach,
    Answer {
        id: u64,
        answer: WireAnswer,
    },
    /// Mount the mappings again; `serve` reads the config file first.
    Remount,
    /// Unmount everything and quit.
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireAnswer {
    Allow,
    /// For a minute, to the reader's parent.
    Minute,
    /// For a minute, to the ancestor at this place among the choices.
    MinuteTo {
        at: usize,
    },
    Deny,
}

impl From<WireAnswer> for Answer {
    fn from(answer: WireAnswer) -> Answer {
        match answer {
            WireAnswer::Allow => Answer::Allow,
            WireAnswer::Minute => Answer::AllowMinute,
            WireAnswer::MinuteTo { at } => Answer::AllowMinuteTo(at),
            WireAnswer::Deny => Answer::Deny,
        }
    }
}

/// What a terminal asks of the process holding the mounts, which acts on
/// it on its own thread.
pub enum Control {
    Stop,
    /// Answered with what got mounted, or why it couldn't.
    Remount(mpsc::Sender<Result<String, String>>),
}

/// `READGATE_SOCKET` overrides it, for trying things out. Otherwise it's
/// in the runtime folder, which an SSH session shares with the desktop
/// session of the same user.
pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("READGATE_SOCKET").filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("readgate.sock");
    }
    #[cfg(unix)]
    {
        // An SSH session without pam_systemd has no XDG_RUNTIME_DIR, but
        // the desktop's is still there.
        let dir = PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }));
        if dir.is_dir() {
            return dir.join("readgate.sock");
        }
    }
    crate::config::data_dir().join("readgate.sock")
}

#[cfg(unix)]
pub use unix::{Client, listen, probe, request};

#[cfg(unix)]
mod unix {
    use std::io::{self, BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    use anyhow::{Context, Result, bail};

    use super::{Control, Kind, ToClient, ToServer, WireQuestion, socket_path};
    use crate::server::{Event, Hub};

    /// How long `remount` waits for the mounts to come back.
    const REMOUNT_WAIT: Duration = Duration::from_secs(60);

    /// Listens on the socket, on a thread of its own. Fails if another
    /// readgate is listening already. A socket left behind by one that
    /// was killed is replaced.
    pub fn listen(hub: Arc<Hub>, control: async_channel::Sender<Control>, kind: Kind) -> Result<()> {
        let path = socket_path();
        if path.exists() {
            if UnixStream::connect(&path).is_ok() {
                bail!("another readgate is listening on {}", path.display());
            }
            std::fs::remove_file(&path).with_context(|| format!("removing the old {}", path.display()))?;
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let listener = UnixListener::bind(&path).with_context(|| format!("listening on {}", path.display()))?;
        // Between the bind and this, the peer check still turns others
        // away.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("making {} yours alone", path.display()))?;
        std::thread::Builder::new().name("readgate-socket".into()).spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (hub, control) = (hub.clone(), control.clone());
                let _ = std::thread::Builder::new()
                    .name("readgate-terminal".into())
                    .spawn(move || connection(stream, &hub, &control, kind));
            }
        })?;
        Ok(())
    }

    type Writer = Arc<Mutex<UnixStream>>;

    fn send(writer: &Writer, message: &ToClient) -> io::Result<()> {
        let mut line = serde_json::to_vec(message)?;
        line.push(b'\n');
        writer.lock().unwrap().write_all(&line)
    }

    fn reply(writer: &Writer, ok: bool, message: impl Into<String>) {
        let _ = send(writer, &ToClient::Reply { ok, message: message.into() });
    }

    fn connection(stream: UnixStream, hub: &Arc<Hub>, control: &async_channel::Sender<Control>, kind: Kind) {
        let Ok(write_half) = stream.try_clone() else { return };
        let writer: Writer = Arc::new(Mutex::new(write_half));
        match peer_uid(&stream) {
            Ok(uid) if uid == unsafe { libc::getuid() } => {}
            _ => {
                reply(&writer, false, "this readgate belongs to another user");
                return;
            }
        }
        if send(&writer, &ToClient::Hello { kind }).is_err() {
            return;
        }
        let mut attached = false;
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            let message = match serde_json::from_str::<ToServer>(&line) {
                Ok(message) => message,
                Err(e) => {
                    reply(&writer, false, format!("not a request readgate knows: {e}"));
                    continue;
                }
            };
            match message {
                ToServer::Attach if !attached => {
                    attached = true;
                    let events = hub.subscribe();
                    let writer = writer.clone();
                    let _ = std::thread::Builder::new().name("readgate-attached".into()).spawn(move || {
                        // Ends at the first event after the terminal goes,
                        // as writing to it fails.
                        while let Ok(event) = events.recv_blocking() {
                            let message = match event {
                                Event::Ask(question) => ToClient::Ask(WireQuestion::of(&question)),
                                Event::Done(id) => ToClient::Done { id },
                                Event::Logged(_) => continue,
                            };
                            if send(&writer, &message).is_err() {
                                break;
                            }
                        }
                    });
                }
                ToServer::Attach => {}
                ToServer::Answer { id, answer } => hub.answer(id, answer.into()),
                ToServer::Remount => {
                    let (answer, answered) = mpsc::channel();
                    if control.send_blocking(Control::Remount(answer)).is_err() {
                        reply(&writer, false, "readgate is stopping");
                        continue;
                    }
                    match answered.recv_timeout(REMOUNT_WAIT) {
                        Ok(Ok(report)) => reply(&writer, true, report),
                        Ok(Err(e)) => reply(&writer, false, e),
                        Err(_) => reply(&writer, false, "readgate didn't say how mounting went"),
                    }
                }
                ToServer::Stop => {
                    // Before, as the process may end before it gets to
                    // replying. The terminal waits for the connection to
                    // close.
                    reply(&writer, true, "stopping");
                    let _ = control.send_blocking(Control::Stop);
                }
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
        use std::os::fd::AsRawFd;
        let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: cred and len are valid for writes of the sizes given.
        let r = unsafe {
            libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &mut len)
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(cred.uid)
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
        use std::os::fd::AsRawFd;
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: uid and gid are valid for writes.
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(uid)
    }

    /// A connection to the running readgate, past its hello.
    pub struct Client {
        pub kind: Kind,
        reader: BufReader<UnixStream>,
        writer: UnixStream,
    }

    impl Client {
        pub fn connect() -> Result<Client> {
            let path = socket_path();
            let stream = UnixStream::connect(&path)
                .with_context(|| format!("readgate isn't running (nothing listens on {})", path.display()))?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            let mut client = Client { kind: Kind::App, writer: stream.try_clone()?, reader: BufReader::new(stream) };
            match client.receive()? {
                Some(ToClient::Hello { kind }) => client.kind = kind,
                Some(ToClient::Reply { message, .. }) => bail!("{message}"),
                _ => bail!("{} isn't readgate's socket", path.display()),
            }
            client.reader.get_ref().set_read_timeout(None)?;
            Ok(client)
        }

        pub fn send(&mut self, message: &ToServer) -> Result<()> {
            let mut line = serde_json::to_vec(message)?;
            line.push(b'\n');
            self.writer.write_all(&line)?;
            Ok(())
        }

        /// The next message, or `None` once the connection closes.
        pub fn receive(&mut self) -> Result<Option<ToClient>> {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                return Ok(None);
            }
            Ok(Some(serde_json::from_str(&line).context("readgate sent something this one doesn't understand")?))
        }

        /// Splits off the reading end, for a thread of its own.
        pub fn into_parts(self) -> (BufReader<UnixStream>, UnixStream) {
            (self.reader, self.writer)
        }
    }

    /// Which readgate is listening, if one is.
    pub fn probe() -> Option<Kind> {
        Client::connect().ok().map(|c| c.kind)
    }

    /// Sends `remount` or `stop`, and returns what readgate said. A stop
    /// waits for readgate to end, so its mounts are gone when it returns.
    pub fn request(message: ToServer) -> Result<String> {
        let mut client = Client::connect()?;
        client.send(&message)?;
        let stopping = matches!(message, ToServer::Stop);
        loop {
            match client.receive()? {
                Some(ToClient::Reply { ok: true, message }) => {
                    if stopping {
                        // Unmounting is lazy, so this doesn't take long.
                        client.reader.get_ref().set_read_timeout(Some(Duration::from_secs(30)))?;
                        while client.receive().is_ok_and(|m| m.is_some()) {}
                    }
                    return Ok(message);
                }
                Some(ToClient::Reply { ok: false, message }) => bail!("{message}"),
                Some(_) => continue,
                None => bail!("readgate closed the connection without answering"),
            }
        }
    }
}

#[cfg(not(unix))]
pub use other::{Client, listen, probe, request};

/// No Unix sockets: no terminals, until there's a named pipe.
#[cfg(not(unix))]
mod other {
    use std::sync::Arc;

    use anyhow::{Result, bail};

    use super::{Control, Kind, ToClient, ToServer};
    use crate::server::Hub;

    pub struct Client {
        pub kind: Kind,
    }

    impl Client {
        pub fn connect() -> Result<Client> {
            bail!("answering from a terminal isn't implemented on this platform yet")
        }
        pub fn send(&mut self, _: &ToServer) -> Result<()> {
            unreachable!()
        }
        pub fn receive(&mut self) -> Result<Option<ToClient>> {
            unreachable!()
        }
    }

    pub fn listen(_: Arc<Hub>, _: async_channel::Sender<Control>, _: Kind) -> Result<()> {
        bail!("answering from a terminal isn't implemented on this platform yet")
    }

    pub fn probe() -> Option<Kind> {
        None
    }

    pub fn request(_: ToServer) -> Result<String> {
        bail!("answering from a terminal isn't implemented on this platform yet")
    }
}
