use std::sync::Arc;

use fsspec_rs::{
    FileInfo, FileSystem, FsError, FsFile, FsResult, LocalFs, MemoryFs, OpenMode, OpenOptions,
    SharedFileSystem,
};
use fuser::{Errno, FileType, INodeNo};

use crate::{errno, FsspecFuse};

fn tree() -> (tempfile::TempDir, FsspecFuse) {
    let dir = tempfile::tempdir().unwrap();
    let local = LocalFs::new();
    let root = dir.path().to_str().unwrap().to_string();
    local.mkdir(&format!("{root}/sub"), false).unwrap();
    local
        .pipe_file(&format!("{root}/a.txt"), b"hello world")
        .unwrap();
    local
        .pipe_file(&format!("{root}/sub/b.txt"), b"nested")
        .unwrap();
    let fs: SharedFileSystem = Arc::new(local);
    (dir, FsspecFuse::new(fs, root))
}

fn writable_tree() -> (tempfile::TempDir, FsspecFuse) {
    let (dir, fuse) = tree();
    (dir, fuse.read_only(false))
}

fn read(dir: &tempfile::TempDir, path: &str) -> Vec<u8> {
    std::fs::read(dir.path().join(path)).unwrap()
}

/// An object store: directories exist only while something is stored below them, so
/// creating or removing one does nothing. Like S3, it lists keys without the leading
/// `/` the mount's paths have.
struct ObjectStore(MemoryFs);

impl FileSystem for ObjectStore {
    fn protocol(&self) -> &[&str] {
        &["objects"]
    }
    fn ls(&self, path: &str, detail: bool) -> FsResult<Vec<FileInfo>> {
        let mut listing = self.0.ls(path, detail)?;
        for info in &mut listing {
            info.name = info.name.trim_start_matches('/').to_string();
        }
        Ok(listing)
    }
    fn rm_file(&self, path: &str) -> FsResult<()> {
        self.0.rm_file(path)
    }
    fn cp_file(&self, src: &str, dst: &str) -> FsResult<()> {
        self.0.cp_file(src, dst)
    }
    fn open(
        &self,
        path: &str,
        mode: OpenMode,
        opts: Option<OpenOptions>,
    ) -> FsResult<Box<dyn FsFile>> {
        self.0.open(path, mode, opts)
    }
    fn info(&self, path: &str) -> FsResult<FileInfo> {
        self.0.info(path)
    }
    fn mkdir(&self, _path: &str, _create_parents: bool) -> FsResult<()> {
        Ok(())
    }
    fn rmdir(&self, _path: &str) -> FsResult<()> {
        Ok(())
    }
}

#[test]
fn lists_the_root_with_dot_entries() {
    let (_dir, fuse) = tree();
    let entries = fuse.list(INodeNo::ROOT).unwrap();
    let names: Vec<_> = entries.iter().map(|e| (e.name.as_str(), e.kind)).collect();
    assert_eq!(
        names,
        [
            (".", FileType::Directory),
            ("..", FileType::Directory),
            ("a.txt", FileType::RegularFile),
            ("sub", FileType::Directory),
        ]
    );
}

#[test]
fn lookup_assigns_stable_inodes_and_read_only_attributes() {
    let (_dir, fuse) = tree();
    let file = fuse.lookup_entry(INodeNo::ROOT, "a.txt").unwrap();
    assert_eq!(
        (file.kind, file.size, file.perm),
        (FileType::RegularFile, 11, 0o444)
    );
    assert_eq!(
        fuse.lookup_entry(INodeNo::ROOT, "a.txt").unwrap().ino,
        file.ino
    );
    assert_eq!(fuse.attributes(file.ino).unwrap().size, 11);

    let sub = fuse.lookup_entry(INodeNo::ROOT, "sub").unwrap();
    assert_eq!((sub.kind, sub.perm), (FileType::Directory, 0o555));
    let listed = fuse.list(INodeNo::ROOT).unwrap();
    assert!(listed.iter().any(|e| e.name == "sub" && e.ino == sub.ino));

    let nested = fuse.lookup_entry(sub.ino, "b.txt").unwrap();
    assert_eq!(nested.size, 6);
    assert_eq!(
        fuse.lookup_entry(INodeNo::ROOT, "missing"),
        Err(Errno::ENOENT)
    );
    assert_eq!(fuse.attributes(INodeNo(999)), Err(Errno::ENOENT));
}

