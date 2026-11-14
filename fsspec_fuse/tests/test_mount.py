import errno
import os
import shutil
import subprocess
import sys
import time
import uuid

import fsspec
import fsspec_rs
import pytest
from fsspec.implementations.memory import MemoryFileSystem

from fsspec_fuse import linked_to_libfsspec, mount
from fsspec_fuse.cli import _options
from fsspec_fuse.mount import _option


def _fuse_available() -> bool:
    if sys.platform == "linux":
        return os.path.exists("/dev/fuse") and shutil.which("fusermount3") is not None
    if sys.platform == "darwin":
        return os.path.exists("/Library/Filesystems/macfuse.fs")
    return False


needs_fuse = pytest.mark.skipif(not _fuse_available(), reason="FUSE is not available")


@pytest.fixture
def mounted(tmp_path):
    mounts = []

    def _mount(url, **storage_options):
        point = tmp_path / f"mnt{len(mounts)}"
        point.mkdir()
        try:
            session = mount(url, point, **storage_options)
        except OSError as error:
            # Hosted macOS runners install macFUSE but cannot load its kernel extension.
            if sys.platform != "linux":
                pytest.skip(f"cannot mount here: {error}")
            raise
        mounts.append(session)
        return point

    yield _mount
    for session in mounts:
        session.unmount()


class PythonMemory(MemoryFileSystem):
    """fsspec's memory filesystem under a protocol no native backend registers."""

    protocol = ("python-memory",)

    @classmethod
    def _strip_protocol(cls, path):
        return super()._strip_protocol(path.replace("python-memory://", "memory://", 1))


@needs_fuse
def test_mounts_a_local_directory(mounted, tmp_path):
    source = tmp_path / "source"
    (source / "sub").mkdir(parents=True)
    (source / "a.txt").write_bytes(b"hello world")
    (source / "sub" / "b.txt").write_bytes(b"nested")

    point = mounted(f"file://{source}")
    assert sorted(os.listdir(point)) == ["a.txt", "sub"]
    assert (point / "a.txt").read_bytes() == b"hello world"
    assert (point / "sub" / "b.txt").read_bytes() == b"nested"
    assert os.stat(point / "a.txt").st_size == 11
    assert (point / "sub").is_dir()
    with open(point / "a.txt", "rb") as f:
        f.seek(6)
        assert f.read(3) == b"wor"
    with pytest.raises(FileNotFoundError):
        os.stat(point / "missing")


@needs_fuse
def test_mount_is_read_only(mounted, tmp_path):
    source = tmp_path / "source"
    source.mkdir()
    (source / "a.txt").write_bytes(b"x")

    def write(path):
        with open(path, "wb"):
            pass

    point = mounted(f"file://{source}")
    for change, path in ((write, point / "a.txt"), (write, point / "new.txt"), (os.mkdir, point / "d")):
        with pytest.raises(OSError) as error:
            change(path)
        assert error.value.errno == errno.EROFS
    assert (source / "a.txt").read_bytes() == b"x"


@needs_fuse
def test_writable_mount_changes_the_source(mounted, tmp_path):
    source = tmp_path / "source"
    (source / "sub").mkdir(parents=True)
    (source / "a.txt").write_bytes(b"hello")

    point = mounted(f"file://{source}", read_only=False)
    (point / "new.txt").write_bytes(b"created")
    assert (source / "new.txt").read_bytes() == b"created"
    with open(point / "a.txt", "ab") as f:
        f.write(b" world")
    assert (source / "a.txt").read_bytes() == b"hello world"
    with open(point / "a.txt", "r+b") as f:
        f.write(b"J")
        f.seek(0)
        assert f.read() == b"Jello world"
    assert (source / "a.txt").read_bytes() == b"Jello world"
    os.truncate(point / "a.txt", 5)
    assert (point / "a.txt").read_bytes() == b"Jello"

    os.mkdir(point / "made")
    assert (source / "made").is_dir()
    os.rename(point / "new.txt", point / "made" / "moved.txt")
    shutil.copy(point / "a.txt", point / "made" / "copy.txt")
    assert sorted(os.listdir(source / "made")) == ["copy.txt", "moved.txt"]
    os.unlink(point / "made" / "copy.txt")
    with pytest.raises(OSError) as error:
        os.rmdir(point / "made")
    assert error.value.errno == errno.ENOTEMPTY
    os.rename(point / "made", point / "renamed")
    assert (source / "renamed" / "moved.txt").read_bytes() == b"created"
    shutil.rmtree(point / "renamed")
    assert sorted(os.listdir(source)) == ["a.txt", "sub"]


