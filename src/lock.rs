//! Cross-process workspace lock.
//!
//! One advisory `flock(2)` on `.code-sanity/tmp/apply.lock` serializes every
//! writer (apply, sync, index, project-edit, rename, recover); readers (MCP
//! read/search, verify, output-sanitizer construction) take the shared side so
//! they never observe a torn mirror/map/db snapshot. Unlike the old
//! `create_new` sentinel, the kernel releases the lock automatically when the
//! process exits, so a crash mid-apply never wedges the workspace.
//!
//! flock is per open file description: never acquire a second lock on the same
//! workspace from a process already holding one — it self-deadlocks. In
//! practice: entry points take the lock exactly once via
//! `index::init_workspace_locked` (or a direct `acquire`) and pass control to
//! `_locked` functions from there; `acquire`/`acquire_shared` must be
//! unreachable while the same process already holds a workspace lock.

use crate::config::Layout;
use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;

pub struct WorkspaceLock {
    _file: File,
}

impl WorkspaceLock {
    /// Block until the exclusive (writer) workspace lock is held.
    pub fn acquire(layout: &Layout) -> Result<Self> {
        Self::acquire_kind(layout, libc::LOCK_EX)
    }

    /// Block until a shared (reader) workspace lock is held. Readers coexist;
    /// writers wait for them and vice versa.
    pub fn acquire_shared(layout: &Layout) -> Result<Self> {
        Self::acquire_kind(layout, libc::LOCK_SH)
    }

    fn acquire_kind(layout: &Layout, op: libc::c_int) -> Result<Self> {
        std::fs::create_dir_all(&layout.tmp_dir)
            .with_context(|| format!("create {}", layout.tmp_dir.display()))?;
        let path = layout.tmp_dir.join("apply.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("open workspace lock {}", path.display()))?;
        loop {
            let rc = unsafe { libc::flock(file.as_raw_fd(), op) };
            if rc == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err).with_context(|| format!("acquire workspace lock {}", path.display()));
        }
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_locks_coexist() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        let _a = WorkspaceLock::acquire_shared(&layout).unwrap();
        let _b = WorkspaceLock::acquire_shared(&layout).unwrap();
    }
}
