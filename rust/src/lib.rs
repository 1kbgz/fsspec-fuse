//! Mount any fsspec-rs filesystem read-only through FUSE.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, UNIX_EPOCH};

use fsspec_rs::{FileInfo, FsError, FsFile, OpenMode, SharedFileSystem};
pub use fuser::BackgroundSession;
use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, MountOption, OpenAccMode, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, Request,
};

/// How long the kernel may cache attributes and lookups before asking again.
const TTL: Duration = Duration::from_secs(1);

/// An open file, locked per read so seeks and reads stay paired.
type OpenFile = Arc<Mutex<Box<dyn FsFile>>>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
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

/// Paths by inode number. Inodes are never reused, so a mount that sees many paths grows
/// this table for its lifetime.
struct Inodes {
    paths: HashMap<u64, String>,
    ids: HashMap<String, u64>,
}

impl Inodes {
    fn new(root: String) -> Self {
        let root_ino = u64::from(INodeNo::ROOT);
        Self {
            ids: HashMap::from([(root.clone(), root_ino)]),
            paths: HashMap::from([(root_ino, root)]),
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
        let ino = self.paths.len() as u64 + 1;
        self.ids.insert(path.to_string(), ino);
        self.paths.insert(ino, path.to_string());
        INodeNo(ino)
    }
}

/// One directory entry for `readdir`.
#[derive(Clone, Debug, PartialEq)]
pub struct DirEntry {
    pub ino: INodeNo,
    pub kind: FileType,
    pub name: String,
}

/// A read-only FUSE filesystem over an fsspec-rs filesystem.
pub struct FsspecFuse {
    fs: SharedFileSystem,
    uid: u32,
    gid: u32,
    inodes: Mutex<Inodes>,
    files: Mutex<HashMap<u64, OpenFile>>,
    dirs: Mutex<HashMap<u64, Vec<DirEntry>>>,
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
            inodes: Mutex::new(Inodes::new(root.into())),
            files: Mutex::new(HashMap::new()),
            dirs: Mutex::new(HashMap::new()),
            next_handle: AtomicU64::new(1),
        }
    }

    fn path(&self, ino: INodeNo) -> Result<String, Errno> {
        lock(&self.inodes).path(ino)
    }

    fn attr(&self, ino: INodeNo, info: &FileInfo) -> FileAttr {
        let mtime = info.modified.unwrap_or(UNIX_EPOCH);
        let (kind, perm, nlink, size) = if info.is_dir() {
            (FileType::Directory, 0o555, 2, 0)
        } else {
            (FileType::RegularFile, 0o444, 1, info.size)
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

    /// Attributes of the entry `name` in directory `parent`.
    pub fn lookup_entry(&self, parent: INodeNo, name: &str) -> Result<FileAttr, Errno> {
        let path = child(&self.path(parent)?, name);
        let info = self.fs.info(&path).map_err(|e| errno(&e))?;
        let ino = lock(&self.inodes).ino(&path);
        Ok(self.attr(ino, &info))
    }

    /// Attributes of the inode `ino`.
    pub fn attributes(&self, ino: INodeNo) -> Result<FileAttr, Errno> {
        let info = self.fs.info(&self.path(ino)?).map_err(|e| errno(&e))?;
        Ok(self.attr(ino, &info))
    }

    /// The entries of directory `ino`, starting with `.` and `..`.
    pub fn list(&self, ino: INodeNo) -> Result<Vec<DirEntry>, Errno> {
        let path = self.path(ino)?;
        let listing = self.fs.ls(&path, true).map_err(|e| errno(&e))?;
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
        for info in listing {
            let name = basename(&info.name).to_string();
            if name.is_empty() {
                continue;
            }
            let kind = if info.is_dir() {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
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
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        lock(&self.files).insert(handle, Arc::new(Mutex::new(file)));
        Ok(handle)
    }

    /// Up to `size` bytes of the open file `handle`, starting at `offset`.
    pub fn read_at(&self, handle: u64, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        let file = lock(&self.files)
            .get(&handle)
            .cloned()
            .ok_or(Errno::EBADF)?;
        let mut file = lock(&file);
        file.seek(SeekFrom::Start(offset)).map_err(Errno::from)?;
        let mut data = Vec::with_capacity(size as usize);
        file.by_ref()
            .take(u64::from(size))
            .read_to_end(&mut data)
            .map_err(Errno::from)?;
        Ok(data)
    }

    /// Closes the open file `handle`.
    pub fn close_file(&self, handle: u64) {
        lock(&self.files).remove(&handle);
    }
}

impl Filesystem for FsspecFuse {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            return reply.error(Errno::ENOENT);
        };
        match self.lookup_entry(parent, name) {
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
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            return reply.error(Errno::EROFS);
        }
        match self.open_file(ino) {
            Ok(handle) => reply.opened(FileHandle(handle), FopenFlags::empty()),
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
        self.close_file(u64::from(fh));
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        reply.statfs(0, 0, 0, 0, 0, 4096, 255, 4096);
    }
}

/// Mount configuration for a read-only fsspec mount named `name` in the mount table.
pub fn config(name: &str, threads: usize) -> Config {
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::RO,
        MountOption::FSName(name.to_string()),
        MountOption::Subtype("fsspec".to_string()),
    ];
    config.n_threads = Some(threads.max(1));
    config
}

/// Mounts `root` of `fs` at `mountpoint` on background threads. Dropping the returned
/// session unmounts it.
pub fn spawn_mount(
    fs: SharedFileSystem,
    root: impl Into<String>,
    mountpoint: &Path,
    config: &Config,
) -> io::Result<BackgroundSession> {
    fuser::spawn_mount(FsspecFuse::new(fs, root), mountpoint, config)
}

#[cfg(test)]
mod tests;
