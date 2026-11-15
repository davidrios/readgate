//! Who is reading: everything the platform tells about the process behind
//! a request.
//!
//! The kernel only hands the filesystem a pid, uid and gid; the rest is
//! looked up from the pid right away, while the reader is still blocked in
//! its read and so can't have exited (its pid can't have been reused).
//!
//! Some of it the kernel vouches for, and some the process says about
//! itself. A process's executable and working folder (Linux's
//! `/proc/<pid>/exe` and `cwd`) are the kernel's, but only readable for a
//! process of the same user, or by root. Its name and command line, what
//! `ps` shows, anyone can read, and the process can set them to anything.
//! For another user's process only the second kind is there, and it's
//! shown as such.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReaderInfo {
    pub pid: u32,
    /// When it started, in clock ticks since boot: with the pid, it names
    /// this process and no later one that gets the pid again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<u64>,
    pub uid: u32,
    pub gid: u32,
    /// The uid's user name, when it has one.
    pub user: Option<String>,
    /// The executable's path, from the kernel: unset for another user's
    /// process.
    pub exe: Option<PathBuf>,
    /// Its name as `ps` shows it (Linux's `comm`, at most 15 bytes), which
    /// it can change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The command line, argument by argument.
    pub cmdline: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Its parents, closest first, up to the first process (or as far as
    /// the platform lets us see): what started the reader, often a shell or
    /// a service manager.
    pub ancestors: Vec<Ancestor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ancestor {
    pub pid: u32,
    pub exe: Option<PathBuf>,
    /// As [`ReaderInfo::name`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cmdline: Vec<String>,
    /// Its real user, which can differ from the reader's (`sudo`, `su`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    /// As [`ReaderInfo::start`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<u64>,
}

impl ReaderInfo {
    pub fn of(pid: u32, uid: u32, gid: u32) -> ReaderInfo {
        let mut info = ReaderInfo { pid, uid, gid, user: user_name(uid), ..ReaderInfo::default() };
        platform::fill(&mut info);
        info
    }

    /// A short name for lists: the executable's file name, or, when that's
    /// out of sight, the name the process gives itself, or the pid.
    pub fn program(&self) -> String {
        short_name(self.exe.as_ref(), &self.cmdline, self.name.as_deref())
            .unwrap_or_else(|| format!("pid {}", self.pid))
    }

    pub fn user_label(&self) -> String {
        match &self.user {
            Some(name) => format!("{name} ({})", self.uid),
            None => self.uid.to_string(),
        }
    }

    /// Everything, one fact per line, for the prompt.
    pub fn describe(&self) -> String {
        let mut claimed = false;
        let program = match (&self.exe, self.cmdline.first().or(self.name.as_ref())) {
            (Some(exe), _) => exe.display().to_string(),
            (None, Some(said)) => {
                claimed = true;
                format!("{said} *")
            }
            (None, None) => "unknown".into(),
        };
        let cwd = match &self.cwd {
            Some(cwd) => cwd.display().to_string(),
            None if self.exe.is_none() => "not visible (another user's process)".into(),
            None => "unknown".into(),
        };
        let mut lines = vec![
            format!("Program: {program}"),
            format!(
                "Command line: {}",
                if self.cmdline.is_empty() { "unknown".into() } else { self.cmdline.join(" ") }
            ),
            format!("Process id: {}", self.pid),
            format!("User: {}", self.user_label()),
            format!("Group id: {}", self.gid),
            format!("Working folder: {cwd}"),
        ];
        if !self.ancestors.is_empty() {
            lines.push("Started by:".into());
            for a in &self.ancestors {
                let what = match (&a.exe, a.cmdline.is_empty()) {
                    // The kernel's path, and the arguments it was given.
                    (Some(exe), true) => exe.display().to_string(),
                    (Some(exe), false) => format!("{} {}", exe.display(), a.cmdline[1..].join(" ")),
                    (None, false) => {
                        claimed = true;
                        format!("{} *", a.cmdline.join(" "))
                    }
                    (None, true) => match &a.name {
                        Some(name) => {
                            claimed = true;
                            format!("{name} *")
                        }
                        None => "unknown".into(),
                    },
                };
                let user = match a.uid {
                    Some(uid) if uid != self.uid => {
                        format!(", user {}", user_name(uid).unwrap_or_else(|| uid.to_string()))
                    }
                    _ => String::new(),
                };
                lines.push(format!("  {} (pid {}{user})", what.trim_end(), a.pid));
            }
        }
        if claimed {
            lines.push(String::new());
            lines.push(
                "* As the process names itself, which it can change: the kernel only shows another user's \
                 executable to root."
                    .into(),
            );
        }
        lines.join("\n")
    }
}

/// The executable's file name, else the first argument's, else the name.
fn short_name(exe: Option<&PathBuf>, cmdline: &[String], name: Option<&str>) -> Option<String> {
    let file_name = |p: &std::path::Path| p.file_name().map(|n| n.to_string_lossy().into_owned());
    exe.and_then(|e| file_name(e))
        .or_else(|| cmdline.first().and_then(|a| file_name(std::path::Path::new(a))))
        .or_else(|| name.map(str::to_owned))
}

/// Programs that only run other programs for someone: shells, and
/// wrappers that run one command and exit with it. "Allow for 1 Minute"'s
/// menu leaves them out, since each command an agent runs gets a fresh one
/// and a grant to it would cover nothing after.
const PASS_THROUGH: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "mksh", "tcsh", "csh", "nu", "elvish", "xonsh", "busybox", "timeout",
    "env",
];

