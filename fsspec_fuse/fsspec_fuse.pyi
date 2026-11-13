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
) -> RustMount: ...
def linked_to_libfsspec() -> bool: ...
