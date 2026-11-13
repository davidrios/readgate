//! A FUSE filesystem that is a folder of gated files: every mapping that
//! names this mount, at its dest inside, plus the files programs create
//! there (an editor's temporary file while it saves).
//!
//! The folders on the way to a dest (`keys/` for `keys/deploy`) are there
//! because a dest is in them: programs can't make or remove folders.
//!
//! The mount's owner is the folder's, its subfolders' and its mapped files'.
//! Its mode is the files' (without write when read-only); folders add
//! execute wherever there's read, so they can be got through. The kernel checks them
//! (`default_permissions`) before readgate sees a request: someone they
//! don't let in gets `EACCES`, without a prompt. A created file belongs to
//! whoever created it, kept in an extended attribute on its scratch copy.
//!
//! Opening a mapped file always succeeds, so programs that only look (file
//! managers, indexers calling `stat` or `open`) never prompt. The first
//! read, write or truncation on an open file asks; the answer holds for
//! that open file until it's closed. Every open asks again. Pages are never
//! cached (`FOPEN_DIRECT_IO`), so data the user allowed never serves
//! another reader from the cache.
//!
//! Files programs create are kept in a scratch folder of readgate's, at
//! the same path inside it, and aren't gated: they're the programs' own.
//! Renaming one over a mapped file is how most editors save, so that rename
//! is a write, and asks. If allowed, readgate copies the file's content
//! into the source, which keeps the source's inode, owner and mode. Mapped
//! files themselves can't be deleted, renamed away or have their mode
//! changed.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, InitFlags,
    KernelConfig, LockOwner, MountOption, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, SessionACL, TimeOrNow, WriteFlags,
};

use crate::access_log::{Access, Decision};
use crate::config::Mapping;
use crate::gate::Gate;
use crate::perms::Perms;
use crate::reader::ReaderInfo;

use super::Shared;

/// Attributes and names may be cached briefly; contents never are.
const TTL: Duration = Duration::from_secs(1);

/// The mount folder itself.
const ROOT: u64 = 1;
/// A mapped file's inode is its mapping's id plus this.
const MAPPED_BASE: u64 = 2;
/// Folders' and created files' inodes start here, past any mapping's.
const PATH_BASE: u64 = 1 << 48;

/// An open mapped file: who opened it, for what, and the user's answer
/// once there is one. The mutex is held while asking, so operations racing
/// on one open file prompt once, and wait for that answer.
struct Handle {
    mapping: Mapping,
    reader: ReaderInfo,
    access: Access,
    decision: Mutex<Option<Decision>>,
}

impl Handle {
    fn new(mapping: Mapping, reader: ReaderInfo, access: Access) -> Handle {
        Handle { mapping, reader, access, decision: Mutex::new(None) }
    }

    /// Asks the first time; the same answer after.
    fn decide(&self, gate: &Gate) -> Decision {
        let mut decision = self.decision.lock().unwrap();
        *decision.get_or_insert_with(|| gate.ask(self.mapping.clone(), self.reader.clone(), self.access))
    }
}

enum Open {
    Mapped(Arc<Handle>),
    Created(Arc<File>),
}

/// Inodes for folders and created files, by their path inside the mount
/// (the root is the empty path). Mapped files' come from their ids.
struct Inodes {
    by_path: HashMap<PathBuf, u64>,
    by_ino: HashMap<u64, PathBuf>,
    next: u64,
}

impl Inodes {
    fn new() -> Inodes {
        let mut inodes = Inodes { by_path: HashMap::new(), by_ino: HashMap::new(), next: PATH_BASE };
        inodes.by_path.insert(PathBuf::new(), ROOT);
        inodes.by_ino.insert(ROOT, PathBuf::new());
        inodes
    }

    fn of(&mut self, path: &Path) -> u64 {
        if let Some(ino) = self.by_path.get(path) {
            return *ino;
        }
        let ino = self.next;
        self.next += 1;
        self.by_path.insert(path.to_owned(), ino);
        self.by_ino.insert(ino, path.to_owned());
        ino
    }

    fn forget(&mut self, path: &Path) -> Option<u64> {
        let ino = self.by_path.remove(path)?;
        self.by_ino.remove(&ino);
        Some(ino)
    }