#[test]
fn reads_ranges_from_open_handles() {
    let (_dir, fuse) = tree();
    let ino = fuse.lookup_entry(INodeNo::ROOT, "a.txt").unwrap().ino;
    let handle = fuse.open_file(ino).unwrap();
    assert_eq!(fuse.read_at(handle, 6, 100).unwrap(), b"world");
    assert_eq!(fuse.read_at(handle, 0, 5).unwrap(), b"hello");
    assert_eq!(fuse.read_at(handle, 50, 5).unwrap(), b"");
    fuse.close_file(handle).unwrap();
    assert_eq!(fuse.read_at(handle, 0, 5), Err(Errno::EBADF));
}

#[test]
fn listing_a_file_is_not_a_directory() {
    let (_dir, fuse) = tree();
    let ino = fuse.lookup_entry(INodeNo::ROOT, "a.txt").unwrap().ino;
    assert_eq!(fuse.list(ino), Err(Errno::ENOTDIR));
}

#[test]
fn filesystem_errors_map_to_errno() {
    assert_eq!(errno(&FsError::NotFound("x".into())), Errno::ENOENT);
    assert_eq!(errno(&FsError::IsADirectory("x".into())), Errno::EISDIR);
    assert_eq!(errno(&FsError::NotSupported("x".into())), Errno::ENOSYS);
    let io = std::io::Error::from_raw_os_error(libc::EACCES);
    assert_eq!(errno(&FsError::IoError(io)), Errno::EACCES);
    assert_eq!(errno(&FsError::Other("x".into())), Errno::EIO);
}

#[test]
fn writable_mounts_report_write_permissions() {
    let (_dir, fuse) = writable_tree();
    let file = fuse.lookup_entry(INodeNo::ROOT, "a.txt").unwrap();
    let sub = fuse.lookup_entry(INodeNo::ROOT, "sub").unwrap();
    assert_eq!((file.perm, sub.perm), (0o644, 0o755));
}

#[test]
fn writes_are_buffered_until_flushed() {
    let (dir, fuse) = writable_tree();
    let (attr, handle) = fuse.create_file(INodeNo::ROOT, "new.txt").unwrap();
    assert_eq!((attr.kind, attr.size), (FileType::RegularFile, 0));
    assert_eq!(read(&dir, "new.txt"), b"");

    assert_eq!(fuse.write_at(handle, 0, b"hello").unwrap(), 5);
    assert_eq!(fuse.write_at(handle, 8, b"!").unwrap(), 1);
    assert_eq!(fuse.read_at(handle, 0, 100).unwrap(), b"hello\0\0\0!");
    assert_eq!(fuse.attributes(attr.ino).unwrap().size, 9);
    assert_eq!(read(&dir, "new.txt"), b"");

    fuse.sync_file(handle).unwrap();
    assert_eq!(read(&dir, "new.txt"), b"hello\0\0\0!");
    fuse.write_at(handle, 5, b"   ").unwrap();
    fuse.close_file(handle).unwrap();
    assert_eq!(read(&dir, "new.txt"), b"hello   !");
    assert_eq!(fuse.write_at(handle, 0, b"x"), Err(Errno::EBADF));
}

