//! The config file: the mappings, and where the access log goes.
//!
//! It's TOML in the platform's config folder
//! (`~/.config/readgate/config.toml` on Linux,
//! `~/Library/Application Support/br.com.roboport.readgate/config.toml` on
//! macOS, `%APPDATA%\roboport\readgate\config\config.toml` on Windows).
//! The app reads it at start and writes it back after each change made in
//! the mappings screen, so it can also be edited by hand while the app is
//! closed.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// One source file shown at `dest` inside the folder `mount`, behind a
/// prompt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mapping {
    /// Stable across edits and restarts, so the UI and the log can refer
    /// to a mapping while its paths change.
    pub id: u64,
    /// The real file. Only readgate (running as you) reads it.
    pub source: PathBuf,
    /// The folder readgate mounts. Mappings with the same one share it.
    #[serde(default)]
    pub mount: PathBuf,
    /// Where the file is in `mount`, e.g. `token.txt` or `keys/deploy`:
    /// what other programs and users open, at `mount/dest`.
    pub dest: PathBuf,
    /// Whether the dest refuses writes. When false, writes (and
    /// truncating) go through to the source, behind the same prompt.
    #[serde(default = "read_only_by_default")]
    pub read_only: bool,
}

fn read_only_by_default() -> bool {
    true
}

impl Mapping {
    /// The dest's full path: what programs open.
    pub fn dest_path(&self) -> PathBuf {
        self.mount.join(&self.dest)
    }

    /// The dest inside its mount, checked: relative, down only, and naming
    /// a file. `None` if it isn't.
    pub fn dest_components(&self) -> Option<Vec<std::ffi::OsString>> {
        use std::path::Component;
        let parts: Option<Vec<_>> = self
            .dest
            .components()
            .map(|c| match c {
                Component::Normal(name) => Some(name.to_owned()),
                _ => None,
            })
            .collect();
        parts.filter(|p| !p.is_empty())
    }

    /// Configs from before `mount` had the dest's full path: its folder is
    /// the mount.
    fn upgrade(&mut self) {
        if self.mount.as_os_str().is_empty()
            && self.dest.is_absolute()
            && let (Some(folder), Some(name)) = (self.dest.parent(), self.dest.file_name())
        {
            (self.mount, self.dest) = (folder.to_owned(), PathBuf::from(name));
        }
    }
}

/// Settings of a mount folder, shared by every mapping in it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountSettings {
    pub path: PathBuf,
    /// `user:group`, `user` (with that user's group) or `:group`, by name
    /// or number. Unset, it's yours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// The files' mode in octal, like `0660`: without the write bits for
    /// read-only ones. The folders get execute wherever there's read
    /// (`0660` gives `0770`). Unset, it's `0666`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Where access log lines are appended. `None` uses the platform's
    /// data folder (see [`default_log_file`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_file: Option<PathBuf>,
    /// How long a prompt waits for an answer before it denies.
    /// Not written when it's the default, so a config saved before a new
    /// default gets that default.
    #[serde(default = "default_prompt_timeout_secs", skip_serializing_if = "is_default_prompt_timeout")]
    pub prompt_timeout_secs: u64,
    #[serde(default, rename = "mount", skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<MountSettings>,
    #[serde(default, rename = "mapping")]
    pub mappings: Vec<Mapping>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            log_file: None,
            prompt_timeout_secs: default_prompt_timeout_secs(),
            mounts: Vec::new(),
            mappings: Vec::new(),
        }
    }
}

fn default_prompt_timeout_secs() -> u64 {
    120
}

fn is_default_prompt_timeout(secs: &u64) -> bool {
    *secs == default_prompt_timeout_secs()
}

impl Config {
    /// Reads the config, or an empty one if there's no file yet.
    pub fn load(path: &Path) -> Result<Config> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let mut config: Config =
                    toml::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
                config.mappings.iter_mut().for_each(Mapping::upgrade);
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Writes the config next to its old self, then moves it in place, so
    /// a crash mid-write never leaves half a file.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = toml::to_string_pretty(self)?;
        let temp = path.with_extension("toml.tmp");
        fs::write(&temp, text).with_context(|| format!("writing {}", temp.display()))?;
        fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// The mount folder's settings, or the defaults if it has none.
    pub fn mount_settings(&self, path: &Path) -> MountSettings {
        self.mounts
            .iter()
            .find(|m| m.path == path)
            .cloned()
            .unwrap_or_else(|| MountSettings { path: path.to_owned(), ..MountSettings::default() })
    }

    /// Keeps `settings` for its folder, replacing what was there. Settings
    /// that are all defaults aren't written.
    pub fn set_mount_settings(&mut self, settings: MountSettings) {
        self.mounts.retain(|m| m.path != settings.path);
        if settings.owner.is_some() || settings.mode.is_some() {
            self.mounts.push(settings);
        }
    }

    /// Drops the settings of folders no mapping uses anymore.
    pub fn prune_mounts(&mut self) {
        let used: Vec<PathBuf> = self.mappings.iter().map(|m| m.mount.clone()).collect();
        self.mounts.retain(|m| used.contains(&m.path));
    }

    /// An id no mapping has yet.
    pub fn next_id(&self) -> u64 {
        self.mappings.iter().map(|m| m.id + 1).max().unwrap_or(1)
    }

    pub fn prompt_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.prompt_timeout_secs)
    }

    pub fn log_file(&self) -> PathBuf {
        self.log_file.clone().unwrap_or_else(default_log_file)
    }
}

fn dirs() -> ProjectDirs {
    ProjectDirs::from("br.com", "roboport", "readgate").expect("no home folder to keep readgate's files in")
}

/// `READGATE_CONFIG` overrides it, for trying things out.
pub fn default_config_file() -> PathBuf {
    std::env::var_os("READGATE_CONFIG").map(PathBuf::from).unwrap_or_else(|| dirs().config_dir().join("config.toml"))
}

pub fn default_log_file() -> PathBuf {
    data_dir().join("access.log")
}

/// readgate's data folder: the default log, the server's output, scratch
/// folders.
pub fn data_dir() -> PathBuf {
    dirs().data_dir().to_owned()
}

/// Where the files programs create in a mounted `folder` are kept: a folder
/// of readgate's per mounted folder, named after its path.
pub fn scratch_dir(folder: &Path) -> PathBuf {
    let name = folder.to_string_lossy().replace('%', "%25").replace('/', "%2F").replace('\\', "%5C");
    data_dir().join("scratch").join(name)
}