    /// The inode moves with its file.
    fn rename(&mut self, from: &Path, to: &Path) {
        self.forget(to);
        if let Some(ino) = self.forget(from) {
            self.by_path.insert(to.to_owned(), ino);
            self.by_ino.insert(ino, to.to_owned());
        }
    }
}

enum Node {
    /// The root, or a folder on the way to a dest.
    Dir(PathBuf),
    Mapped(Mapping),
    Created(PathBuf),
}

struct GatedFolder {
    mappings: Shared,
    gate: Gate,
    scratch: PathBuf,
    inodes: Arc<Mutex<Inodes>>,
    /// A created file renamed over a mapped one keeps its inode in the
    /// kernel's cache for a moment: it's that mapped file now.
    aliases: Arc<Mutex<HashMap<u64, u64>>>,
    handles: Mutex<HashMap<u64, Open>>,
    next_handle: AtomicU64,
    perms: Perms,
}

impl GatedFolder {
    fn mapping_at(&self, path: &Path) -> Option<Mapping> {
        self.mappings.read().unwrap().iter().find(|m| m.dest == path).cloned()
    }

    fn mapping_by_id(&self, id: u64) -> Option<Mapping> {
        self.mappings.read().unwrap().iter().find(|m| m.id == id).cloned()
    }

    /// The root, or a folder some dest is in.
    fn is_dir(&self, path: &Path) -> bool {
        path.as_os_str().is_empty()
            || self.mappings.read().unwrap().iter().any(|m| m.dest.starts_with(path) && m.dest != path)
    }

    /// Programs may create files here only if a mapping here is writable:
    /// a mount of read-only files stays read-only.
    fn writable(&self) -> bool {
        self.mappings.read().unwrap().iter().any(|m| !m.read_only)
    }

    fn created_path(&self, path: &Path) -> PathBuf {
        self.scratch.join(path)
    }

    /// What's at `path`: a mapped file wins over a folder, and both over a
    /// created file.
    fn node_at(&self, path: &Path) -> Option<(u64, Node)> {
        if let Some(m) = self.mapping_at(path) {
            return Some((m.id + MAPPED_BASE, Node::Mapped(m)));
        }
        if self.is_dir(path) {
            return Some((self.inodes.lock().unwrap().of(path), Node::Dir(path.to_owned())));
        }
        if self.created_path(path).is_file() {
            return Some((self.inodes.lock().unwrap().of(path), Node::Created(path.to_owned())));
        }
        None
    }

    fn node(&self, ino: u64) -> Option<Node> {
        if let Some(id) = self.aliases.lock().unwrap().get(&ino) {
            return self.mapping_by_id(*id).map(Node::Mapped);
        }
        if (MAPPED_BASE..PATH_BASE).contains(&ino) {
            return self.mapping_by_id(ino - MAPPED_BASE).map(Node::Mapped);
        }
        let path = self.inodes.lock().unwrap().by_ino.get(&ino)?.clone();
        if self.is_dir(&path) {
            Some(Node::Dir(path))
        } else if self.created_path(&path).is_file() {
            Some(Node::Created(path))
        } else {
            None
        }
    }

    /// The folder `ino` is, for an operation on a name in it.
    fn dir(&self, ino: INodeNo) -> Result<PathBuf, Errno> {
        match self.node(ino.0) {
            Some(Node::Dir(path)) => Ok(path),
            Some(_) => Err(Errno::ENOTDIR),
            None => Err(Errno::ENOENT),
        }
    }

    fn attr(&self, ino: u64, node: &Node) -> Result<FileAttr, Errno> {
        let (meta, perm, owner) = match node {
            Node::Dir(_) => {
                let perm = self.perms.dir_mode() as u16;
                return Ok(self.make_attr(ino, 0, SystemTime::now(), FileType::Directory, perm, 2));
            }
            Node::Mapped(m) => {
                let meta = fs::metadata(&m.source).map_err(|_| Errno::EIO)?;
                (meta, self.perms.file_mode(m.read_only) as u16, None)
            }
            Node::Created(path) => {
                let path = self.created_path(path);
                let meta = fs::metadata(&path).map_err(|_| Errno::ENOENT)?;
                let perm = (meta.mode() & 0o777) as u16;
                (meta, perm, owner_of(&path))
            }
        };
        let mtime = time(meta.mtime(), meta.mtime_nsec());
        let mut attr = self.make_attr(ino, meta.len(), mtime, FileType::RegularFile, perm, 1);
        if let Some((uid, gid)) = owner {
            (attr.uid, attr.gid) = (uid, gid);
        }
        attr.atime = time(meta.atime(), meta.atime_nsec());
        attr.ctime = time(meta.ctime(), meta.ctime_nsec());
        Ok(attr)
    }