#[test]
fn opening_for_write_keeps_or_truncates_the_contents() {
    let (dir, fuse) = writable_tree();
    let ino = fuse.lookup_entry(INodeNo::ROOT, "a.txt").unwrap().ino;

    let handle = fuse.open_for_write(ino, false).unwrap();
    fuse.write_at(handle, 0, b"J").unwrap();
    fuse.close_file(handle).unwrap();
    assert_eq!(read(&dir, "a.txt"), b"Jello world");

    let handle = fuse.open_for_write(ino, true).unwrap();
    fuse.close_file(handle).unwrap();
    assert_eq!(read(&dir, "a.txt"), b"");

    let reader = fuse.open_file(ino).unwrap();
    assert_eq!(fuse.write_at(reader, 0, b"x"), Err(Errno::EBADF));
}

#[test]
fn truncates_open_and_closed_files() {
    let (dir, fuse) = writable_tree();
    let ino = fuse.lookup_entry(INodeNo::ROOT, "a.txt").unwrap().ino;
    fuse.truncate(ino, None, 5).unwrap();
    assert_eq!(read(&dir, "a.txt"), b"hello");
    fuse.truncate(ino, None, 7).unwrap();
    assert_eq!(read(&dir, "a.txt"), b"hello\0\0");

    let handle = fuse.open_for_write(ino, false).unwrap();
    fuse.truncate(ino, Some(handle), 2).unwrap();
    assert_eq!(fuse.attributes(ino).unwrap().size, 2);
    fuse.close_file(handle).unwrap();
    assert_eq!(read(&dir, "a.txt"), b"he");
}

#[test]
fn creates_and_removes_directories_and_files() {
    let (dir, fuse) = writable_tree();
    let made = fuse.make_dir(INodeNo::ROOT, "made").unwrap();
    assert_eq!(made.kind, FileType::Directory);
    assert!(dir.path().join("made").is_dir());
    assert_eq!(fuse.make_dir(INodeNo::ROOT, "made"), Err(Errno::EEXIST));

    assert_eq!(fuse.remove_dir(INodeNo::ROOT, "sub"), Err(Errno::ENOTEMPTY));
    let sub = fuse.lookup_entry(INodeNo::ROOT, "sub").unwrap().ino;
    fuse.remove_file(sub, "b.txt").unwrap();
    assert!(!dir.path().join("sub/b.txt").exists());
    fuse.remove_dir(INodeNo::ROOT, "sub").unwrap();
    fuse.remove_dir(INodeNo::ROOT, "made").unwrap();
    assert!(!dir.path().join("sub").exists());
    assert_eq!(
        fuse.remove_file(INodeNo::ROOT, "missing"),
        Err(Errno::ENOENT)
    );
}

#[test]
fn renames_files_and_directories_keeping_inodes() {
    let (dir, fuse) = writable_tree();
    let file = fuse.lookup_entry(INodeNo::ROOT, "a.txt").unwrap().ino;
    let sub = fuse.lookup_entry(INodeNo::ROOT, "sub").unwrap().ino;
    let nested = fuse.lookup_entry(sub, "b.txt").unwrap().ino;
    let empty = fuse.make_dir(sub, "empty").unwrap().ino;

    fuse.rename_entry(INodeNo::ROOT, "a.txt", sub, "c.txt", false)
        .unwrap();
    assert_eq!(read(&dir, "sub/c.txt"), b"hello world");
    assert_eq!(fuse.lookup_entry(sub, "c.txt").unwrap().ino, file);

    fuse.rename_entry(INodeNo::ROOT, "sub", INodeNo::ROOT, "moved", false)
        .unwrap();
    assert!(!dir.path().join("sub").exists());
    assert_eq!(read(&dir, "moved/b.txt"), b"nested");
    assert!(dir.path().join("moved/empty").is_dir());
    let moved = fuse.lookup_entry(INodeNo::ROOT, "moved").unwrap().ino;
    assert_eq!(moved, sub);
    assert_eq!(fuse.lookup_entry(moved, "b.txt").unwrap().ino, nested);
    assert_eq!(fuse.lookup_entry(moved, "empty").unwrap().ino, empty);
}

