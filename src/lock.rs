//! Cross-process workspace lock.
//!
//! One advisory `flock(2)` on `.code-sanity/tmp/apply.lock` serializes every
//! writer (apply, sync, index, project-edit, rename, recover). Unlike the old
//! `create_new` sentinel, the kernel releases the lock automatically when the
//! process exits, so a crash mid-apply never wedges the workspace.

use crate::config::Layout;
use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;

pub struct WorkspaceLock {
    _file: File,
}

impl WorkspaceLock {
    /// Block until the exclusive workspace lock is held.
    pub fn acquire(layout: &Layout) -> Result<Self> {
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
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err)
                .with_context(|| format!("acquire workspace lock {}", path.display()));
        }
        Ok(Self { _file: file })
    }
}
