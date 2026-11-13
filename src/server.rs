//! The server: owns the mounts and answers their requests, running on
//! after the app closes. Each request gets a prompt window of its own, from
//! `readgate prompt`, so the server needs no window of its own.
//!
//! It reads the config once, at start. The app restarts it after changing
//! the config.

use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::Result;
use time::OffsetDateTime;

use crate::access_log::{self, Access, Decision, LogEntry};
use crate::config::Config;
use crate::gate::{self, AccessRequest};
use crate::ipc::{self, Command, Event, MappingStatus};
use crate::mount::Mounts;
use crate::reader::Ancestor;
use crate::ui::prompt::PromptRequest;

/// How long past its deadline a prompt window gets to answer before it's
/// stopped: its own countdown ends at the deadline.
const PROMPT_GRACE: Duration = Duration::from_secs(2);

/// How long "Allow for 1 Minute" lasts.
const GRANT_LENGTH: Duration = Duration::from_secs(60);

/// An "Allow for 1 Minute": requests for its mapping, for its access or
/// less, from its process or anything it started, are allowed without
/// asking until it runs out.
struct Grant {
    mapping_id: u64,
    access: Access,
    /// The reader's parent when it was given: a pid with its start time,
    /// so a later process that gets the pid again isn't it.
    pid: u32,
    start: Option<u64>,
    until: Instant,
}

impl Grant {
    fn covers(&self, request: &AccessRequest, now: Instant) -> bool {
        let reader = &request.reader;
        let is_it = |pid: u32, start: Option<u64>| pid == self.pid && start == self.start;
        now < self.until
            && request.mapping.id == self.mapping_id
            && self.access.covers(request.access)
            && (is_it(reader.pid, reader.start) || reader.ancestors.iter().any(|a| is_it(a.pid, a.start)))
    }
}

/// readgate's own path, as it was when the server started: a rebuild or an
/// upgrade replaces the file, after which the running server's own link to
/// it (`/proc/self/exe`) names a deleted file, which can't be started.
static EXE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

struct Server {
    config_path: PathBuf,
    statuses: Vec<MappingStatus>,
    #[cfg(unix)]
    subscribers: Mutex<Vec<UnixStream>>,
    /// Dropped (unmounting everything) on shutdown.
    mounts: Mutex<Option<Mounts>>,
    /// The prompt window up now, stopped on shutdown.
    prompt: Mutex<Option<Child>>,
    grants: Mutex<Vec<Grant>>,
}

#[cfg(not(unix))]
pub fn run(_config_path: PathBuf) -> Result<()> {
    anyhow::bail!("the server doesn't run on this platform yet")
}

#[cfg(unix)]
pub fn run(config_path: PathBuf) -> Result<()> {
    let _ = EXE.set(std::env::current_exe()?);
    let config = Config::load(&config_path)?;
    let listener = ipc::listen(&config_path)?;
    eprintln!("readgate: serving {} ({} mappings)", config_path.display(), config.mappings.len());

    let (gate, requests) = gate::gate(config.prompt_timeout());
    let mut mounts = Mounts::new(gate);
    let statuses = config
        .mappings
        .iter()
        .map(|m| {
            let error = mounts.mount(m, &config.mount_settings(&m.mount)).err();
            if let Some(e) = &error {
                eprintln!("readgate: {} isn't mounted: {e}", m.dest_path().display());
            }
            MappingStatus { id: m.id, error }
        })
        .collect();
    let server = Arc::new(Server {
        config_path: config_path.clone(),
        statuses,
        subscribers: Mutex::default(),
        mounts: Mutex::new(Some(mounts)),
        prompt: Mutex::default(),
        grants: Mutex::default(),
    });

    let accepting = server.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let server = accepting.clone();
            std::thread::spawn(move || server.serve(stream));
        }
    });

    // One prompt at a time: the others wait in line, their readers still
    // blocked, their time still running.
    while let Ok(request) = requests.recv_blocking() {
        let decision = server.ask(&request);
        server.record(&config, &request, decision);
        request.answer(decision);
    }
    Ok(())
}

impl Server {
    #[cfg(unix)]
    fn serve(&self, stream: UnixStream) {
        let mut line = String::new();
        let Ok(mut writer) = stream.try_clone() else { return };
        // Empty: a check that the server is running.
        if BufReader::new(&stream).read_line(&mut line).is_err() || line.trim().is_empty() {
            return;
        }
        match serde_json::from_str(&line) {
            Ok(Command::Subscribe) => {
                let status = Event::Status { mappings: self.statuses.clone() };
                if ipc::write_line(&mut writer, &status).is_ok() {
                    self.subscribers.lock().unwrap().push(writer);
                }
            }
            Ok(Command::Shutdown) => self.shutdown(),
            Err(e) => eprintln!("readgate: a command didn't parse: {e}"),
        }
    }

