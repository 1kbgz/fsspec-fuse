//! Mount any fsspec-rs filesystem through FUSE, read-only or writable.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fsspec_rs::{FileInfo, FsError, FsFile, OpenMode, SharedFileSystem};
pub use fuser::BackgroundSession;
use fuser::{
    BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, LockOwner, MountOption, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyWrite, Request, TimeOrNow, WriteFlags,
};

/// How long the kernel may cache attributes and lookups before asking again.
const TTL: Duration = Duration::from_secs(1);

/// A file open for writing. The whole file is held in memory and written back to the
/// filesystem when the file is flushed, synced, or closed.
struct WriteBuffer {
    path: String,
    data: Vec<u8>,
    dirty: bool,
}

/// An open file. Reads lock the file so seeks and reads stay paired.
#[derive(Clone)]
enum OpenFile {
    Read(Arc<Mutex<Box<dyn FsFile>>>),
    Write(Arc<Mutex<WriteBuffer>>),
}

fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The errno FUSE reports for a filesystem error.
pub fn errno(error: &FsError) -> Errno {
    match error {
        FsError::NotFound(_) => Errno::ENOENT,
        FsError::PermissionDenied(_) => Errno::EACCES,
        FsError::AlreadyExists(_) => Errno::EEXIST,
        FsError::NotADirectory(_) => Errno::ENOTDIR,
        FsError::IsADirectory(_) => Errno::EISDIR,
        FsError::InvalidArgument(_) => Errno::EINVAL,
        FsError::NotSupported(_) => Errno::ENOSYS,
        FsError::IoError(error) => error.raw_os_error().map_or(Errno::EIO, Errno::from_i32),
        FsError::Other(_) => Errno::EIO,
    }
}

