//! Durable filesystem primitives shared by every write path.
//!
//! Two tiers:
//! - `atomic_write_sync`: temp file in the same directory -> write ->
//!   fsync(file) -> rename over the target -> fsync(parent dir). A power
//!   failure leaves either the old or the new content, never a torn or
//!   zero-length file. For state that cannot be recomputed: real repo files,
//!   journal entries, config (salt + registry), stashes.
//! - `atomic_write` / `atomic_write_if_changed`: same temp+rename atomicity
//!   without the fsyncs. For derived state (mirror, span maps) where a lost
//!   rename after power loss is repaired by `sync`/`verify`, and per-file
//!   fsyncs would dominate full-index time (macOS fsync is ~tens of ms).
//!
//! macOS caveat: `sync_all` maps to `fsync(2)`, which on macOS does not flush
//! the drive's own write cache (`F_FULLFSYNC` would, at ~10-100x the cost).
//! The write ORDERING the recovery protocol relies on (journal entry durable
//! before real files change) still holds; a full power loss can lose the
//! newest few writes as a unit — a documented trade-off, not a torn state.

use anyhow::{Context, Result, anyhow};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static WRITE_NONCE: AtomicU64 = AtomicU64::new(0);

fn temp_path_for(path: &Path, parent: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let nonce = WRITE_NONCE.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(
        ".{file_name}.code-sanity-tmp-{}-{nonce}",
        std::process::id()
    ))
}

fn atomic_write_impl(path: &Path, content: &str, durable: bool) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    if durable {
        // Syncing only the immediate parent is not enough for a fresh subtree:
        // if the directories themselves were just created, their entries in
        // the grandparent are volatile and power loss drops the whole subtree
        // — journal entry included.
        create_dir_all_synced(parent)?;
    } else {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let tmp = temp_path_for(path, parent);
    let result = (|| -> Result<()> {
        let mut file =
            fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        file.write_all(content.as_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
        if durable {
            file.sync_all()
                .with_context(|| format!("fsync {}", tmp.display()))?;
        }
        drop(file);
        fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
        if durable {
            fs::File::open(parent)
                .and_then(|dir| dir.sync_all())
                .with_context(|| format!("fsync {}", parent.display()))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Atomically replace `path` with `content` and make the replacement durable
/// (file fsync + parent directory fsync). Creates missing parent directories.
pub fn atomic_write_sync(path: &Path, content: &str) -> Result<()> {
    atomic_write_impl(path, content, true)
}

/// Atomically replace `path` with `content` (temp + rename, no fsync). For
/// derived state that `sync`/`verify` can rebuild after a power loss.
pub fn atomic_write(path: &Path, content: &str) -> Result<()> {
    atomic_write_impl(path, content, false)
}

/// `atomic_write` unless the file already holds exactly `content`. Returns
/// whether a write happened.
pub fn atomic_write_if_changed(path: &Path, content: &str) -> Result<bool> {
    if fs::read_to_string(path).ok().as_deref() == Some(content) {
        return Ok(false);
    }
    atomic_write(path, content)?;
    Ok(true)
}

/// Atomic durable write that first preserves existing, different content in a
/// sibling `.bak` file, so a config a human may have edited is never silently
/// destroyed. The backup itself is written atomically too.
pub fn write_with_backup_sync(path: &Path, content: &str) -> Result<()> {
    if let Ok(existing) = fs::read_to_string(path) {
        if existing == content {
            return Ok(());
        }
        let backup = backup_path(path);
        atomic_write_sync(&backup, &existing)
            .with_context(|| format!("back up {}", path.display()))?;
    }
    atomic_write_sync(path, content)
}

/// `create_dir_all`, then fsync every directory that was just created plus
/// the deepest pre-existing ancestor (whose entry list changed). Durable-tier
/// only. A racing concurrent creator merely causes an extra fsync.
pub(crate) fn create_dir_all_synced(dir: &Path) -> Result<()> {
    let mut missing = Vec::new();
    let mut probe = dir;
    let existing_ancestor = loop {
        if probe.as_os_str().is_empty() || probe.exists() {
            break probe;
        }
        missing.push(probe.to_path_buf());
        match probe.parent() {
            Some(parent) => probe = parent,
            None => break probe,
        }
    };
    if missing.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mut to_sync: Vec<&Path> = vec![existing_ancestor];
    to_sync.extend(missing.iter().rev().map(PathBuf::as_path));
    for created in to_sync {
        if created.as_os_str().is_empty() {
            continue;
        }
        fs::File::open(created)
            .and_then(|handle| handle.sync_all())
            .with_context(|| format!("fsync {}", created.display()))?;
    }
    Ok(())
}

/// Durably remove a file: unlink + parent-dir fsync. `Ok(false)` when it was
/// already gone. For records whose absence carries meaning (in-flight journal
/// markers): a marker resurrected by power loss would block the workspace.
pub(crate) fn remove_file_sync(path: &Path) -> Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).with_context(|| format!("remove {}", path.display())),
    }
    if let Some(parent) = path.parent() {
        fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .with_context(|| format!("fsync {}", parent.display()))?;
    }
    Ok(true)
}

/// Whether `file_name` matches the temp naming of [`atomic_write_impl`]. A
/// SIGKILL or power loss between temp creation and rename strands such a file;
/// since every writer runs under the exclusive workspace lock, any temp file
/// observed by a lock holder is garbage from a dead process.
pub fn is_stale_temp_file(file_name: &str) -> bool {
    file_name.starts_with('.') && file_name.contains(".code-sanity-tmp-")
}

/// Recursively delete stranded atomic-write temp files under `dir`. Only call
/// while holding the exclusive workspace lock. Returns how many were removed.
pub fn remove_stale_temp_files(dir: &Path) -> Result<usize> {
    remove_stale_temp_files_impl(dir, true)
}

/// Non-recursive variant for directories in the real repository, where a deep
/// walk could be arbitrarily large.
pub fn remove_stale_temp_files_shallow(dir: &Path) -> Result<usize> {
    remove_stale_temp_files_impl(dir, false)
}

fn remove_stale_temp_files_impl(dir: &Path, recurse: bool) -> Result<usize> {
    let mut removed = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = match fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err).with_context(|| format!("read {}", current.display())),
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("read entry in {}", current.display()))?;
            let file_type = entry
                .file_type()
                .with_context(|| format!("stat {}", entry.path().display()))?;
            if file_type.is_dir() {
                if recurse {
                    stack.push(entry.path());
                }
            } else if file_type.is_file()
                && entry.file_name().to_str().is_some_and(is_stale_temp_file)
            {
                fs::remove_file(entry.path())
                    .with_context(|| format!("remove {}", entry.path().display()))?;
                removed += 1;
            }
        }
    }
    Ok(removed)
}

