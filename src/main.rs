//! readgate: files other programs can only read after you say yes.
//!
//! The README has the design.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod access_log;
mod config;
mod gate;
mod mount;
mod perms;
mod reader;
mod service;
mod ui;

use config::Config;

/// `readgate` opens the app, or the running one's window; `readgate
/// --hidden` starts it in the tray only; `readgate stop` quits the running
/// one.
fn main() -> anyhow::Result<()> {
    // Before any thread starts.
    ui::init_local_offset();
    let start = match std::env::args().nth(1).as_deref() {
        None => ui::Start::Open,
        Some("--hidden") => ui::Start::Hidden,
        Some("stop") => ui::Start::Stop,
        Some(other) => anyhow::bail!("readgate: {other:?} isn't a command; use none, --hidden or stop"),
    };
    let config_path = config::default_config_file();
    ui::run(config_path.clone(), Config::load(&config_path)?, start);
    Ok(())
}