    /// Unmounts everything and exits. The connection that asked closes
    /// with the process, which tells the app it's done.
    #[cfg(unix)]
    fn shutdown(&self) -> ! {
        eprintln!("readgate: shutting down");
        if let Some(mut prompt) = self.prompt.lock().unwrap().take() {
            let _ = prompt.kill();
        }
        drop(self.mounts.lock().unwrap().take());
        let _ = std::fs::remove_file(ipc::socket_path(&self.config_path));
        std::process::exit(0);
    }

    fn ask(&self, request: &AccessRequest) -> Decision {
        let now = Instant::now();
        {
            let mut grants = self.grants.lock().unwrap();
            grants.retain(|g| now < g.until);
            // Checked as each request's turn comes, so those that waited in
            // line behind the prompt that gave it are covered too.
            if grants.iter().any(|g| g.covers(request, now)) {
                return Decision::Granted;
            }
        }
        let left = request.deadline.saturating_duration_since(now);
        // Ran out while waiting in line: never shown.
        if left.is_zero() {
            return Decision::TimedOut;
        }
        match self.show_prompt(request, left) {
            Ok(decision) => decision,
            Err(e) => {
                eprintln!("readgate: couldn't show a prompt, so it's denied: {e:#}");
                Decision::Denied
            }
        }
    }

    /// Shows `request` in a prompt window, and waits for its answer: what
    /// it prints, `allow`, `deny` or `timeout`. A window closed without an
    /// answer prints nothing, and that's a no.
    fn show_prompt(&self, request: &AccessRequest, left: Duration) -> Result<Decision> {
        let dest = request.mapping.dest_path();
        let name = dest.file_name().map_or_else(|| dest.display().to_string(), |n| n.to_string_lossy().into_owned());
        let prompt = PromptRequest {
            title: format!("Allow {} to {} {name}?", request.reader.program(), request.access.verb()),
            summary: format!(
                "{} (pid {}), user {}",
                request.reader.program(),
                request.reader.pid,
                request.reader.user_label()
            ),
            details: format!(
                "{}\n\nFile: {}\nMounted from: {}",
                request.reader.describe(),
                dest.display(),
                request.mapping.source.display()
            ),
            seconds: left.as_secs_f64(),
            grant: request.reader.ancestors.first().map(|parent| parent.label()),
            // Only when it's not the parent itself, which the button
            // already offers.
            grant_program: request.reader.first_program().filter(|(at, _)| *at > 0).map(|(_, a)| a.label()),
        };
        let exe = EXE.get().cloned().map_or_else(std::env::current_exe, Ok)?;
        let mut child =
            std::process::Command::new(exe).arg("prompt").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
        let mut stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        ipc::write_line(&mut stdin, &prompt)?;
        drop(stdin);
        *self.prompt.lock().unwrap() = Some(child);

        let (answered, answer) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = answered.send(line);
        });
        let decision = match answer.recv_timeout(left + PROMPT_GRACE) {
            Ok(line) => match line.trim() {
                "allow" => Decision::Allowed,
                "allow-minute" => self.grant(request, request.reader.ancestors.first()),
                "allow-minute-program" => self.grant(request, request.reader.first_program().map(|(_, a)| a)),
                "timeout" => Decision::TimedOut,
                _ => Decision::Denied,
            },
            Err(_) => Decision::TimedOut,
        };
        if let Some(mut child) = self.prompt.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        Ok(decision)
    }

    /// Allows `request`, and for a minute what `to` (one of its
    /// ancestors) and everything it starts asks of the same file.
    fn grant(&self, request: &AccessRequest, to: Option<&Ancestor>) -> Decision {
        if let Some(to) = to {
            self.grants.lock().unwrap().push(Grant {
                mapping_id: request.mapping.id,
                access: request.access,
                pid: to.pid,
                start: to.start,
                until: Instant::now() + GRANT_LENGTH,
            });
        }
        Decision::Allowed
    }

    fn record(&self, config: &Config, request: &AccessRequest, decision: Decision) {
        let entry = LogEntry {
            time: OffsetDateTime::now_utc(),
            mapping_id: request.mapping.id,
            source: request.mapping.source.clone(),
            dest: request.mapping.dest_path(),
            reader: request.reader.clone(),
            access: request.access,
            decision,
        };
        if let Err(e) = access_log::append(&config.log_file(), &entry) {
            eprintln!("readgate: couldn't write the access log: {e:#}");
        }
        #[cfg(unix)]
        {
            let event = Event::Answer { entry: Box::new(entry) };
            // An app that closed is dropped here.
            self.subscribers
                .lock()
                .unwrap()
                .retain_mut(|s| ipc::write_line(s, &event).and_then(|()| s.flush()).is_ok());
        }
    }
}
