# Mount a directory and read it through FUSE

In this tutorial we mount a local directory through fsspec-fuse, read it with ordinary
file operations, and unmount it. It runs on Linux.

## Before you start

Install FUSE 3 and fsspec-fuse in a Python 3.11 or newer environment:

```console
sudo apt-get install fuse3
python -m pip install fsspec-fuse
```

## Create files to mount

Make a source directory with two files and an empty directory to mount on:

```console
mkdir -p demo/source/notes demo/mnt
printf 'hello from fsspec\n' > demo/source/greeting.txt
printf 'first note\n' > demo/source/notes/one.txt
```

## Mount and read

Create `mount_demo.py`:

```python
import os
from pathlib import Path

from fsspec_fuse import mount

source = Path("demo/source").resolve()
mountpoint = Path("demo/mnt")

with mount(f"file://{source}", mountpoint):
    print(sorted(os.listdir(mountpoint)))
    print((mountpoint / "greeting.txt").read_text(), end="")
    print((mountpoint / "notes" / "one.txt").read_text(), end="")
```

Run it:

```console
python mount_demo.py
```

The output lists the files and prints their contents:

```text
['greeting.txt', 'notes']
hello from fsspec
first note
```

Inside the `with` block, `demo/mnt` showed the files in `demo/source`. Python read them
with `os.listdir` and `read_text`, the same calls it uses for any directory.

## Try to write

Add these lines inside the `with` block, after the `print` calls:

```python
    try:
        (mountpoint / "new.txt").write_text("hi")
    except OSError as error:
        print(error.strerror)
```

Run the script again. The last line of output is:

```text
Read-only file system
```

Mounts are read-only unless you ask for writes, so `demo/source` is unchanged.

## Check the unmount

After the script exits, list the mount point:

```console
ls demo/mnt
```

It prints nothing: leaving the `with` block unmounted the filesystem, and `demo/mnt` is an
empty directory again.

You have mounted a directory through fsspec-fuse and read it with standard file
operations. [Mount a remote filesystem](how-to.md) applies the same steps to S3 and other
backends.