    fn make_attr(&self, ino: u64, size: u64, mtime: SystemTime, kind: FileType, perm: u16, nlink: u32) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: UNIX_EPOCH,
            kind,
            perm,
            nlink,
            uid: self.perms.uid,
            gid: self.perms.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn reply_attr(&self, ino: u64, reply: ReplyAttr) {
        match self.node(ino).ok_or(Errno::ENOENT).and_then(|n| self.attr(ino, &n)) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(e) => reply.error(e),
        }
    }

    fn open_handle(&self, open: Open) -> FileHandle {
        let fh = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().unwrap().insert(fh, open);
        FileHandle(fh)
    }

    fn mapped_handle(&self, fh: FileHandle) -> Option<Arc<Handle>> {
        match self.handles.lock().unwrap().get(&fh.0)? {
            Open::Mapped(h) => Some(h.clone()),
            Open::Created(_) => None,
        }
    }

    fn created_file(&self, fh: FileHandle) -> Option<Arc<File>> {
        match self.handles.lock().unwrap().get(&fh.0)? {
            Open::Created(f) => Some(f.clone()),
            Open::Mapped(_) => None,
        }
    }

    /// Asks for `handle` off the session's thread (a prompt can take
    /// minutes, and other requests, another reader's `getattr` or a
    /// `release`, must go on), then runs `work` with whether it was allowed.
    fn when_allowed(&self, handle: Arc<Handle>, work: impl FnOnce(bool) + Send + 'static) {
        let gate = self.gate.clone();
        std::thread::spawn(move || work(handle.decide(&gate).allows()));
    }

    /// What's in the folder at `dir`, by name: mapped files and folders
    /// first, then created files of other names.
    fn entries(&self, dir: &Path) -> BTreeMap<OsString, (u64, FileType)> {
        let mut entries = BTreeMap::new();
        for m in self.mappings.read().unwrap().iter() {
            let Ok(rest) = m.dest.strip_prefix(dir) else { continue };
            let mut parts = rest.components();
            let Some(first) = parts.next() else { continue };
            let name = first.as_os_str().to_owned();
            if parts.next().is_none() {
                entries.insert(name, (m.id + MAPPED_BASE, FileType::RegularFile));
            } else {
                let ino = self.inodes.lock().unwrap().of(&dir.join(&name));
                entries.entry(name).or_insert((ino, FileType::Directory));
            }
        }
        for entry in fs::read_dir(self.created_path(dir)).into_iter().flatten().flatten() {
            let name = entry.file_name();
            if entry.file_type().is_ok_and(|t| t.is_file()) && !entries.contains_key(&name) {
                let ino = self.inodes.lock().unwrap().of(&dir.join(&name));
                entries.insert(name, (ino, FileType::RegularFile));
            }
        }
        entries
    }
}

fn time(secs: i64, nanos: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64) + Duration::from_nanos(nanos.max(0) as u64)
}

fn access_of(flags: i32) -> Access {
    match OpenFlags(flags).acc_mode() {
        OpenAccMode::O_RDONLY => Access::Read,
        OpenAccMode::O_WRONLY => Access::Write,
        OpenAccMode::O_RDWR => Access::ReadWrite,
    }
}

fn errno(e: &std::io::Error) -> Errno {
    Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))
}

/// Opens a created file as the program asked.
fn open_created(path: &Path, flags: i32, create: bool, mode: u32) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let access = access_of(flags);
    OpenOptions::new()
        .read(access != Access::Write)
        .write(access != Access::Read)
        .append(flags & libc::O_APPEND != 0)
        .truncate(flags & libc::O_TRUNC != 0)
        .create(create)
        .mode(mode)
        .open(path)
}

