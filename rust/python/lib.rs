use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use fsspec_rs::{FsError, FsOptions, SharedFileSystem};
use pyo3::exceptions::PyOSError;
use pyo3::prelude::*;

/// Whether URLs resolve through the shared `libfsspec` rather than a registry compiled
/// into this extension.
const LINKED_TO_LIBFSSPEC: bool = cfg!(feature = "capi");

fn fs_error(error: FsError) -> PyErr {
    // OSError(errno, message) becomes FileNotFoundError and the other errno subclasses.
    PyOSError::new_err((::fsspec_fuse::errno(&error).code(), error.to_string()))
}

/// A mounted filesystem. Dropping it unmounts.
#[pyclass(name = "RustMount")]
struct RustMount {
    session: Mutex<Option<::fsspec_fuse::BackgroundSession>>,
}

#[pymethods]
impl RustMount {
    /// Whether the filesystem is still mounted.
    #[getter]
    fn mounted(&self) -> bool {
        let session = self.session.lock().unwrap_or_else(PoisonError::into_inner);
        session
            .as_ref()
            .is_some_and(|session| !session.guard.is_finished())
    }

    /// Unmounts and waits for the session threads to finish.
    fn unmount(&self, py: Python<'_>) -> PyResult<()> {
        let session = self
            .session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        match session {
            // Unmounted from outside, for example with fusermount3 -u: only wait for the
            // session threads, since unmounting again fails.
            Some(session) if session.guard.is_finished() => Ok(py.detach(|| session.join())?),
            Some(session) => Ok(py.detach(|| session.umount_and_join())?),
            None => Ok(()),
        }
    }
}

/// Mounts `url` at `mountpoint` and returns the session.
#[pyfunction]
#[pyo3(signature = (url, mountpoint, storage_options = None, threads = 4, read_only = true))]
fn mount(
    py: Python<'_>,
    url: &str,
    mountpoint: PathBuf,
    storage_options: Option<HashMap<String, String>>,
    threads: usize,
    read_only: bool,
) -> PyResult<RustMount> {
    let options: FsOptions = storage_options.unwrap_or_default().into_iter().collect();
    let (fs, root) = fsspec_rs::url_to_fs(url, &options).map_err(fs_error)?;
    let config = ::fsspec_fuse::config(url, threads, read_only);
    let fuse = ::fsspec_fuse::FsspecFuse::new(fs, root).source_url(url);
    let session = py.detach(|| ::fsspec_fuse::spawn(fuse, &mountpoint, &config))?;
    Ok(RustMount {
        session: Mutex::new(Some(session)),
    })
}

/// Mounts `root` of the Python fsspec filesystem `fs` at `mountpoint` and returns the
/// session. `source_url` is the URL of `root`, reported in each path's `user.fsspec.url`.
#[pyfunction]
#[pyo3(signature = (fs, root, mountpoint, source_url, threads = 4, read_only = true))]
fn mount_filesystem(
    py: Python<'_>,
    fs: Py<PyAny>,
    root: &str,
    mountpoint: PathBuf,
    source_url: String,
    threads: usize,
    read_only: bool,
) -> PyResult<RustMount> {
    let fs = fsspec_rs_bridge::PyFsspecFs::from_py_fs(fs).map_err(fs_error)?;
    let config = ::fsspec_fuse::config(fs.source_protocol(), threads, read_only);
    let fs: SharedFileSystem = Arc::new(fs);
    let fuse = ::fsspec_fuse::FsspecFuse::new(fs, root).source_url(source_url);
    let session = py.detach(|| ::fsspec_fuse::spawn(fuse, &mountpoint, &config))?;
    Ok(RustMount {
        session: Mutex::new(Some(session)),
    })
}

/// Whether this extension resolves URLs through the shared `libfsspec`.
#[pyfunction]
fn linked_to_libfsspec() -> bool {
    LINKED_TO_LIBFSSPEC
}

#[pymodule]
fn fsspec_fuse(m: &Bound<PyModule>) -> PyResult<()> {
    // Resolve URLs in the registry every extension in the process shares, which falls
    // back to Python fsspec for protocols no native backend registers.
    #[cfg(feature = "capi")]
    fsspec_sys::use_shared_registry();
    m.add_class::<RustMount>()?;
    m.add_function(wrap_pyfunction!(mount, m)?)?;
    m.add_function(wrap_pyfunction!(mount_filesystem, m)?)?;
    m.add_function(wrap_pyfunction!(linked_to_libfsspec, m)?)?;
    Ok(())
}
