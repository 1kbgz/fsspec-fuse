from __future__ import annotations

import os
from typing import Self

from fsspec import AbstractFileSystem
from fsspec.core import url_to_fs

from .fsspec_fuse import RustMount, mount as _mount, mount_filesystem as _mount_filesystem


def _option(value: object) -> str:
    """Storage options cross into Rust as strings; booleans use the registry's spelling."""
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


class Mount:
    """A mount of an fsspec URL. Unmounts on ``unmount()``, on context exit, or when
    garbage collected."""

    def __init__(self, url: str | AbstractFileSystem, mountpoint: str, session: RustMount):
        self.url = url
        self.mountpoint = mountpoint
        self._session = session

    @property
    def mounted(self) -> bool:
        """Whether the filesystem is still mounted. False after ``unmount()`` or an
        unmount from outside, such as ``fusermount3 -u``."""
        return self._session.mounted

    def unmount(self) -> None:
        """Unmount and wait for the mount's threads to stop. Calling it again does nothing."""
        self._session.unmount()

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *args) -> None:
        self.unmount()

    def __repr__(self) -> str:
        state = "mounted" if self.mounted else "unmounted"
        return f"Mount({self.url!r}, {self.mountpoint!r}, {state})"


def mount(
    url: str | AbstractFileSystem,
    mountpoint: str | os.PathLike[str],
    *,
    path: str = "",
    threads: int = 4,
    read_only: bool = True,
    **storage_options,
) -> Mount:
    """Mount ``url`` at the existing directory ``mountpoint``.

    The URL resolves through the fsspec-rs registry shared by every extension in the
    process: native backends such as ``file://``, ``s3://``, ``db+sqlite://``, or ``kafka://``
    when their packages are imported, and any Python fsspec backend otherwise.
    ``threads`` sets how many requests the mount serves at once.

    ``url`` may also be a Python fsspec filesystem, mounted from ``path``, or a chained URL
    such as ``dataview::s3://bucket/data``, which Python fsspec opens with
    ``storage_options`` as given (for example ``dataview={"mapping": {...}}``).

    With ``read_only=False`` the mount accepts changes. A file opened for writing is
    held in memory and written to the filesystem when it is flushed or closed, so a
    failed upload surfaces as an error from ``close()``.
    """
    mountpoint = os.fspath(mountpoint)
    if isinstance(url, str) and "::" in url:
        fs, path = url_to_fs(url, **storage_options)
        return Mount(url, mountpoint, _mount_filesystem(fs, path, mountpoint, threads, read_only))
    if isinstance(url, AbstractFileSystem):
        if storage_options:
            raise TypeError("storage options apply to URLs, not filesystem objects")
        return Mount(url, mountpoint, _mount_filesystem(url, path, mountpoint, threads, read_only))
    if path:
        raise TypeError("path applies to filesystem objects; put it in the URL")
    options = {key: _option(value) for key, value in storage_options.items()}
    return Mount(url, mountpoint, _mount(url, mountpoint, options, threads, read_only))
