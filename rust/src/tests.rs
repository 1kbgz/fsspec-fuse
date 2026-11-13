use std::sync::Arc;

use fsspec_rs::{FileSystem, FsError, LocalFs, SharedFileSystem};
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
    fuse.close_file(handle);
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
