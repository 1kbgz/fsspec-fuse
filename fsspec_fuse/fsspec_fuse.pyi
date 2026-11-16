import os

class RustMount:
    @property
    def mounted(self) -> bool: ...
    def unmount(self) -> None: ...

def mount(
    url: str,
    mountpoint: str | os.PathLike[str],
    storage_options: dict[str, str] | None = None,
    threads: int = 4,
    read_only: bool = True,
) -> RustMount: ...
def mount_filesystem(
    fs: object,
    root: str,
    mountpoint: str | os.PathLike[str],
    source_url: str,
    threads: int = 4,
    read_only: bool = True,
) -> RustMount: ...
def linked_to_libfsspec() -> bool: ...