impl Filesystem for GatedFolder {
    /// `open(O_TRUNC)` comes as one open with `O_TRUNC` in its flags,
    /// instead of an open and then a truncation, so it asks once.
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        let _ = config.add_capabilities(InitFlags::FUSE_ATOMIC_O_TRUNC);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let found = self
            .dir(parent)
            .and_then(|dir| self.node_at(&dir.join(name)).ok_or(Errno::ENOENT))
            .and_then(|(ino, node)| self.attr(ino, &node));
        match found {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.reply_attr(ino.0, reply);
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let dir = match self.dir(ino) {
            Ok(dir) => dir,
            Err(e) => return reply.error(e),
        };
        let parent = match dir.parent() {
            Some(up) => self.inodes.lock().unwrap().of(up),
            None => ROOT,
        };
        let listed = [(ino.0, FileType::Directory, OsString::from(".")), (parent, FileType::Directory, "..".into())]
            .into_iter()
            .chain(self.entries(&dir).into_iter().map(|(name, (ino, kind))| (ino, kind, name)));
        for (i, (ino, kind, name)) in listed.enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), i as u64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let access = access_of(flags.0);
        match self.node(ino.0) {
            None => reply.error(Errno::ENOENT),
            Some(Node::Dir(_)) => reply.error(Errno::EISDIR),
            Some(Node::Created(path)) => match open_created(&self.created_path(&path), flags.0, false, 0) {
                Ok(file) => reply.opened(self.open_handle(Open::Created(Arc::new(file))), FopenFlags::FOPEN_DIRECT_IO),
                Err(e) => reply.error(errno(&e)),
            },
            Some(Node::Mapped(mapping)) => {
                if access != Access::Read && mapping.read_only {
                    return reply.error(Errno::EROFS);
                }
                // Looked up now: the opener is blocked in open(), so its
                // pid is still its own.
                let reader = ReaderInfo::of(req.pid(), req.uid(), req.gid());
                let handle = Arc::new(Handle::new(mapping, reader, access));
                if access == Access::Read || flags.0 & libc::O_TRUNC == 0 {
                    return reply.opened(self.open_handle(Open::Mapped(handle)), FopenFlags::FOPEN_DIRECT_IO);
                }
                // Truncating is writing: this open asks now, and only opens
                // (on the truncated file) if allowed. Its answer holds for
                // its writes.
                let fh = self.open_handle(Open::Mapped(handle.clone()));
                let source = handle.mapping.source.clone();
                self.when_allowed(handle, move |allowed| {
                    let truncated =
                        allowed && OpenOptions::new().write(true).open(&source).and_then(|s| s.set_len(0)).is_ok();
                    if truncated {
                        reply.opened(fh, FopenFlags::FOPEN_DIRECT_IO);
                    } else {
                        reply.error(Errno::EIO);
                    }
                });
            }
        }
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let dir = match self.dir(parent) {
            Ok(dir) => dir,
            Err(e) => return reply.error(e),
        };
        if !self.writable() {
            return reply.error(Errno::EROFS);
        }
        let path = dir.join(name);
        if matches!(self.node_at(&path), Some((_, Node::Mapped(_) | Node::Dir(_)))) {
            return reply.error(Errno::EEXIST);
        }
        let opened = private_dir_all(&self.created_path(&dir))
            .and_then(|()| open_created(&self.created_path(&path), flags, true, mode & !umask & PLAIN_MODE));
        let file = match opened {
            Ok(file) => file,
            Err(e) => return reply.error(errno(&e)),
        };
        // The creator owns it, as on any filesystem.
        set_owner(&self.created_path(&path), req.uid(), req.gid());
        let ino = self.inodes.lock().unwrap().of(&path);
        match self.attr(ino, &Node::Created(path)) {
            Ok(attr) => {
                let fh = self.open_handle(Open::Created(Arc::new(file)));
                reply.created(&TTL, &attr, Generation(0), fh, FopenFlags::FOPEN_DIRECT_IO);
            }
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        if let Some(file) = self.created_file(fh) {
            return match read_at(&file, offset, size) {
                Ok(data) => reply.data(&data),
                Err(e) => reply.error(errno(&e)),
            };
        }
        let Some(handle) = self.mapped_handle(fh) else { return reply.error(Errno::EBADF) };
        let source = handle.mapping.source.clone();
        self.when_allowed(handle, move |allowed| {
            match allowed.then(|| File::open(&source).and_then(|s| read_at(&s, offset, size))) {
                Some(Ok(data)) => reply.data(&data),
                // Denied, or the source failed. To the reader, either is
                // what a failing disk returns: a plain I/O error.
                _ => reply.error(Errno::EIO),
            }
        });
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        if let Some(file) = self.created_file(fh) {
            // An append-mode file writes at its end, whatever the offset.
            let written = if flags.0 & libc::O_APPEND != 0 {
                use std::io::Write;
                (&*file).write_all(data)
            } else {
                file.write_all_at(data, offset)
            };
            return match written {
                Ok(()) => reply.written(data.len() as u32),
                Err(e) => reply.error(errno(&e)),
            };
        }
        let Some(handle) = self.mapped_handle(fh) else { return reply.error(Errno::EBADF) };
        if handle.mapping.read_only || handle.access == Access::Read {
            return reply.error(Errno::EBADF);
        }
        let source = handle.mapping.source.clone();
        let data = data.to_vec();
        self.when_allowed(handle, move |allowed| {
            let write = || OpenOptions::new().write(true).open(&source)?.write_all_at(&data, offset);
            match allowed.then(write) {
                Some(Ok(())) => reply.written(data.len() as u32),
                _ => reply.error(Errno::EIO),
            }
        });
    }

