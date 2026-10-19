//! Isolated storage for app integration tests, including their background work.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

static TEST_LEASE: Mutex<()> = Mutex::new(());
static TEST_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Keep this guard alive until the test app and its tasks have been dropped.
/// Guards serialize test apps because storage paths are process-wide.
#[must_use = "keep the isolated storage guard alive until the test app has been dropped"]
pub struct TestRoot {
    _lease: MutexGuard<'static, ()>,
}

pub fn set_root(root: PathBuf) -> TestRoot {
    let lease = TEST_LEASE.lock().unwrap_or_else(|error| error.into_inner());
    assert!(root.is_absolute(), "test storage must use an absolute root");
    *TEST_ROOT.lock().unwrap_or_else(|error| error.into_inner()) = Some(root);
    TestRoot { _lease: lease }
}

pub(crate) fn root() -> Option<PathBuf> {
    TEST_ROOT
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        *TEST_ROOT.lock().unwrap_or_else(|error| error.into_inner()) = None;
    }
}
