//! The access log: one JSON object per line, appended for every decision.
//!
//! JSON Lines so other tools (`jq`, a log shipper) can read it, and so a
//! line cut short by a crash only loses itself.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::reader::ReaderInfo;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allowed,
    Denied,
    /// Nobody answered in time. To the reader, the same as `Denied`.
    TimedOut,
    /// Allowed without asking, by an earlier "Allow for 1 Minute".
    Granted,
}

impl Decision {
    pub fn allows(self) -> bool {
        matches!(self, Decision::Allowed | Decision::Granted)
    }
}

/// What a reader opened the file for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Read,
    Write,
    ReadWrite,
}

impl Access {
    /// Whether allowing `self` allows `other` too: reading and writing
    /// covers either, and each covers itself.
    pub fn covers(self, other: Access) -> bool {
        self == other || self == Access::ReadWrite
    }

    /// For "Allow cat to … notes.txt?".
    pub fn verb(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::Write => "write to",
            Access::ReadWrite => "read and write",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    #[serde(with = "time::serde::rfc3339")]
    pub time: OffsetDateTime,
    pub mapping_id: u64,
    pub source: std::path::PathBuf,
    pub dest: std::path::PathBuf,
    pub reader: ReaderInfo,
    pub access: Access,
    pub decision: Decision,
}

pub fn append(path: &Path, entry: &LogEntry) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut line = serde_json::to_vec(entry)?;
    line.push(b'\n');
    // One write per line: appends of a line this size don't interleave.
    file.write_all(&line)?;
    Ok(())
}
