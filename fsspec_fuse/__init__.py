__version__ = "0.2.0"

# The extension links libfsspec from the fsspec_rs package, which puts its lib/ on the DLL path.
import fsspec_rs

from .fsspec_fuse import linked_to_libfsspec
from .mount import URL_XATTR, Mount, mount, resolve

__all__ = ("URL_XATTR", "Mount", "linked_to_libfsspec", "mount", "resolve")
