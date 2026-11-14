# Mount behavior reference

## Operations

| Operation           | Backend call                   | Result on a mount                                    |
| ------------------- | ------------------------------ | ---------------------------------------------------- |
| `stat`, path lookup | `info`                         | Attributes below.                                    |
| list a directory    | `ls(detail=True)`              | `.`, `..`, then the backend's entries.               |
| open for reading    | `open(mode="rb")`              | One backend file per open handle.                    |
| read                | seek and read on the open file | Bytes from the requested offset.                     |
| `statfs`            | none                           | Zero counts, block size 4096, names up to 255 bytes. |

A directory's listing is taken once when it is opened, so entries do not shift between
`readdir` calls on the same handle.

On a read-only mount, every operation that changes something fails with `EROFS`. A
writable mount adds these:

| Operation                   | Backend call                        | Result on a mount                                           |
| --------------------------- | ----------------------------------- | ----------------------------------------------------------- |
| create a file               | `pipe_file(path, b"")`              | An empty file, open for writing.                            |
| open for writing            | `cat_file` unless truncating        | The file's contents, held in memory.                        |
| write                       | none                                | Changes the memory copy; gaps past the end fill with zeros. |
| flush, `fsync`, close       | `pipe_file` if the copy changed     | Uploads the whole file. A failure returns its errno.        |
| truncate                    | `cat_file` unless to 0, `pipe_file` | Through the open copy when the file is open for writing.    |
| `mkdir`                     | `mkdir`                             | `EEXIST` if the path exists.                                |
| `rmdir`                     | `rmdir`                             | `ENOTEMPTY` unless the directory is empty.                  |
| delete a file               | `rm_file`                           | Removes the file.                                           |
| rename a file               | `mv`                                | Replaces an existing file at the target.                    |
| rename a directory          | `mkdir`, `mv` per file, `rmdir`     | The target must be missing or an empty directory.           |
| `chmod`, `chown`, set times | none                                | Accepted and ignored.                                       |

Rename returns `EEXIST` when `RENAME_NOREPLACE` is set and the target exists, and `EINVAL`
for `RENAME_EXCHANGE`. Renaming a directory onto a file returns `ENOTDIR`, and a file onto a
directory `EISDIR`.

## Attributes

| Field        | Files                                         | Directories                     |
| ------------ | --------------------------------------------- | ------------------------------- |
| mode         | `0444`, or `0644` on a writable mount         | `0555`, or `0755` when writable |
| size         | `size` from `info`, or of the open write copy | `0`                             |
| owner, group | the user and group that mounted               | the user and group that mounted |
| mtime        | `modified` from `info`, or 1970-01-01 without | same; object stores report none |
| atime, ctime | equal to mtime                                | equal to mtime                  |
| links        | 1                                             | 2                               |

The kernel caches attributes and lookups for 1 second.

## Errors

| Backend error             | errno     |
| ------------------------- | --------- |
| not found                 | `ENOENT`  |
| permission denied         | `EACCES`  |
| already exists            | `EEXIST`  |
| not a directory           | `ENOTDIR` |
| is a directory            | `EISDIR`  |
| invalid argument          | `EINVAL`  |
| not supported             | `ENOSYS`  |
| I/O error with an OS code | that code |
| any other error           | `EIO`     |

Python's `mount()` raises `OSError` before mounting: `EINVAL` for a protocol no backend
provides, and `FileNotFoundError` for a mount point that does not exist.

## Writes

A file opened for writing is held in memory in full until it is closed. Writes and reads
through that handle use the memory copy, and `stat` reports its size. Two handles open for
writing on the same file each hold their own copy, and the last one closed wins.

Object stores keep no directories of their own. A directory created through a writable
mount, or left empty after its last file is deleted or moved, stays visible in the mount
until `rmdir` removes it or the mount ends. Renames on object stores copy and then delete,
so they are not atomic.

## Mount table

Read-only mounts use the `ro` option. On Linux, `/proc/mounts` lists the URL as the source and
`fuse` as the type.

## Inodes

Each path gets an inode number the first time it is looked up. Numbers are not reused, so a
mount that visits many paths keeps a growing table for its lifetime.
