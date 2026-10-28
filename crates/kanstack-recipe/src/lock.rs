//! One orchestrator per repository: an exclusive `flock(2)` on `orchestrator.lock` in the
//! repository's [`RunStore`]. Two orchestrators driving the same runs would each write their
//! own snapshot over the other's, and could both launch the same step.
//!
//! The kernel releases the lock the moment the holder's file is closed — on drop, on an error
//! partway through, or when the process is killed — so a crashed orchestrator never leaves a
//! stale lock behind for the next one to clean up.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use crate::state::RunStore;

#[derive(Debug)]
pub enum LockError {
    /// Another process holds the lock at this path.
    Held(PathBuf),
    Io(PathBuf, io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Held(path) => write!(f, "another orchestrator is already running for this repository (it holds {})", path.display()),
            LockError::Io(path, e) => write!(f, "could not lock {}: {e}", path.display()),
        }
    }
}

impl std::error::Error for LockError {}

/// Held for as long as this orchestrator runs. There is deliberately no unlock method:
/// dropping it is the unlock.
pub struct OrchestratorLock(#[allow(dead_code, reason = "held only for its Drop, which releases the flock")] File);

impl OrchestratorLock {
    /// Takes the lock without waiting: a second orchestrator should refuse to start, not
    /// queue up behind the first.
    pub fn try_acquire(store: &RunStore) -> Result<Self, LockError> {
        let path = store.lock_path();
        let io_err = |e| LockError::Io(path.clone(), e);
        fs::create_dir_all(store.root()).map_err(io_err)?;
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            // Only the fd matters, for `flock`; the content is never read or written.
            .truncate(false)
            .open(&path)
            .map_err(io_err)?;
        // SAFETY: `flock` only reads the fd, which `file` keeps open for this call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = io::Error::last_os_error();
            return Err(if err.raw_os_error() == Some(libc::EWOULDBLOCK) { LockError::Held(path) } else { io_err(err) });
        }
        Ok(OrchestratorLock(file))
    }
}