    /// On a mapped file only truncation does anything: it's a write, and
    /// asks like one. Times are accepted and dropped (the source keeps its
    /// own, so `touch` works), and owner and mode can't change. A created
    /// file takes a new size and mode. Folders don't change.
    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let ino = ino.0;
        // Only root may chown, and the kernel already refused anyone else;
        // readgate (not root) can't give its files away either.
        let changes_owner = uid.is_some_and(|u| u != self.perms.uid) || gid.is_some_and(|g| g != self.perms.gid);
        match self.node(ino) {
            None => reply.error(Errno::ENOENT),
            Some(Node::Dir(_)) => {
                if mode.is_some() || changes_owner || size.is_some() {
                    reply.error(Errno::EPERM)
                } else {
                    self.reply_attr(ino, reply)
                }
            }
            Some(Node::Created(path)) => {
                let owner = owner_of(&self.created_path(&path)).unwrap_or((self.perms.uid, self.perms.gid));
                if uid.is_some_and(|u| u != owner.0) || gid.is_some_and(|g| g != owner.1) {
                    return reply.error(Errno::EPERM);
                }
                let path = self.created_path(&path);
                let applied = (|| {
                    if let Some(size) = size {
                        OpenOptions::new().write(true).open(&path)?.set_len(size)?;
                    }
                    if let Some(mode) = mode {
                        fs::set_permissions(&path, fs::Permissions::from_mode(mode & PLAIN_MODE))?;
                    }
                    Ok::<(), std::io::Error>(())
                })();
                match applied {
                    Ok(()) => self.reply_attr(ino, reply),
                    Err(e) => reply.error(errno(&e)),
                }
            }
            Some(Node::Mapped(mapping)) => {
                if mode.is_some() || changes_owner {
                    return reply.error(Errno::EPERM);
                }
                let Some(size) = size else { return self.reply_attr(ino, reply) };
                if mapping.read_only {
                    return reply.error(Errno::EROFS);
                }
                // Through an open file, it shares that file's answer;
                // `truncate(2)` on the path has none, and asks by itself.
                let handle = match fh.and_then(|fh| self.mapped_handle(fh)) {
                    Some(handle) if handle.access == Access::Read => return reply.error(Errno::EBADF),
                    Some(handle) => handle,
                    None => {
                        let reader = ReaderInfo::of(req.pid(), req.uid(), req.gid());
                        Arc::new(Handle::new(mapping.clone(), reader, Access::Write))
                    }
                };
                let attr = self.attr(ino, &Node::Mapped(mapping.clone()));
                self.when_allowed(handle, move |allowed| {
                    let truncated = allowed
                        && OpenOptions::new().write(true).open(&mapping.source).and_then(|s| s.set_len(size)).is_ok();
                    match attr {
                        Ok(mut attr) if truncated => {
                            attr.size = size;
                            attr.blocks = size.div_ceil(512);
                            reply.attr(&TTL, &attr);
                        }
                        _ => reply.error(Errno::EIO),
                    }
                });
            }
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let path = match self.dir(parent) {
            Ok(dir) => dir.join(name),
            Err(e) => return reply.error(e),
        };
        match self.node_at(&path) {
            None => reply.error(Errno::ENOENT),
            Some((_, Node::Mapped(_))) => reply.error(Errno::EPERM),
            Some((_, Node::Dir(_))) => reply.error(Errno::EISDIR),
            Some((_, Node::Created(_))) => match fs::remove_file(self.created_path(&path)) {
                Ok(()) => {
                    self.inodes.lock().unwrap().forget(&path);
                    reply.ok();
                }
                Err(e) => reply.error(errno(&e)),
            },
        }
    }

