# How one mount reaches Rust and Python backends

fsspec-fuse is one program that mounts URLs served by Rust backends, such as `file://` and
`s3://` from fsspec-rs, and URLs served only by Python packages, such as gcsfs. This page
explains how that works, what it costs, and why mounts start out read-only.

## One registry per process

fsspec-rs ships a shared library, `libfsspec`, that holds the filesystem registry for the
whole process. Rust extensions such as fsspec-db and fsspec-kafka register their protocols
there, and fsspec-fuse looks URLs up there. When no native backend claims a protocol, the
registry's fallback hands the URL to Python fsspec, which finds the installed backend by
protocol name.

The alternative would be a mount program per backend, or one that only knows the backends
it was compiled with. `fsspec.fuse.run`, the mount helper inside fsspec, takes the opposite
route: it is pure Python, so it reaches every Python backend, but every request runs
Python code. fsspec-fuse keeps native requests out of the interpreter and uses Python only
for the backends that exist only there.

## The cost of a Python backend

A request against a Python backend has to take the GIL. Each FUSE request runs on one of
the mount's threads, which waits for the GIL, calls the backend, and replies. The Python
code in those calls runs one request at a time, though backends that release the GIL while
waiting on the network, as most do, still overlap their waits. Native backends do not
touch the interpreter, so their requests run fully in parallel.

The GIL also explains how a process can mount a Python backend and read from it at the
same time. Python releases the GIL during blocking file calls such as `os.listdir` and
`open`, so a mount thread can take it while the reading thread waits. Native code that
reads the mount while holding the GIL would block the thread that serves it. Running the
mount in its own process, as the `fsspec-fuse` command does, avoids that case.

## Why mounts are read-only

A local filesystem lets a program write a few bytes into the middle of a file. Object
stores do not: S3 replaces a whole object on every write. A writable mount over S3 has to
collect a file's writes and upload the complete object when the file closes, and it has to
report a failed upload on close rather than on the write that caused it. Renames become a
copy followed by a delete.

mountpoint-s3, AWS's S3 mount, handles this by allowing only sequential writes to new
files. Another approach buffers each open file, which costs memory or temporary disk but
allows random writes. fsspec-fuse started read-only so that behavior is the same on every
backend while a write design is chosen.

## When changes appear

A FUSE filesystem tells the kernel how long it may cache what it learns. fsspec-fuse lets
the kernel cache attributes and lookups for one second and lists a directory once per
open. A file changed by another writer appears within about a second, and nothing
coordinates between separate mounts of the same bucket. Longer caching would cut requests
to remote backends at the cost of staler listings.
