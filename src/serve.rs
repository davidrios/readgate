//! `readgate serve`: readgate with no window, for a machine with no
//! display. It mounts the config's mappings and asks about each read on
//! its socket, where `readgate attach` answers. The mappings come from the
//! config file; `readgate remount` reads it again.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::gate;
use crate::mount::{self, Mounts};
use crate::remote::{self, Control, Kind};
use crate::server;

pub fn run(config_path: &Path, mut config: Config) -> Result<()> {
    if let Some(kind) = remote::probe() {
        match kind {
            Kind::App => bail!("the readgate app is running; answer it with `readgate attach`"),
            Kind::Serve => bail!("readgate serve is running already"),
        }
    }
    let (gate, requests) = gate::gate(config.prompt_timeout());
    let hub = server::start(requests, config.log_file());
    let (control, controls) = async_channel::unbounded();
    stop_on_signals(control.clone())?;
    remote::listen(hub, control, Kind::Serve).context("terminals couldn't get to readgate")?;

    let mut mounts = Some(mount(&config, &gate).0);
    eprintln!("readgate: serving; answer with `readgate attach`, stop with `readgate stop`");
    while let Ok(control) = controls.recv_blocking() {
        match control {
            Control::Stop => break,
            Control::Remount(answer) => {
                // A config that doesn't load keeps the mounts as they are.
                let report = match Config::load(config_path) {
                    Ok(loaded) => {
                        config = loaded;
                        drop(mounts.take());
                        let (new, report) = mount(&config, &gate);
                        mounts = Some(new);
                        Ok(report)
                    }
                    Err(e) => Err(format!("{e:#}; the mounts are as they were")),
                };
                let _ = answer.send(report);
            }
        }
    }
    // Each mount goes lazily, so even with files open in it.
    drop(mounts);
    let _ = std::fs::remove_file(remote::socket_path());
    Ok(())
}

/// Mounts the mappings, and says how it went: a line per mapping.
fn mount(config: &Config, gate: &gate::Gate) -> (Mounts, String) {
    let (mounts, errors) = mount::mount_all(config, gate);
    let mut report = String::new();
    for m in &config.mappings {
        let dest = m.dest_path();
        match errors.get(&m.id) {
            None => report.push_str(&format!("{}: mounted\n", dest.display())),
            Some(e) => report.push_str(&format!("{}: not mounted: {e}\n", dest.display())),
        }
    }
    if config.mappings.is_empty() {
        report.push_str("no mappings in the config\n");
    }
    eprint!("{report}");
    (mounts, report.trim_end().to_owned())
}

/// Ctrl-C and SIGTERM stop it as `readgate stop` does, unmounting first.
#[cfg(unix)]
fn stop_on_signals(control: async_channel::Sender<Control>) -> Result<()> {
    use std::io::Read;
    use std::os::fd::IntoRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicI32, Ordering};

    static WAKE: AtomicI32 = AtomicI32::new(-1);
    extern "C" fn on_signal(_: libc::c_int) {
        let fd = WAKE.load(Ordering::Relaxed);
        // SAFETY: write is async-signal-safe; the byte's only a wake-up.
        unsafe { libc::write(fd, [0u8].as_ptr().cast(), 1) };
    }

    // A pair of sockets as a self-pipe: the handler writes, a thread reads.
    let (mut read, write) = UnixStream::pair().context("making the signal pipe")?;
    // Kept open for as long as the process runs.
    WAKE.store(write.into_raw_fd(), Ordering::Relaxed);
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: on_signal only does async-signal-safe things.
        unsafe { libc::signal(signal, on_signal as *const () as libc::sighandler_t) };
    }
    std::thread::spawn(move || {
        let mut byte = [0u8];
        if read.read(&mut byte).is_ok_and(|n| n == 1) {
            let _ = control.send_blocking(Control::Stop);
        }
    });
    Ok(())
}

#[cfg(not(unix))]
fn stop_on_signals(_: async_channel::Sender<Control>) -> Result<()> {
    Ok(())
}