/// `name` below `parent`, joined with `/`.
fn child(parent: &str, name: &str) -> String {
    if parent.ends_with('/') {
        format!("{parent}{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// The last path segment of a listing entry.
fn basename(name: &str) -> &str {
    let name = name.trim_end_matches('/');
    name.rsplit('/').next().unwrap_or(name)
}

/// Where `path` ends up when `from` is renamed to `to`, if it is `from` or below it.
fn renamed(path: &str, from: &str, to: &str) -> Option<String> {
    if path == from {
        return Some(to.to_string());
    }
    let rest = path.strip_prefix(from)?.strip_prefix('/')?;
    Some(child(to, rest))
}

/// Paths by inode number. Inodes are never reused, so a mount that sees many paths grows
/// this table for its lifetime.
struct Inodes {
    paths: HashMap<u64, String>,
    ids: HashMap<String, u64>,
    next: u64,
}

impl Inodes {
    fn new(root: String) -> Self {
        let root_ino = u64::from(INodeNo::ROOT);
        Self {
            ids: HashMap::from([(root.clone(), root_ino)]),
            paths: HashMap::from([(root_ino, root)]),
            next: root_ino + 1,
        }
    }

    fn path(&self, ino: INodeNo) -> Result<String, Errno> {
        self.paths
            .get(&u64::from(ino))
            .cloned()
            .ok_or(Errno::ENOENT)
    }

    fn ino(&mut self, path: &str) -> INodeNo {
        if let Some(ino) = self.ids.get(path) {
            return INodeNo(*ino);
        }
        let ino = self.next;
        self.next += 1;
        self.ids.insert(path.to_string(), ino);
        self.paths.insert(ino, path.to_string());
        INodeNo(ino)
    }

    /// Moves `from` and everything below it to `to`, keeping their inode numbers. Inodes
    /// of entries the rename replaced stop resolving.
    fn rename(&mut self, from: &str, to: &str) {
        let replaced: Vec<String> = self
            .ids
            .keys()
            .filter(|path| renamed(path, to, to).is_some())
            .cloned()
            .collect();
        for path in replaced {
            if let Some(ino) = self.ids.remove(&path) {
                self.paths.remove(&ino);
            }
        }
        let moved: Vec<(String, String)> = self
            .ids
            .keys()
            .filter_map(|path| Some((path.clone(), renamed(path, from, to)?)))
            .collect();
        for (old, new) in moved {
            if let Some(ino) = self.ids.remove(&old) {
                self.paths.insert(ino, new.clone());
                self.ids.insert(new, ino);
            }
        }
    }
}

/// One directory entry for `readdir`.
#[derive(Clone, Debug, PartialEq)]
pub struct DirEntry {
    pub ino: INodeNo,
    pub kind: FileType,
    pub name: String,
}

/// A FUSE filesystem over an fsspec-rs filesystem. Read-only unless built with
/// [`FsspecFuse::read_only`] set to false.
pub struct FsspecFuse {
    fs: SharedFileSystem,
    uid: u32,
    gid: u32,
    read_only: bool,
    inodes: Mutex<Inodes>,
    files: Mutex<HashMap<u64, OpenFile>>,
    dirs: Mutex<HashMap<u64, Vec<DirEntry>>>,
    /// Directories created through the mount that the filesystem does not report, as on
    /// object stores where a directory exists only while something is stored below it.
    local_dirs: Mutex<HashSet<String>>,
    next_handle: AtomicU64,
}

impl FsspecFuse {
    /// Exposes `root` of `fs` as the mount's root directory, owned by the current user.
    pub fn new(fs: SharedFileSystem, root: impl Into<String>) -> Self {
        // SAFETY: getuid and getgid cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Self {
            fs,
            uid,
            gid,
            read_only: true,
            inodes: Mutex::new(Inodes::new(root.into())),
            files: Mutex::new(HashMap::new()),
            dirs: Mutex::new(HashMap::new()),
            local_dirs: Mutex::new(HashSet::new()),
            next_handle: AtomicU64::new(1),
        }
    }

    /// Whether the mount rejects changes with `EROFS`. True by default.
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    fn path(&self, ino: INodeNo) -> Result<String, Errno> {
        lock(&self.inodes).path(ino)
    }

    fn handle(&self, handle: u64) -> Result<OpenFile, Errno> {
        lock(&self.files).get(&handle).cloned().ok_or(Errno::EBADF)
    }

    fn add_handle(&self, file: OpenFile) -> u64 {
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        lock(&self.files).insert(handle, file);
        handle
    }

    /// Write buffers open on `path`.
    fn buffers(&self, path: &str) -> Vec<Arc<Mutex<WriteBuffer>>> {
        let files: Vec<OpenFile> = lock(&self.files).values().cloned().collect();
        files
            .into_iter()
            .filter_map(|file| match file {
                OpenFile::Write(buffer) if lock(&buffer).path == path => Some(buffer),
                _ => None,
            })
            .collect()
    }

    /// The filesystem's view of `path`, with the size of an open write buffer and with
    /// directories created through the mount.
    fn info(&self, path: &str) -> Result<FileInfo, Errno> {
        match self.fs.info(path) {
            Ok(mut info) => {
                if let Some(buffer) = self.buffers(path).first() {
                    info.size = lock(buffer).data.len() as u64;
                }
                Ok(info)
            }
            Err(FsError::NotFound(_)) if lock(&self.local_dirs).contains(path) => {
                Ok(FileInfo::directory(path))
            }
            Err(error) => Err(errno(&error)),
        }
    }

    fn attr(&self, ino: INodeNo, info: &FileInfo) -> FileAttr {
        let mtime = info.modified.unwrap_or(UNIX_EPOCH);
        let write = if self.read_only { 0 } else { 0o200 };
        let (kind, perm, nlink, size) = if info.is_dir() {
            (FileType::Directory, 0o555 | write, 2, 0)
        } else {
            (FileType::RegularFile, 0o444 | write, 1, info.size)
        };
        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: info.created.unwrap_or(mtime),
            kind,
            perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: 4096,
        }
    }

    fn entry(&self, path: &str) -> Result<FileAttr, Errno> {
        let info = self.info(path)?;
        let ino = lock(&self.inodes).ino(path);
        Ok(self.attr(ino, &info))
    }

    /// Attributes of the entry `name` in directory `parent`.
    pub fn lookup_entry(&self, parent: INodeNo, name: &str) -> Result<FileAttr, Errno> {
        self.entry(&child(&self.path(parent)?, name))
    }

    /// Attributes of the inode `ino`.
    pub fn attributes(&self, ino: INodeNo) -> Result<FileAttr, Errno> {
        let info = self.info(&self.path(ino)?)?;
        Ok(self.attr(ino, &info))
    }

    /// The entries of the directory at `path`, without `.` and `..`.
    fn children(&self, path: &str) -> Result<Vec<(String, FileType)>, Errno> {
        let listing = match self.fs.ls(path, true) {
            Ok(listing) => listing,
            Err(FsError::NotFound(_)) if lock(&self.local_dirs).contains(path) => Vec::new(),
            Err(error) => return Err(errno(&error)),
        };
        let mut children: Vec<(String, FileType)> = listing
            .iter()
            .map(|info| {
                let kind = if info.is_dir() {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                };
                (basename(&info.name).to_string(), kind)
            })
            .filter(|(name, _)| !name.is_empty())
            .collect();
        let mut local: Vec<String> = lock(&self.local_dirs)
            .iter()
            .filter(|dir| child(path, basename(dir)) == **dir)
            .map(|dir| basename(dir).to_string())
            .filter(|name| children.iter().all(|(listed, _)| listed != name))
            .collect();
        local.sort();
        children.extend(local.into_iter().map(|name| (name, FileType::Directory)));
        Ok(children)
    }

    /// The entries of directory `ino`, starting with `.` and `..`.
    pub fn list(&self, ino: INodeNo) -> Result<Vec<DirEntry>, Errno> {
        let path = self.path(ino)?;
        let children = self.children(&path)?;
        let mut entries = vec![
            DirEntry {
                ino,
                kind: FileType::Directory,
                name: ".".into(),
            },
            DirEntry {
                ino,
                kind: FileType::Directory,
                name: "..".into(),
            },
        ];
        let mut inodes = lock(&self.inodes);
        for (name, kind) in children {
            entries.push(DirEntry {
                ino: inodes.ino(&child(&path, &name)),
                kind,
                name,
            });
        }
        Ok(entries)
    }

    /// Opens `ino` for reading and returns its handle.
    pub fn open_file(&self, ino: INodeNo) -> Result<u64, Errno> {
        let file = self
            .fs
            .open(&self.path(ino)?, OpenMode::Read, None)
            .map_err(|e| errno(&e))?;
        Ok(self.add_handle(OpenFile::Read(Arc::new(Mutex::new(file)))))
    }

    /// Opens `ino` for writing and returns its handle. The current contents are read
    /// into memory unless `truncate` is set.
    pub fn open_for_write(&self, ino: INodeNo, truncate: bool) -> Result<u64, Errno> {
        let path = self.path(ino)?;
        let data = if truncate {
            Vec::new()
        } else {
            self.fs.cat_file(&path, None, None).map_err(|e| errno(&e))?
        };
        Ok(
            self.add_handle(OpenFile::Write(Arc::new(Mutex::new(WriteBuffer {
                path,
                data,
                dirty: truncate,
            })))),
        )
    }

    /// Creates the empty file `name` in directory `parent` and opens it for writing.
    pub fn create_file(&self, parent: INodeNo, name: &str) -> Result<(FileAttr, u64), Errno> {
        let path = child(&self.path(parent)?, name);
        self.fs.pipe_file(&path, b"").map_err(|e| errno(&e))?;
        let ino = lock(&self.inodes).ino(&path);
        let attr = self.attr(ino, &FileInfo::file(path.clone(), 0));
        let handle = self.add_handle(OpenFile::Write(Arc::new(Mutex::new(WriteBuffer {
            path,
            data: Vec::new(),
            dirty: false,
        }))));
        Ok((attr, handle))
    }

    /// Up to `size` bytes of the open file `handle`, starting at `offset`.
    pub fn read_at(&self, handle: u64, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        match self.handle(handle)? {
            OpenFile::Read(file) => {
                let mut file = lock(&file);
                file.seek(SeekFrom::Start(offset)).map_err(Errno::from)?;
                let mut data = Vec::with_capacity(size as usize);
                file.by_ref()
                    .take(u64::from(size))
                    .read_to_end(&mut data)
                    .map_err(Errno::from)?;
                Ok(data)
            }
            OpenFile::Write(buffer) => {
                let buffer = lock(&buffer);
                let start = (offset as usize).min(buffer.data.len());
                let end = start.saturating_add(size as usize).min(buffer.data.len());
                Ok(buffer.data[start..end].to_vec())
            }
        }
    }

    /// Writes `data` at `offset` into the file open for writing as `handle`, filling any
    /// gap past the end with zeros.
    pub fn write_at(&self, handle: u64, offset: u64, data: &[u8]) -> Result<u32, Errno> {
        let OpenFile::Write(buffer) = self.handle(handle)? else {
            return Err(Errno::EBADF);
        };
        let mut buffer = lock(&buffer);
        let start = offset as usize;
        let end = start + data.len();
        if buffer.data.len() < end {
            buffer.data.resize(end, 0);
        }
        buffer.data[start..end].copy_from_slice(data);
        buffer.dirty = true;
        Ok(data.len() as u32)
    }

    fn commit(&self, file: &OpenFile) -> Result<(), Errno> {
        let OpenFile::Write(buffer) = file else {
            return Ok(());
        };
        let mut buffer = lock(buffer);
        if buffer.dirty {
            self.fs
                .pipe_file(&buffer.path, &buffer.data)
                .map_err(|e| errno(&e))?;
            buffer.dirty = false;
        }
        Ok(())
    }

    /// Writes the buffered contents of `handle` to the filesystem if they changed.
    pub fn sync_file(&self, handle: u64) -> Result<(), Errno> {
        self.commit(&self.handle(handle)?)
    }

    /// Closes the open file `handle`, writing back any changes first.
    pub fn close_file(&self, handle: u64) -> Result<(), Errno> {
        match lock(&self.files).remove(&handle) {
            Some(file) => self.commit(&file),
            None => Ok(()),
        }
    }

    /// Sets the size of `ino` to `size`, through the open write handle `handle` when
    /// there is one.
    pub fn truncate(&self, ino: INodeNo, handle: Option<u64>, size: u64) -> Result<(), Errno> {
        if let Some(OpenFile::Write(buffer)) = handle.map(|h| self.handle(h)).transpose()? {
            let mut buffer = lock(&buffer);
            buffer.data.resize(size as usize, 0);
            buffer.dirty = true;
            return Ok(());
        }
        let path = self.path(ino)?;
        let mut data = if size == 0 {
            Vec::new()
        } else {
            self.fs.cat_file(&path, None, None).map_err(|e| errno(&e))?
        };
        data.resize(size as usize, 0);
        self.fs.pipe_file(&path, &data).map_err(|e| errno(&e))
    }

    /// Creates the directory `name` in directory `parent`.
    pub fn make_dir(&self, parent: INodeNo, name: &str) -> Result<FileAttr, Errno> {
        let path = child(&self.path(parent)?, name);
        if self.fs.exists(&path).map_err(|e| errno(&e))? {
            return Err(Errno::EEXIST);
        }
        self.fs.mkdir(&path, false).map_err(|e| errno(&e))?;
        if !self.fs.exists(&path).map_err(|e| errno(&e))? {
            lock(&self.local_dirs).insert(path.clone());
        }
        self.entry(&path)
    }

    /// Removes the empty directory `name` from directory `parent`.
    pub fn remove_dir(&self, parent: INodeNo, name: &str) -> Result<(), Errno> {
        let path = child(&self.path(parent)?, name);
        if !self.children(&path)?.is_empty() {
            return Err(Errno::ENOTEMPTY);
        }
        let local = lock(&self.local_dirs).remove(&path);
        match self.fs.rmdir(&path) {
            Err(FsError::NotFound(_)) if local => {}
            result => result.map_err(|e| errno(&e))?,
        }
        self.keep_parents(&path);
        Ok(())
    }

    /// Removes the file `name` from directory `parent`.
    pub fn remove_file(&self, parent: INodeNo, name: &str) -> Result<(), Errno> {
        let path = child(&self.path(parent)?, name);
        self.fs.rm_file(&path).map_err(|e| errno(&e))?;
        self.keep_parents(&path);
        Ok(())
    }

    /// Keeps the directories above `path` that the filesystem dropped along with their
    /// last entry, as object stores do, until they are removed through the mount.
    fn keep_parents(&self, path: &str) {
        let Ok(root) = self.path(INodeNo::ROOT) else {
            return;
        };
        let root = root.trim_end_matches('/');
        let mut dir = path.trim_end_matches('/');
        while let Some((parent, _)) = dir.rsplit_once('/') {
            if parent.len() <= root.len() || self.fs.exists(parent).unwrap_or(true) {
                break;
            }
            lock(&self.local_dirs).insert(parent.to_string());
            dir = parent;
        }
    }

    /// Moves `name` in `parent` to `new_name` in `new_parent`, replacing a file there
    /// unless `no_replace` is set. Object stores copy and delete, so the move is not
    /// atomic there.
    pub fn rename_entry(
        &self,
        parent: INodeNo,
        name: &str,
        new_parent: INodeNo,
        new_name: &str,
        no_replace: bool,
    ) -> Result<(), Errno> {
        let from = child(&self.path(parent)?, name);
        let to = child(&self.path(new_parent)?, new_name);
        if from == to {
            return Ok(());
        }
        let source = self.info(&from)?;
        match self.info(&to) {
            Ok(_) if no_replace => return Err(Errno::EEXIST),
            Ok(target) if target.is_dir() && !source.is_dir() => return Err(Errno::EISDIR),
            Ok(target) if !target.is_dir() && source.is_dir() => return Err(Errno::ENOTDIR),
            Ok(target) if target.is_dir() && !self.children(&to)?.is_empty() => {
                return Err(Errno::ENOTEMPTY)
            }
            Ok(_) | Err(Errno::ENOENT) => {}
            Err(error) => return Err(error),
        }
        if source.is_dir() {
            self.move_dir(&from, &to)?;
        } else {
            self.fs.mv(&from, &to).map_err(|e| errno(&e))?;
        }
        self.keep_parents(&from);

        lock(&self.inodes).rename(&from, &to);
        let files: Vec<OpenFile> = lock(&self.files).values().cloned().collect();
        for file in files {
            if let OpenFile::Write(buffer) = file {
                let mut buffer = lock(&buffer);
                if let Some(path) = renamed(&buffer.path, &from, &to) {
                    buffer.path = path;
                }
            }
        }
        let mut local_dirs = lock(&self.local_dirs);
        let moved: Vec<String> = local_dirs
            .iter()
            .filter(|dir| renamed(dir, &from, &to).is_some())
            .cloned()
            .collect();
        for dir in moved {
            local_dirs.remove(&dir);
            let dir = renamed(&dir, &from, &to).unwrap_or(dir);
            if !self.fs.exists(&dir).unwrap_or(false) {
                local_dirs.insert(dir);
            }
        }
        Ok(())
    }

    /// Moves the directory `from` to `to` one file at a time, creating each directory,
    /// empty ones included, before removing the source. Paths are built from listing
    /// names, so backends that list in a different path form than the mount uses still
    /// move correctly.
    fn move_dir(&self, from: &str, to: &str) -> Result<(), Errno> {
        if !self.fs.exists(to).map_err(|e| errno(&e))? {
            self.fs.mkdir(to, false).map_err(|e| errno(&e))?;
        }
        for (name, kind) in self.children(from)? {
            let (src, dst) = (child(from, &name), child(to, &name));
            if kind == FileType::Directory {
                self.move_dir(&src, &dst)?;
            } else {
                self.fs.mv(&src, &dst).map_err(|e| errno(&e))?;
            }
        }
        match self.fs.rmdir(from) {
            Err(FsError::NotFound(_)) => Ok(()),
            result => result.map_err(|e| errno(&e)),
        }
    }
}

/// Whether rename `flags` forbid replacing the target. Exchanging two paths, and any other
/// flag, is not supported.
#[cfg(target_os = "linux")]
fn no_replace(flags: RenameFlags) -> Result<bool, Errno> {
    if (flags - RenameFlags::RENAME_NOREPLACE).is_empty() {
        Ok(flags.contains(RenameFlags::RENAME_NOREPLACE))
    } else {
        Err(Errno::EINVAL)
    }
}

/// Whether rename `flags` forbid replacing the target. fuser defines rename flags only on
/// Linux, so any flag is unsupported elsewhere.
#[cfg(not(target_os = "linux"))]
fn no_replace(flags: RenameFlags) -> Result<bool, Errno> {
    if flags.is_empty() {
        Ok(false)
    } else {
        Err(Errno::EINVAL)
    }
}

/// The UTF-8 name FUSE passed, or `ENOENT` for names no fsspec path can hold.
fn name(name: &OsStr) -> Result<&str, Errno> {
    name.to_str().ok_or(Errno::ENOENT)
}

impl Filesystem for FsspecFuse {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self::name(name).and_then(|name| self.lookup_entry(parent, name)) {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(error) => reply.error(error),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.attributes(ino) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(error) => reply.error(error),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        if self.read_only {
            return reply.error(Errno::EROFS);
        }
        // Modes, owners, and times are synthesized, so changes to them are accepted and
        // dropped; tools such as cp -p and touch would fail otherwise.
        let result = match size {
            Some(size) => self.truncate(ino, fh.map(u64::from), size),
            None => Ok(()),
        };
        match result.and_then(|()| self.attributes(ino)) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(error) => reply.error(error),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        if self.read_only {
            return reply.error(Errno::EROFS);
        }
        match self::name(name).and_then(|name| self.make_dir(parent, name)) {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(error) => reply.error(error),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.read_only {
            return reply.error(Errno::EROFS);
        }
        match self::name(name).and_then(|name| self.remove_file(parent, name)) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.read_only {
            return reply.error(Errno::EROFS);
        }
        match self::name(name).and_then(|name| self.remove_dir(parent, name)) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        if self.read_only {
            return reply.error(Errno::EROFS);
        }
        let result = no_replace(flags).and_then(|no_replace| {
            let (name, newname) = (self::name(name)?, self::name(newname)?);
            self.rename_entry(parent, name, newparent, newname, no_replace)
        });
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        // List once per open so offsets stay stable across readdir calls.
        match self.list(ino) {
            Ok(entries) => {
                let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
                lock(&self.dirs).insert(handle, entries);
                reply.opened(FileHandle(handle), FopenFlags::empty());
            }
            Err(error) => reply.error(error),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let dirs = lock(&self.dirs);
        let Some(entries) = dirs.get(&u64::from(fh)) else {
            return reply.error(Errno::EBADF);
        };
        for (index, entry) in entries.iter().enumerate().skip(offset as usize) {
            if reply.add(entry.ino, index as u64 + 1, entry.kind, &entry.name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        lock(&self.dirs).remove(&u64::from(fh));
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let result = if flags.acc_mode() == OpenAccMode::O_RDONLY {
            self.open_file(ino)
        } else if self.read_only {
            Err(Errno::EROFS)
        } else {
            self.open_for_write(ino, flags.0 & libc::O_TRUNC != 0)
        };
        match result {
            Ok(handle) => reply.opened(FileHandle(handle), FopenFlags::empty()),
            Err(error) => reply.error(error),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        if self.read_only {
            return reply.error(Errno::EROFS);
        }
        match self::name(name).and_then(|name| self.create_file(parent, name)) {
            Ok((attr, handle)) => reply.created(
                &TTL,
                &attr,
                Generation(0),
                FileHandle(handle),
                FopenFlags::empty(),
            ),
            Err(error) => reply.error(error),
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
        match self.read_at(u64::from(fh), offset, size) {
            Ok(data) => reply.data(&data),
            Err(error) => reply.error(error),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.write_at(u64::from(fh), offset, data) {
            Ok(written) => reply.written(written),
            Err(error) => reply.error(error),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        // close() flushes, so a failed write-back reaches the caller as an error from close.
        match self.sync_file(u64::from(fh)) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        match self.sync_file(u64::from(fh)) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
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
        match self.close_file(u64::from(fh)) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        reply.statfs(0, 0, 0, 0, 0, 4096, 255, 4096);
    }
}

/// Mount configuration for an fsspec mount named `name` in the mount table.
pub fn config(name: &str, threads: usize, read_only: bool) -> Config {
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName(name.to_string()),
        MountOption::Subtype("fsspec".to_string()),
    ];
    if read_only {
        config.mount_options.push(MountOption::RO);
    }
    config.n_threads = Some(threads.max(1));
    config
}

/// Mounts `root` of `fs` at `mountpoint` on background threads, writable unless `config`
/// has [`MountOption::RO`]. Dropping the returned session unmounts it.
pub fn spawn_mount(
    fs: SharedFileSystem,
    root: impl Into<String>,
    mountpoint: &Path,
    config: &Config,
) -> io::Result<BackgroundSession> {
    let read_only = config.mount_options.contains(&MountOption::RO);
    fuser::spawn_mount(
        FsspecFuse::new(fs, root).read_only(read_only),
        mountpoint,
        config,
    )
}

#[cfg(test)]
mod tests;
