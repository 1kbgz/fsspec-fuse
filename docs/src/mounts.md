# Mount behavior reference

## Operations

| Operation                         | Backend call                   | Result on a mount                                    |
| --------------------------------- | ------------------------------ | ---------------------------------------------------- |
| `stat`, path lookup               | `info`                         | Attributes below.                                    |
| list a directory                  | `ls(detail=True)`              | `.`, `..`, then the backend's entries.               |
| open for reading                  | `open(mode="rb")`              | One backend file per open handle.                    |
| read                              | seek and read on the open file | Bytes from the requested offset.                     |
| `statfs`                          | none                           | Zero counts, block size 4096, names up to 255 bytes. |
| open for writing, create, `mkdir` | none                           | `EROFS`.                                             |
| rename, delete, `chmod`, `chown`  | none                           | `EROFS`.                                             |

A directory's listing is taken once when it is opened, so entries do not shift between
`readdir` calls on the same handle.

## Attributes

| Field        | Files                                         | Directories                     |
| ------------ | --------------------------------------------- | ------------------------------- |
| mode         | `0444` (`r--r--r--`)                          | `0555` (`r-xr-xr-x`)            |
| size         | `size` from `info`                            | `0`                             |
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

## Mount table

Mounts use the `ro` option. On Linux, `/proc/mounts` lists the URL as the source and
`fuse` as the type.

## Inodes

Each path gets an inode number the first time it is looked up. Numbers are not reused, so a
mount that visits many paths keeps a growing table for its lifetime.