fn backup_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    path.with_file_name(format!("{file_name}.bak"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_creates_parents_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c.txt");
        atomic_write_sync(&path, "hello").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
        atomic_write_sync(&path, "world").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "world");
    }

    #[test]
    fn atomic_write_if_changed_skips_identical_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        assert!(atomic_write_if_changed(&path, "x").unwrap());
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        assert!(!atomic_write_if_changed(&path, "x").unwrap());
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), mtime);
        assert!(atomic_write_if_changed(&path, "y").unwrap());
    }

    #[test]
    fn backup_preserves_previous_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_with_backup_sync(&path, "v1").unwrap();
        assert!(!path.with_file_name("config.toml.bak").exists());
        write_with_backup_sync(&path, "v2").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "v2");
        assert_eq!(
            fs::read_to_string(path.with_file_name("config.toml.bak")).unwrap(),
            "v1"
        );
    }

    #[test]
    fn create_dir_all_synced_handles_deep_and_existing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a/b/c");
        create_dir_all_synced(&deep).unwrap();
        assert!(deep.is_dir());
        // Idempotent on an existing path.
        create_dir_all_synced(&deep).unwrap();
        create_dir_all_synced(dir.path()).unwrap();
    }

    #[test]
    fn remove_file_sync_reports_whether_the_file_existed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("marker");
        fs::write(&path, "").unwrap();
        assert!(remove_file_sync(&path).unwrap());
        assert!(!path.exists());
        assert!(!remove_file_sync(&path).unwrap());
    }

    #[test]
    fn concurrent_writers_never_produce_torn_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hot.txt");
        atomic_write_sync(&path, "seed").unwrap();
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            for word in ["alpha", "bravo"] {
                let path = path.clone();
                let stop = &stop;
                scope.spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        atomic_write_sync(&path, word).unwrap();
                    }
                });
            }
            for _ in 0..500 {
                let seen = fs::read_to_string(&path).unwrap();
                assert!(
                    ["seed", "alpha", "bravo"].contains(&seen.as_str()),
                    "torn read: {seen:?}"
                );
            }
            stop.store(true, Ordering::Relaxed);
        });
    }
}
