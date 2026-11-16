from __future__ import annotations

import os
import weakref
from typing import Any, Self

from fsspec import AbstractFileSystem
from fsspec.core import url_to_fs

from .fsspec_fuse import RustMount, mount as _mount, mount_filesystem as _mount_filesystem

URL_XATTR = "user.fsspec.url"

# Mounts made in this process by real mountpoint, so resolve() can return their filesystems.
_MOUNTS: weakref.WeakValueDictionary[str, Mount] = weakref.WeakValueDictionary()


def _option(value: object) -> str:
    """Storage options cross into Rust as strings; booleans use the registry's spelling."""
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


class Mount:
    """A mount of an fsspec URL. Unmounts on ``unmount()``, on context exit, or when
    garbage collected."""

    def __init__(
        self,
        url: str | AbstractFileSystem,
        mountpoint: str,
        session: RustMount,
        fs: AbstractFileSystem | None = None,
        root: str = "",
        storage_options: dict[str, Any] | None = None,
    ):
        self.url = url
        self.mountpoint = mountpoint
        self._session = session
        self._fs = fs
        self._root = root
        self._storage_options = storage_options or {}
        _MOUNTS[os.path.realpath(mountpoint)] = self

    def filesystem(self) -> tuple[AbstractFileSystem, str]:
        """The Python fsspec filesystem behind the mount and the path of its root. A URL
        mount opens its URL with Python fsspec the first time this is called."""
        if self._fs is None:
            self._fs, self._root = url_to_fs(self.url, **self._storage_options)
        return self._fs, self._root

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
        session = _mount_filesystem(fs, path, mountpoint, url, threads, read_only)
        return Mount(url, mountpoint, session, fs, path)
    if isinstance(url, AbstractFileSystem):
        if storage_options:
            raise TypeError("storage options apply to URLs, not filesystem objects")
        session = _mount_filesystem(url, path, mountpoint, url.unstrip_protocol(path), threads, read_only)
        return Mount(url, mountpoint, session, url, path)
    if path:
        raise TypeError("path applies to filesystem objects; put it in the URL")
    options = {key: _option(value) for key, value in storage_options.items()}
    return Mount(url, mountpoint, _mount(url, mountpoint, options, threads, read_only), storage_options=storage_options)


def resolve(path: str | os.PathLike[str]) -> tuple[AbstractFileSystem, str]:
    """The fsspec filesystem and path behind ``path`` on a mount.

    A mount made in this process resolves to the filesystem it mounted, with its storage
    options. Other mounts resolve through the path's ``user.fsspec.url`` attribute (Linux
    only), opened without storage options. Raises ``ValueError`` for a path on no mount.
    """
    path = os.path.realpath(path)
    for point in sorted(_MOUNTS.keys(), key=len, reverse=True):
        mount = _MOUNTS.get(point)
        if mount is None or not mount.mounted or not (path == point or path.startswith(point + os.sep)):
            continue
        fs, root = mount.filesystem()
        rel = os.path.relpath(path, point).replace(os.sep, "/")
        if rel == ".":
            return fs, root
        return fs, f"{root.rstrip('/')}/{rel}" if root else rel
    try:
        url = os.getxattr(path, URL_XATTR).decode()
    except (AttributeError, OSError) as error:
        raise ValueError(f"{path} is not on an fsspec mount") from error
    return url_to_fs(url)
