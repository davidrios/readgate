//! readgate: files other programs can only read after you say yes.
//!
//! The README has the design.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod access_log;
mod config;
mod gate;
mod ipc;
mod mount;
mod perms;
mod reader;
mod server;
mod ui;

use config::Config;

/// `readgate` opens the app; `readgate server` runs the server (the app
/// starts it); `readgate stop` stops it; `readgate prompt` shows one
/// prompt (the server starts it).
fn main() -> anyhow::Result<()> {
    // Before any thread starts.
    ui::init_local_offset();
    let config_path = config::default_config_file();
    match std::env::args().nth(1).as_deref() {
        Some("server") => server::run(config_path),
        Some("stop") => ipc::shutdown(&config_path),
        Some("prompt") => ui::prompt::run(),
        Some(other) => anyhow::bail!("readgate: {other:?} isn't a command; use none, server, stop or prompt"),
        None => {
            ui::run(config_path.clone(), Config::load(&config_path)?);
            Ok(())
        }
    }
}
