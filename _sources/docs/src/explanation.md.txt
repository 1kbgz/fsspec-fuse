# How one mount reaches Rust and Python backends

fsspec-fuse is one program that mounts URLs served by Rust backends, such as `file://` and
`s3://` from fsspec-rs, and URLs served only by Python packages, such as gcsfs. This page
explains how that works, what it costs, and how writable mounts handle object stores.

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

## Why writes go through a buffer

A local filesystem lets a program write a few bytes into the middle of a file. Object
stores do not: S3 replaces a whole object on every write. fsspec-fuse treats every backend
as an object store, so a file opened for writing is copied into memory, changed there, and
uploaded in full when the program closes or syncs it. That works the same on S3, a local
directory, or a Python backend, and it allows writes at any offset.

The costs follow from the design. A file being written uses as much memory as its size,
and opening a large file to append one line downloads all of it first. A failed upload
surfaces from `close()` rather than from the `write()` that caused it, so programs that
ignore errors from `close()` can miss it. mountpoint-s3, AWS's S3 mount, makes the other
trade: it streams sequential writes to new files as a multipart upload and rejects
anything else. Streaming large sequential files may come later as an addition to the
buffer.

Directories need similar care. An object store has no directories, only keys with slashes
in them, so a directory exists while something is stored below it. A program that runs
`mkdir` and then lists its parent expects to see the new directory, so the mount remembers
directories it created, and ones left empty by a delete or a move, until they are removed.
Renames are a copy followed by a delete, file by file, because object stores have no
rename.

Mounts stay read-only unless writes are requested. A read-only mount cannot lose or
overwrite data, and most mounts of remote data are only read.

## When changes appear

A FUSE filesystem tells the kernel how long it may cache what it learns. fsspec-fuse lets
the kernel cache attributes and lookups for one second and lists a directory once per
open. A file changed by another writer appears within about a second, and nothing
coordinates between separate mounts of the same bucket. Longer caching would cut requests
to remote backends at the cost of staler listings.
