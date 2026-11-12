__version__ = "0.1.0"

# The extension links libfsspec from the fsspec_rs package, which puts its lib/ on the DLL path.
import fsspec_rs

from .fsspec_fuse import linked_to_libfsspec
from .mount import Mount, mount

__all__ = ("Mount", "linked_to_libfsspec", "mount")