impl ReaderInfo {
    /// The ancestors past the parent that "Allow for 1 Minute"'s menu
    /// offers, with their places in `ancestors`: each one up the chain
    /// that isn't a shell or a wrapper. It stops before pid 1, before
    /// another user's process (`sudo`, `sshd`), and before the root of the
    /// reader's session, whose parent is one of those (a terminal
    /// multiplexer's server, the user's service manager): a grant to it
    /// would cover everything the user runs.
    pub fn grant_choices(&self) -> Vec<(usize, &Ancestor)> {
        // An unknown user means the process is gone: stop there too.
        let outside = |a: &Ancestor| a.pid == 1 || a.uid != Some(self.uid);
        let mut choices = Vec::new();
        for (at, a) in self.ancestors.iter().enumerate() {
            if outside(a) || self.ancestors.get(at + 1).is_none_or(outside) {
                break;
            }
            // The parent is the button's own choice.
            if at > 0 && !a.passes_through() {
                choices.push((at, a));
            }
        }
        choices
    }
}

impl Ancestor {
    /// By its name: a starred one (another user's) counts as it calls
    /// itself.
    pub fn passes_through(&self) -> bool {
        self.program().is_some_and(|p| PASS_THROUGH.contains(&p.trim_start_matches('-')))
    }

    /// "bash (pid 4242)", for the prompt.
    pub fn label(&self) -> String {
        format!("{} (pid {})", self.program().unwrap_or_else(|| "unknown".into()), self.pid)
    }

    /// As [`ReaderInfo::program`].
    pub fn program(&self) -> Option<String> {
        short_name(self.exe.as_ref(), &self.cmdline, self.name.as_deref())
    }
}

#[cfg(unix)]
fn user_name(uid: u32) -> Option<String> {
    use std::ffi::CStr;
    // getpwuid isn't thread-safe; requests come in on many threads.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut result = std::ptr::null_mut();
    let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
    if rc != 0 || result.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr(pwd.pw_name) }.to_string_lossy().into_owned())
}

#[cfg(not(unix))]
fn user_name(_uid: u32) -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
mod platform {
    use std::fs;
    use std::path::PathBuf;

    use super::{Ancestor, ReaderInfo};

    pub fn fill(info: &mut ReaderInfo) {
        let proc = PathBuf::from(format!("/proc/{}", info.pid));
        info.start = start_of(info.pid);
        info.exe = fs::read_link(proc.join("exe")).ok();
        info.cwd = fs::read_link(proc.join("cwd")).ok();
        info.name = name_of(info.pid);
        info.cmdline = cmdline_of(info.pid);
        let mut pid = info.pid;
        // Bounded, in case /proc changes under us into a loop.
        for _ in 0..32 {
            let Some(parent) = parent_of(pid) else { break };
            if parent == 0 {
                break;
            }
            info.ancestors.push(Ancestor {
                pid: parent,
                exe: fs::read_link(format!("/proc/{parent}/exe")).ok(),
                name: name_of(parent),
                cmdline: cmdline_of(parent),
                uid: uid_of(parent),
                start: start_of(parent),
            });
            pid = parent;
        }
    }

    /// Field 22 of `/proc/<pid>/stat`, after the command's name, which is in
    /// parentheses and may hold spaces and parentheses itself.
    fn start_of(pid: u32) -> Option<u64> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_name = &stat[stat.rfind(')')? + 1..];
        after_name.split_whitespace().nth(19)?.parse().ok()
    }

    /// What `ps` shows as its name: anyone can read it.
    fn name_of(pid: u32) -> Option<String> {
        fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|n| n.trim_end().to_owned()).filter(|n| !n.is_empty())
    }

    /// Argument by argument: anyone can read it (`ps aux`'s COMMAND).
    fn cmdline_of(pid: u32) -> Vec<String> {
        fs::read(format!("/proc/{pid}/cmdline"))
            .map(|raw| {
                raw.split(|b| *b == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The real uid, the first of `Uid:`'s four.
    fn uid_of(pid: u32) -> Option<u32> {
        let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        status.lines().find_map(|l| l.strip_prefix("Uid:"))?.split_whitespace().next()?.parse().ok()
    }

    fn parent_of(pid: u32) -> Option<u32> {
        let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        status.lines().find_map(|l| l.strip_prefix("PPid:")).and_then(|v| v.trim().parse().ok())
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::ReaderInfo;

    // TODO: cmdline (sysctl KERN_PROCARGS2), cwd (proc_pidinfo
    // PROC_PIDVNODEPATHINFO) and parents (proc_pidinfo PROC_PIDTBSDINFO).
    pub fn fill(info: &mut ReaderInfo) {
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let n = unsafe { libc::proc_pidpath(info.pid as i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
        if n > 0 {
            buf.truncate(n as usize);
            info.exe = Some(String::from_utf8_lossy(&buf).into_owned().into());
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use super::ReaderInfo;

    // TODO: Windows: QueryFullProcessImageNameW, the PEB's command line,
    // and the token's user SID.
    pub fn fill(_info: &mut ReaderInfo) {}
}
