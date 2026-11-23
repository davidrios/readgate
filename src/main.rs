//! readgate: files other programs can only read after you say yes.
//!
//! The README has the design.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod access_log;
#[cfg(unix)]
mod attach;
mod config;
mod gate;
mod mount;
mod perms;
mod reader;
mod remote;
mod serve;
mod server;
mod service;
mod ui;

use config::Config;
use remote::{Kind, ToServer};

const USAGE: &str = "use one of:
  readgate            open the app, or the running one's window
  readgate --hidden   start the app in the tray only
  readgate serve      run without a window, for a machine with no display
  readgate attach     answer the running readgate's questions in this terminal
  readgate remount    mount the mappings again (serve reads the config first)
  readgate stop       unmount everything and quit the running readgate";

fn main() -> anyhow::Result<()> {
    // Before any thread starts.
    ui::init_local_offset();
    let config_path = config::default_config_file();
    let start = match std::env::args().nth(1).as_deref() {
        None => ui::Start::Open,
        Some("--hidden") => ui::Start::Hidden,
        Some("serve") => return serve::run(&config_path, Config::load(&config_path)?),
        Some("attach") => return attach(),
        Some("remount") => return request(ToServer::Remount),
        Some("stop") => return request(ToServer::Stop),
        Some("help" | "--help" | "-h") => {
            println!("{USAGE}");
            return Ok(());
        }
        Some(other) => anyhow::bail!("readgate: {other:?} isn't a command; {USAGE}"),
    };
    // The app and `serve` would mount the same folders.
    if remote::probe() == Some(Kind::Serve) {
        anyhow::bail!(
            "readgate serve is running; stop it with `readgate stop` first, or answer it with `readgate attach`"
        );
    }
    if !ui::has_display() {
        anyhow::bail!(
            "there's no display for the app; run `readgate serve` instead, and answer with `readgate attach`"
        );
    }
    ui::run(config_path.clone(), Config::load(&config_path)?, start);
    Ok(())
}

#[cfg(unix)]
fn attach() -> anyhow::Result<()> {
    attach::run()
}

#[cfg(not(unix))]
fn attach() -> anyhow::Result<()> {
    anyhow::bail!("answering from a terminal isn't implemented on this platform yet")
}

fn request(message: ToServer) -> anyhow::Result<()> {
    println!("{}", remote::request(message)?);
    Ok(())
}
