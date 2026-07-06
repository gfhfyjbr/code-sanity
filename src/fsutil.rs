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
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
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
