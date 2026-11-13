//! A mount folder's owner and mode, from its settings' text to numbers.

use anyhow::{Context, Result, bail};

use crate::config::MountSettings;

/// The files' mode when a mount's settings leave it unset (its folders
/// then are `0777`).
pub const DEFAULT_MODE: u32 = 0o666;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Perms {
    pub uid: u32,
    pub gid: u32,
    /// The mode as set: the files', `0o7777` at most.
    pub mode: u32,
}

impl Perms {
    /// A mapped file's mode: the one set, without execute (or setuid,
    /// setgid and sticky), and without write when read-only.
    pub fn file_mode(self, read_only: bool) -> u32 {
        let mode = self.mode & 0o666;
        if read_only { mode & !0o222 } else { mode }
    }

    /// The mount folder's and its subfolders' mode: the one set, with
    /// execute for whoever can read, so whoever can read the files can get
    /// to them (`0660` gives `0770`, `0640` gives `0750`). Setgid and sticky
    /// are kept.
    pub fn dir_mode(self) -> u32 {
        let mode = self.mode & 0o7777;
        let read = mode & 0o444;
        mode | (read >> 2)
    }
}

pub fn resolve(settings: &MountSettings) -> Result<Perms> {
    let mode = match &settings.mode {
        Some(text) => parse_mode(text)?,
        None => DEFAULT_MODE,
    };
    let (uid, gid) = match &settings.owner {
        Some(text) => parse_owner(text)?,
        None => (current_uid(), current_gid()),
    };
    Ok(Perms { uid, gid, mode })
}

/// Octal, with or without a leading `0` or `0o`: `0660`, `660`, `0o660`.
pub fn parse_mode(text: &str) -> Result<u32> {
    let digits = text.trim();
    let digits = digits.strip_prefix("0o").unwrap_or(digits);
    let mode = u32::from_str_radix(digits, 8).with_context(|| format!("{text:?} isn't an octal mode like 0770"))?;
    if mode > 0o7777 {
        bail!("{text:?} is more than a mode can be (07777)");
    }
    Ok(mode)
}

/// `user:group`, `user` (and that user's primary group) or `:group`, each
/// by name or number. A missing user is the current one.
pub fn parse_owner(text: &str) -> Result<(u32, u32)> {
    let (user, group) = match text.trim().split_once(':') {
        Some((user, group)) => (user, Some(group)),
        None => (text.trim(), None),
    };
    let (uid, primary_gid) = if user.is_empty() {
        (current_uid(), current_gid())
    } else if let Ok(uid) = user.parse::<u32>() {
        (uid, platform::user_by_id(uid).map_or(current_gid(), |(_, gid)| gid))
    } else {
        platform::user_by_name(user).with_context(|| format!("there's no user {user:?}"))?
    };
    let gid = match group {
        None | Some("") => primary_gid,
        Some(group) => match group.parse::<u32>() {
            Ok(gid) => gid,
            Err(_) => platform::group_by_name(group).with_context(|| format!("there's no group {group:?}"))?,
        },
    };
    Ok((uid, gid))
}

#[cfg(unix)]
fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

#[cfg(unix)]
fn current_gid() -> u32 {
    unsafe { libc::getgid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

#[cfg(not(unix))]
fn current_gid() -> u32 {
    0
}

#[cfg(unix)]
mod platform {
    use std::ffi::CString;

    // The _r versions: these run on several threads.

    pub fn user_by_name(name: &str) -> Option<(u32, u32)> {
        let name = CString::new(name).ok()?;
        lookup_user(|pwd, buf, result| unsafe {
            libc::getpwnam_r(name.as_ptr(), pwd, buf.as_mut_ptr(), buf.len(), result)
        })
    }

    pub fn user_by_id(uid: u32) -> Option<(u32, u32)> {
        lookup_user(|pwd, buf, result| unsafe { libc::getpwuid_r(uid, pwd, buf.as_mut_ptr(), buf.len(), result) })
    }

    fn lookup_user(
        call: impl FnOnce(&mut libc::passwd, &mut [libc::c_char], &mut *mut libc::passwd) -> libc::c_int,
    ) -> Option<(u32, u32)> {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0 as libc::c_char; 4096];
        let mut result = std::ptr::null_mut();
        if call(&mut pwd, &mut buf, &mut result) != 0 || result.is_null() {
            return None;
        }
        Some((pwd.pw_uid, pwd.pw_gid))
    }

    pub fn group_by_name(name: &str) -> Option<u32> {
        let name = CString::new(name).ok()?;
        let mut grp: libc::group = unsafe { std::mem::zeroed() };
        // Groups with many members need room for them all.
        let mut buf = vec![0 as libc::c_char; 64 * 1024];
        let mut result = std::ptr::null_mut();
        let rc = unsafe { libc::getgrnam_r(name.as_ptr(), &mut grp, buf.as_mut_ptr(), buf.len(), &mut result) };
        if rc != 0 || result.is_null() {
            return None;
        }
        Some(grp.gr_gid)
    }
}

#[cfg(not(unix))]
mod platform {
    pub fn user_by_name(_name: &str) -> Option<(u32, u32)> {
        None
    }

    pub fn user_by_id(_uid: u32) -> Option<(u32, u32)> {
        None
    }

    pub fn group_by_name(_name: &str) -> Option<u32> {
        None
    }
}
