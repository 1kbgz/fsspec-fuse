# Mount a remote filesystem

These guides mount a backend other than a local directory. Each one assumes FUSE is
installed and an empty directory exists for the mount point.

## Install FUSE

On Debian or Ubuntu, install FUSE 3:

```console
sudo apt-get install fuse3
```

On macOS, install [macFUSE](https://macfuse.github.io) and approve its system extension
when macOS asks:

```console
brew install --cask macfuse
```

## Mount an S3 bucket from the command line

Pass the URL, the mount point, and the backend's storage options with `-o`:

```console
fsspec-fuse s3://warehouse/reports /mnt/reports \
    -o region=us-east-1 -o key=ACCESS_KEY -o secret=SECRET_KEY
```

For an S3-compatible service such as MinIO, add its endpoint:

```console
fsspec-fuse s3://warehouse/reports /mnt/reports \
    -o endpoint_url=http://localhost:9000 -o region=us-east-1 \
    -o key=minioadmin -o secret=minioadmin
```

Use `-o anon=true` for a public bucket. On AWS, leave out `key` and `secret` to use the
standard credential chain. The command keeps running while the bucket is mounted; press
Ctrl-C to unmount. The S3 options are listed under "S3Fs" in the
[fsspec-rs reference](https://1kbgz.github.io/fsspec-rs/).

## Mount from Python

Call `mount` with storage options as keyword arguments, and use it as a context manager so
the filesystem unmounts when the block ends:

```python
import os

from fsspec_fuse import mount

with mount("s3://warehouse/reports", "/mnt/reports", region="us-east-1", anon=True):
    print(os.listdir("/mnt/reports"))
```

To keep the mount past one block, hold the returned object and call `unmount()` when done.

## Mount a backend that only exists in Python

Install the backend's package in the same environment, then mount its URL. fsspec-fuse
finds Python fsspec backends by protocol, so a public Google Cloud Storage bucket mounts
with gcsfs:

```console
python -m pip install gcsfs
fsspec-fuse gcs://my-public-bucket/data /mnt/data -o token=anon
```

Storage options reach the Python backend as strings.

## Write to a mount

Pass `--writable` to create, change, rename, and delete files:

```console
fsspec-fuse s3://warehouse/uploads /mnt/uploads -o region=us-east-1 --writable
```

From Python, pass `read_only=False`:

```python
from fsspec_fuse import mount

with mount("s3://warehouse/uploads", "/mnt/uploads", region="us-east-1", read_only=False):
    with open("/mnt/uploads/report.csv", "w") as f:
        f.write("a,b\n1,2\n")
```

Each file is uploaded when it is closed, so check for errors from `close()`; with a `with`
block, they are raised when the block ends. A file being written is held in memory, so
copy very large files with the backend's own tools instead.

## Serve more requests at once

Raise `--threads` (or `threads=` in Python) when many processes read the mount at once or
the backend has high latency:

```console
fsspec-fuse s3://warehouse/reports /mnt/reports -o anon=true --threads 16
```

The default is 4.

## Remove a mount left behind

If the mounting process was killed before it could unmount, the mount point reports
"Transport endpoint is not connected". Unmount it by hand:

```console
fusermount3 -u /mnt/reports   # Linux
umount /mnt/reports           # macOS
```