#[test]
fn rename_checks_the_target() {
    let (dir, fuse) = writable_tree();
    let sub = fuse.lookup_entry(INodeNo::ROOT, "sub").unwrap().ino;
    let replaced = fuse.lookup_entry(sub, "b.txt").unwrap().ino;
    assert_eq!(
        fuse.rename_entry(INodeNo::ROOT, "a.txt", sub, "b.txt", true),
        Err(Errno::EEXIST)
    );
    assert_eq!(
        fuse.rename_entry(INodeNo::ROOT, "a.txt", INodeNo::ROOT, "sub", false),
        Err(Errno::EISDIR)
    );
    assert_eq!(
        fuse.rename_entry(INodeNo::ROOT, "sub", sub, "b.txt", false),
        Err(Errno::ENOTDIR)
    );
    fuse.rename_entry(INodeNo::ROOT, "a.txt", sub, "b.txt", false)
        .unwrap();
    assert_eq!(read(&dir, "sub/b.txt"), b"hello world");
    assert_eq!(fuse.attributes(replaced), Err(Errno::ENOENT));
}

#[test]
fn directories_made_on_an_object_store_stay_visible() {
    let fs: SharedFileSystem = Arc::new(ObjectStore(MemoryFs::new()));
    let fuse = FsspecFuse::new(fs.clone(), "/bucket").read_only(false);
    fs.pipe_file("/bucket/keep.txt", b"k").unwrap();

    let dir = fuse.make_dir(INodeNo::ROOT, "dir").unwrap().ino;
    assert!(!fs.exists("/bucket/dir").unwrap());
    let names: Vec<_> = fuse
        .list(INodeNo::ROOT)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, [".", "..", "keep.txt", "dir"]);
    assert_eq!(fuse.list(dir).unwrap().len(), 2);

    let inner = fuse.make_dir(dir, "inner").unwrap().ino;
    let (_, handle) = fuse.create_file(inner, "f.txt").unwrap();
    fuse.write_at(handle, 0, b"data").unwrap();
    fuse.close_file(handle).unwrap();
    assert_eq!(
        fs.cat_file("/bucket/dir/inner/f.txt", None, None).unwrap(),
        b"data"
    );

    fuse.rename_entry(INodeNo::ROOT, "dir", INodeNo::ROOT, "renamed", false)
        .unwrap();
    assert_eq!(
        fs.cat_file("/bucket/renamed/inner/f.txt", None, None)
            .unwrap(),
        b"data"
    );
    let renamed = fuse.lookup_entry(INodeNo::ROOT, "renamed").unwrap().ino;
    fuse.make_dir(renamed, "empty").unwrap();
    fuse.rename_entry(renamed, "empty", INodeNo::ROOT, "empty", false)
        .unwrap();
    assert_eq!(
        fuse.lookup_entry(INodeNo::ROOT, "empty").unwrap().kind,
        FileType::Directory
    );
    assert_eq!(fuse.lookup_entry(renamed, "empty"), Err(Errno::ENOENT));
    fuse.remove_dir(INodeNo::ROOT, "empty").unwrap();
    assert_eq!(
        fuse.lookup_entry(INodeNo::ROOT, "empty"),
        Err(Errno::ENOENT)
    );

    // Removing the last file drops its directories from the store but not from the mount.
    let inner = fuse.lookup_entry(renamed, "inner").unwrap().ino;
    fuse.remove_file(inner, "f.txt").unwrap();
    assert!(!fs.exists("/bucket/renamed").unwrap());
    assert_eq!(fuse.list(inner).unwrap().len(), 2);
    assert_eq!(
        fuse.remove_dir(INodeNo::ROOT, "renamed"),
        Err(Errno::ENOTEMPTY)
    );
    fuse.remove_dir(renamed, "inner").unwrap();
    fuse.remove_dir(INodeNo::ROOT, "renamed").unwrap();
    let names: Vec<_> = fuse
        .list(INodeNo::ROOT)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, [".", "..", "keep.txt"]);
}