@needs_fuse
def test_writes_reach_a_python_only_backend(mounted):
    fsspec.register_implementation("python-memory", PythonMemory, clobber=True)
    root = f"/{uuid.uuid4().hex}"
    memory = PythonMemory()
    memory.mkdir(root)
    try:
        point = mounted(f"python-memory://{root}", read_only=False)
        os.mkdir(point / "data")
        (point / "data" / "values.csv").write_text("a,b\n1,2\n")
        assert memory.cat_file(f"{root}/data/values.csv") == b"a,b\n1,2\n"
        os.rename(point / "data" / "values.csv", point / "values.csv")
        assert memory.cat_file(f"{root}/values.csv") == b"a,b\n1,2\n"
        assert sorted(os.listdir(point)) == ["data", "values.csv"]
    finally:
        memory.rm(root, recursive=True)


@needs_fuse
def test_mounts_a_python_only_backend(mounted):
    fsspec.register_implementation("python-memory", PythonMemory, clobber=True)
    root = f"/{uuid.uuid4().hex}"
    memory = PythonMemory()
    memory.pipe_file(f"{root}/data/values.csv", b"a,b\n1,2\n")
    memory.pipe_file(f"{root}/readme.txt", b"from python")
    try:
        point = mounted(f"python-memory://{root}")
        assert sorted(os.listdir(point)) == ["data", "readme.txt"]
        assert (point / "data" / "values.csv").read_bytes() == b"a,b\n1,2\n"
        assert (point / "readme.txt").read_text() == "from python"
    finally:
        memory.rm(root, recursive=True)


@needs_fuse
def test_unmount_and_context_manager(tmp_path):
    source = tmp_path / "source"
    source.mkdir()
    (source / "a.txt").write_bytes(b"x")
    point = tmp_path / "mnt"
    point.mkdir()
    try:
        session = mount(f"file://{source}", point)
    except OSError as error:
        if sys.platform != "linux":
            pytest.skip(f"cannot mount here: {error}")
        raise
    with session:
        assert session.mounted
        assert os.listdir(point) == ["a.txt"]
    assert not session.mounted
    assert os.listdir(point) == []
    session.unmount()


@pytest.mark.skipif(sys.platform != "linux" or not _fuse_available(), reason="needs fusermount3")
def test_unmount_after_an_external_unmount(tmp_path):
    source = tmp_path / "source"
    source.mkdir()
    point = tmp_path / "mnt"
    point.mkdir()
    session = mount(f"file://{source}", point)
    subprocess.run(["fusermount3", "-u", str(point)], check=True)
    for _ in range(50):
        if not session.mounted:
            break
        time.sleep(0.1)
    assert not session.mounted
    session.unmount()


def test_shares_the_registry_with_fsspec_rs():
    assert linked_to_libfsspec() == fsspec_rs.fsspec_rs.linked_to_libfsspec()


def test_storage_options_become_strings():
    assert _option(True) == "true"
    assert _option(False) == "false"
    assert _option(3) == "3"
    assert _options(["region=us-east-1", "endpoint_url=http://a=b"]) == {"region": "us-east-1", "endpoint_url": "http://a=b"}
    with pytest.raises(Exception, match="KEY=VALUE"):
        _options(["region"])
