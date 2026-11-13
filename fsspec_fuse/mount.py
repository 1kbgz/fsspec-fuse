from __future__ import annotations

import os
from typing import Self

from .fsspec_fuse import RustMount, mount as _mount


def _option(value: object) -> str:
    """Storage options cross into Rust as strings; booleans use the registry's spelling."""
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


class Mount:
    """A read-only mount of an fsspec URL. Unmounts on ``unmount()``, on context exit,
    or when garbage collected."""

    def __init__(self, url: str, mountpoint: str, session: RustMount):
        self.url = url
        self.mountpoint = mountpoint
        self._session = session

    @property
    def mounted(self) -> bool:
        return self._session.mounted

    def unmount(self) -> None:
        self._session.unmount()

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *args) -> None:
        self.unmount()

    def __repr__(self) -> str:
        state = "mounted" if self.mounted else "unmounted"
        return f"Mount({self.url!r}, {self.mountpoint!r}, {state})"


def mount(url: str, mountpoint: str | os.PathLike[str], *, threads: int = 4, **storage_options) -> Mount:
    """Mount ``url`` read-only at the existing directory ``mountpoint``.

    The URL resolves through the fsspec-rs registry shared by every extension in the
    process: native backends such as ``file://``, ``s3://``, ``db+sqlite://``, or ``kafka://``
    when their packages are imported, and any Python fsspec backend otherwise.
    ``threads`` sets how many requests the mount serves at once.
    """
    mountpoint = os.fspath(mountpoint)
    options = {key: _option(value) for key, value in storage_options.items()}
    return Mount(url, mountpoint, _mount(url, mountpoint, options, threads))