    fn rename(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let (from, to) = match (self.dir(parent), self.dir(newparent)) {
            (Ok(a), Ok(b)) => (a.join(name), b.join(newname)),
            (Err(e), _) | (_, Err(e)) => return reply.error(e),
        };
        if flags.contains(RenameFlags::RENAME_EXCHANGE) || flags.contains(RenameFlags::RENAME_WHITEOUT) {
            return reply.error(Errno::EINVAL);
        }
        let ino = match self.node_at(&from) {
            None => return reply.error(Errno::ENOENT),
            // Mapped files and their folders stay where they are.
            Some((_, Node::Mapped(_) | Node::Dir(_))) => return reply.error(Errno::EPERM),
            Some((ino, Node::Created(_))) => ino,
        };
        let no_replace = flags.contains(RenameFlags::RENAME_NOREPLACE);
        let mapping = match self.node_at(&to) {
            Some((_, Node::Dir(_))) => return reply.error(Errno::EISDIR),
            Some((_, Node::Mapped(mapping))) => mapping,
            target => {
                // Between created files: a plain rename in the scratch
                // folder.
                if no_replace && target.is_some() {
                    return reply.error(Errno::EEXIST);
                }
                let moved = private_dir_all(&self.created_path(to.parent().unwrap_or(Path::new(""))))
                    .and_then(|()| fs::rename(self.created_path(&from), self.created_path(&to)));
                return match moved {
                    Ok(()) => {
                        self.inodes.lock().unwrap().rename(&from, &to);
                        reply.ok();
                    }
                    Err(e) => reply.error(errno(&e)),
                };
            }
        };

        // A save over a mapped file: it writes the source, so it asks.
        if no_replace {
            return reply.error(Errno::EEXIST);
        }
        if mapping.read_only {
            return reply.error(Errno::EROFS);
        }
        let reader = ReaderInfo::of(req.pid(), req.uid(), req.gid());
        let handle = Arc::new(Handle::new(mapping.clone(), reader, Access::Write));
        let (inodes, aliases, created) = (self.inodes.clone(), self.aliases.clone(), self.created_path(&from));
        self.when_allowed(handle, move |allowed| {
            if !allowed || copy_into(&created, &mapping.source).is_err() {
                return reply.error(Errno::EIO);
            }
            let _ = fs::remove_file(&created);
            inodes.lock().unwrap().forget(&from);
            aliases.lock().unwrap().insert(ino, mapping.id);
            reply.ok();
        });
    }

    fn mkdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        // Folders are there for the dests in them; programs don't make any.
        reply.error(Errno::EPERM);
    }

    fn rmdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EPERM);
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, datasync: bool, reply: ReplyEmpty) {
        let synced = match (self.created_file(fh), self.mapped_handle(fh)) {
            (Some(file), _) => {
                if datasync {
                    file.sync_data()
                } else {
                    file.sync_all()
                }
            }
            // Writes went straight to the source; flush them to its disk.
            (None, Some(h)) => File::open(&h.mapping.source).and_then(|s| s.sync_all()),
            (None, None) => return reply.error(Errno::EBADF),
        };
        match synced {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.handles.lock().unwrap().remove(&fh.0);
        reply.ok();
    }
}

/// Where a created file's owner is kept, on its scratch copy (which is
/// readgate's on disk).
const OWNER_XATTR: &[u8] = b"user.readgate.owner\0";

