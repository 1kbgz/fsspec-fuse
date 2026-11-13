# fsspec-fuse

Mount fsspec filesystems with FUSE

[![Build Status](https://github.com/1kbgz/fsspec-fuse/actions/workflows/build.yaml/badge.svg?branch=main&event=push)](https://github.com/1kbgz/fsspec-fuse/actions/workflows/build.yaml)
[![codecov](https://codecov.io/gh/1kbgz/fsspec-fuse/branch/main/graph/badge.svg)](https://codecov.io/gh/1kbgz/fsspec-fuse)
[![License](https://img.shields.io/github/license/1kbgz/fsspec-fuse)](https://github.com/1kbgz/fsspec-fuse)
[![PyPI](https://img.shields.io/pypi/v/fsspec-fuse.svg)](https://pypi.python.org/pypi/fsspec-fuse)

## Overview

`fsspec-fuse` mounts an fsspec URL as a read-only local directory, so any program can
list and read it with ordinary file operations.

```bash
fsspec-fuse s3://my-bucket/reports /mnt/reports -o region=us-east-1
ls /mnt/reports
```

URLs resolve through the filesystem registry that [fsspec-rs](https://github.com/1kbgz/fsspec-rs)
shares between extensions. Native backends (`file://`, `s3://`, and protocols registered by
packages such as fsspec-db and fsspec-kafka) run in Rust. Any other protocol falls back to
the Python fsspec implementation installed for it, such as gcsfs or adlfs.

From Python, `mount` returns a handle that unmounts on `unmount()` or when its `with` block
ends:

```python
import os

from fsspec_fuse import mount

with mount("s3://my-bucket/reports", "/mnt/reports", region="us-east-1"):
    print(os.listdir("/mnt/reports"))
```

Storage options pass to the backend as strings. `threads` sets how many requests the mount
serves at once (default 4).

## Requirements

- Linux: FUSE 3 (`fusermount3`, from the `fuse3` package on Debian and Ubuntu).
- macOS: [macFUSE](https://macfuse.github.io).
- Windows is not supported yet.

Mounts are read-only. Writes, creating files, and renames fail with `EROFS`.

> [!NOTE]
> This library was generated using [copier](https://copier.readthedocs.io/en/stable/) from the [Base Python Project Template repository](https://github.com/python-project-templates/base).
