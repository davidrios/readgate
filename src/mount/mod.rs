//! Mounting: each mapping's `mount` folder is a mount of readgate's,
//! showing every mapping that names it, at its dest inside. FUSE on Linux (and, later,
//! macOS), WinFsp on Windows. All three mount folders, not files, so every
//! platform works this way.

use std::collections::HashMap;
#[cfg(not(target_os = "linux"))]
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::config::{self, Mapping, MountSettings};
use crate::gate::Gate;

#[cfg(target_os = "linux")]
mod fuse;
#[cfg(target_os = "linux")]
use fuse::{Mounted, mount_folder};

/// A folder's mappings, shared with its filesystem, so adding a mapping to
/// a mounted folder shows it without mounting again.
pub type Shared = Arc<RwLock<Vec<Mapping>>>;

// macOS (macFUSE, FUSE-T) and Windows (WinFsp) will mount the same way.
// Until then, mounting fails with a reason the mappings screen shows.
#[cfg(not(target_os = "linux"))]
pub struct Mounted;

#[cfg(not(target_os = "linux"))]
fn mount_folder(
    _folder: &Path,
    _mappings: Shared,
    _gate: Gate,
    _scratch: &Path,
    _perms: crate::perms::Perms,
) -> anyhow::Result<Mounted> {
    anyhow::bail!("mounting isn't implemented on this platform yet")
}

struct Folder {
    mappings: Shared,
    _mounted: Mounted,
}

/// Every mounted folder, by path.
pub struct Mounts {
    gate: Gate,
    folders: HashMap<PathBuf, Folder>,
}

impl Mounts {
    pub fn new(gate: Gate) -> Mounts {
        Mounts { gate, folders: HashMap::new() }
    }

    /// Shows `mapping` in its mount folder, mounting the folder (with
    /// `settings`) if it isn't yet. The error says why it isn't mounted, for
    /// the mappings screen.
    pub fn mount(&mut self, mapping: &Mapping, settings: &MountSettings) -> Result<(), String> {
        check(mapping).map_err(|e| format!("{e:#}"))?;
        let folder = mapping.mount.clone();
        if let Some(mounted) = self.folders.get(&folder) {
            let mut mappings = mounted.mappings.write().unwrap();
            if let Some(other) = mappings.iter().find(|m| clashes(m, mapping)) {
                return Err(format!("its dest clashes with {}", other.dest_path().display()));
            }
            mappings.push(mapping.clone());
            return Ok(());
        }
        let perms = crate::perms::resolve(settings).map_err(|e| format!("{}: {e:#}", folder.display()))?;
        let mappings: Shared = Arc::new(RwLock::new(vec![mapping.clone()]));
        let scratch = config::scratch_dir(&folder);
        let mounted = mount_folder(&folder, mappings.clone(), self.gate.clone(), &scratch, perms)
            .map_err(|e| format!("{e:#}"))?;
        self.folders.insert(folder, Folder { mappings, _mounted: mounted });
        Ok(())
    }
}

/// Two dests in one mount clash if they're the same path, or one would
/// have to be a folder for the other (`a` and `a/b`).
fn clashes(a: &Mapping, b: &Mapping) -> bool {
    let (Some(a), Some(b)) = (a.dest_components(), b.dest_components()) else { return false };
    let shorter = a.len().min(b.len());
    a[..shorter] == b[..shorter]
}

/// What can be found before mounting, rather than as an I/O error after
/// the user allowed a read or write.
fn check(mapping: &Mapping) -> anyhow::Result<()> {
    use anyhow::Context;
    if !mapping.mount.is_absolute() {
        anyhow::bail!("the mount folder needs to be a full path");
    }
    if mapping.dest_components().is_none() {
        anyhow::bail!("the dest needs to be a path inside the mount folder, like token.txt or keys/token.txt");
    }
    std::fs::File::open(&mapping.source).with_context(|| format!("{} can't be read", mapping.source.display()))?;
    if !mapping.read_only {
        std::fs::OpenOptions::new()
            .write(true)
            .open(&mapping.source)
            .with_context(|| format!("{} can't be written", mapping.source.display()))?;
    }
    Ok(())
}
