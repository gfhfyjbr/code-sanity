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
//!
//! Network filesystems: `flock` on NFS/SMB/CIFS may be host-local (or a no-op)
//! depending on mount options and server support, so two hosts can both hold
//! the "exclusive" lock and silently corrupt the workspace. The repo must live
//! on a local filesystem; a best-effort warning is logged when the lock file's
//! filesystem looks networked.

use crate::config::Layout;
use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;

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
        warn_once_on_network_fs(&path);
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

/// Best-effort, once per process: statfs the lock directory and warn when it
/// sits on a network filesystem, where flock may be host-local and two hosts
/// could both "hold" the exclusive lock. Any statfs failure stays silent —
/// this is advisory, never a gate.
fn warn_once_on_network_fs(path: &Path) {
    static CHECKED: std::sync::Once = std::sync::Once::new();
    CHECKED.call_once(|| {
        let dir = path.parent().unwrap_or(path);
        if let Some(fs_name) = network_fs_name(dir) {
            log::warn!(
                "workspace lock is on a {fs_name} filesystem; flock may not be \
                 enforced across hosts there — keep the repo on a local \
                 filesystem to avoid corruption from concurrent writers"
            );
        }
    });
}

#[cfg(target_os = "macos")]
fn network_fs_name(dir: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stats: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut stats) } != 0 {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(stats.f_fstypename.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    const NETWORK: &[&str] = &["nfs", "smbfs", "cifs", "afpfs", "webdav"];
    NETWORK.contains(&name.as_str()).then_some(name)
}

#[cfg(target_os = "linux")]
fn network_fs_name(dir: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stats: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut stats) } != 0 {
        return None;
    }
    // Magic numbers from linux/magic.h; anything else is treated as local.
    let name = match stats.f_type as i64 {
        0x6969 => "nfs",
        0xFF53_4D42 => "cifs",
        0xFE53_4D42 => "smb2",
        0x517B => "smb",
        _ => return None,
    };
    Some(name.to_string())
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