fn set_owner(path: &Path, uid: u32, gid: u32) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return };
    let value = format!("{uid}:{gid}");
    // Without xattrs (a filesystem that has none), it's readgate's.
    unsafe {
        libc::setxattr(path.as_ptr(), OWNER_XATTR.as_ptr().cast(), value.as_ptr().cast(), value.len(), 0);
    }
}

fn owner_of(path: &Path) -> Option<(u32, u32)> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf = [0u8; 32];
    let n = unsafe { libc::getxattr(path.as_ptr(), OWNER_XATTR.as_ptr().cast(), buf.as_mut_ptr().cast(), buf.len()) };
    let text = std::str::from_utf8(buf.get(..usize::try_from(n).ok()?)?).ok()?;
    let (uid, gid) = text.split_once(':')?;
    Some((uid.parse().ok()?, gid.parse().ok()?))
}

fn read_at(file: &File, offset: u64, size: u32) -> std::io::Result<Vec<u8>> {
    let mut data = vec![0; size as usize];
    let mut filled = 0;
    while filled < data.len() {
        match file.read_at(&mut data[filled..], offset + filled as u64)? {
            0 => break,
            n => filled += n,
        }
    }
    data.truncate(filled);
    Ok(data)
}

/// The mode bits a created file may have: no setuid, setgid or sticky.
/// Its scratch copy is readgate's, so a setuid bit would make a program of
/// another user's content that runs as the user running readgate.
const PLAIN_MODE: u32 = 0o777;

/// Scratch folders are only readgate's: other users reach created files
/// through the mount, where the kernel checks them, never on disk.
fn private_dir_all(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
}

/// Replaces `source`'s content with `from`'s, in place.
fn copy_into(from: &Path, source: &Path) -> std::io::Result<()> {
    let mut from = File::open(from)?;
    let mut to = OpenOptions::new().write(true).truncate(true).open(source)?;
    std::io::copy(&mut from, &mut to)?;
    to.sync_all()
}

pub struct Mounted {
    _session: fuser::BackgroundSession,
}

/// Mounts `folder` with `mappings` in it until the returned value is
/// dropped. Created files go to `scratch`.
pub fn mount_folder(
    folder: &Path,
    mappings: Shared,
    gate: Gate,
    scratch: &Path,
    perms: Perms,
) -> anyhow::Result<Mounted> {
    use anyhow::Context;

    // readgate needs the folder to itself: mounting hides what's in it.
    if folder.exists() {
        let mut entries = fs::read_dir(folder).with_context(|| format!("reading {}", folder.display()))?;
        if entries.next().is_some() {
            anyhow::bail!("{} isn't empty; readgate needs a folder of its own to mount", folder.display());
        }
    } else {
        fs::create_dir_all(folder).with_context(|| format!("creating {}", folder.display()))?;
    }
    // Files programs created in earlier runs are still there, still theirs.
    private_dir_all(scratch).with_context(|| format!("creating {}", scratch.display()))?;
    // Folders made before they were private.
    fs::set_permissions(scratch, fs::Permissions::from_mode(0o700))?;

    let fs = GatedFolder {
        mappings,
        gate,
        scratch: scratch.to_owned(),
        inodes: Arc::new(Mutex::new(Inodes::new())),
        aliases: Arc::default(),
        handles: Mutex::default(),
        next_handle: AtomicU64::new(1),
        perms,
    };
    let mut config = Config::default();
    config.mount_options = vec![
        // Not the sources' paths: every user can read /proc/mounts.
        MountOption::FSName("readgate".into()),
        MountOption::Subtype("readgate".into()),
        // Read-only mappings are refused file by file: a mount can hold
        // both kinds, and gain either while it's mounted.
        MountOption::RW,
        MountOption::NoExec,
        MountOption::NoSuid,
        MountOption::NoDev,
        // The kernel checks owner and mode before anything gets here.
        MountOption::DefaultPermissions,
        // Unmounts if readgate dies, so the folder never stays a dead
        // mount.
        MountOption::AutoUnmount,
    ];
    // Other users open it: `allow_other`, which needs `user_allow_other`
    // in /etc/fuse.conf.
    config.acl = SessionACL::All;
    // fusermount3's own error says what to change (user_allow_other).
    let session = fuser::spawn_mount(fs, folder, &config).context("mounting")?;
    Ok(Mounted { _session: session })
}
