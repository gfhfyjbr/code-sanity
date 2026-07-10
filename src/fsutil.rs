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
//! the drive's own write cache (`F_FULLFSYNC` would, at ~10-100x the cost),
//! so a true power loss may reorder writes on the medium. The journal
//! `applying` entry is therefore always written through
//! [`atomic_write_sync_barrier`] (one `F_FULLFSYNC` per apply): a torn real
//! file without its recovery record is the one state `recover` cannot fix.
//! `durability.full_fsync` extends the full flush to every durable-tier write.
//! Process crash / SIGKILL is unaffected either way — the page cache survives.

use anyhow::{Context, Result, anyhow};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static WRITE_NONCE: AtomicU64 = AtomicU64::new(0);

/// Whether durable-tier fsyncs should flush the drive cache too (macOS
/// `F_FULLFSYNC`). Process-wide because the write primitives are free
/// functions called far from any config; armed at config load.
static FULL_FSYNC: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_full_fsync(enabled: bool) {
    FULL_FSYNC.store(enabled, Ordering::Relaxed);
}

/// Durable-tier fsync of a file or directory handle. With `barrier` (or the
/// process-wide `durability.full_fsync` switch) on macOS, flush the drive's
/// write cache via `F_FULLFSYNC`; some filesystems reject the fcntl, in which
/// case plain `fsync` is the best available and we fall back to it.
fn sync_handle(handle: &fs::File, barrier: bool) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        if (barrier || FULL_FSYNC.load(Ordering::Relaxed))
            && unsafe { libc::fcntl(handle.as_raw_fd(), libc::F_FULLFSYNC) } == 0
        {
            return Ok(());
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = barrier;
    handle.sync_all()
}

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

fn atomic_write_impl(path: &Path, content: &str, durable: bool, barrier: bool) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    if durable {
        // Syncing only the immediate parent is not enough for a fresh subtree:
        // if the directories themselves were just created, their entries in
        // the grandparent are volatile and power loss drops the whole subtree
        // — journal entry included.
        create_dir_all_synced_impl(parent, barrier)?;
    } else {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    // rename replaces the target inode, which would silently reset its
    // permission bits to the temp file's fresh default (0644 minus umask) —
    // stripping the executable bit from a back-projected script. Carry the
    // existing mode over via fchmod on the still-private temp, before the
    // rename, so the target never observably changes mode. When the final
    // component is a symlink the rename replaces the LINK inode, so the link
    // target's mode is deliberately not consulted.
    let existing_mode = fs::symlink_metadata(path)
        .ok()
        .filter(fs::Metadata::is_file)
        .map(|meta| {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o7777
        });
    let tmp = temp_path_for(path, parent);
    let result = (|| -> Result<()> {
        let mut file =
            fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        file.write_all(content.as_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
        if let Some(mode) = existing_mode {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(mode))
                .with_context(|| format!("chmod {}", tmp.display()))?;
        }
        if durable {
            sync_handle(&file, barrier).with_context(|| format!("fsync {}", tmp.display()))?;
        }
        drop(file);
        fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
        if durable {
            let dir =
                fs::File::open(parent).with_context(|| format!("open {}", parent.display()))?;
            sync_handle(&dir, barrier).with_context(|| format!("fsync {}", parent.display()))?;
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
    atomic_write_impl(path, content, true, false)
}

/// [`atomic_write_sync`] that additionally flushes the drive's write cache on
/// macOS (`F_FULLFSYNC`), regardless of `durability.full_fsync`. For the one
/// write whose loss-after-reorder is unrecoverable: the journal `applying`
/// entry, which must be on the medium before any real file changes.
pub fn atomic_write_sync_barrier(path: &Path, content: &str) -> Result<()> {
    atomic_write_impl(path, content, true, true)
}

/// Atomically replace `path` with `content` (temp + rename, no fsync). For
/// derived state that `sync`/`verify` can rebuild after a power loss.
pub fn atomic_write(path: &Path, content: &str) -> Result<()> {
    atomic_write_impl(path, content, false, false)
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

/// Guard for REAL repo file writes/removals: lexical rel validation cannot see
/// a directory component that is an on-disk symlink pointing outside the repo
/// (`src -> /etc` turns a create for `src/evil` into a write to `/etc/evil`).
/// Resolve the deepest EXISTING ancestor of the target's parent through
/// symlinks and require it to stay inside `root`; missing components below it
/// are created fresh by the write itself and cannot be links. The final
/// component needs no resolution: `rename` replaces a symlink inode instead of
/// following it, and `remove_file` unlinks the link itself. Returns the joined
/// (unresolved) path for the caller to write to.
pub fn ensure_real_path_containment(root: &Path, rel: &Path) -> Result<PathBuf> {
    let real_path = root.join(rel);
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("canonicalize repo root {}", root.display()))?;
    let mut anchor = real_path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", real_path.display()))?;
    // symlink_metadata (not exists()): a dangling symlink component must be
    // treated as present so canonicalize fails loudly instead of the walk
    // skipping past it.
    while fs::symlink_metadata(anchor).is_err() {
        anchor = match anchor.parent() {
            Some(parent) => parent,
            None => break,
        };
    }
    let canonical_anchor = anchor
        .canonicalize()
        .with_context(|| format!("canonicalize {}", anchor.display()))?;
    if !canonical_anchor.starts_with(&canonical_root) {
        anyhow::bail!(
            "refusing to write {}: {} resolves to {} outside the repo root \
             (a symlinked directory inside the repo points elsewhere)",
            rel.display(),
            anchor.display(),
            canonical_anchor.display()
        );
    }
    Ok(real_path)
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
    create_dir_all_synced_impl(dir, false)
}

fn create_dir_all_synced_impl(dir: &Path, barrier: bool) -> Result<()> {
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
        let handle =
            fs::File::open(created).with_context(|| format!("open {}", created.display()))?;
        sync_handle(&handle, barrier).with_context(|| format!("fsync {}", created.display()))?;
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
        let dir = fs::File::open(parent).with_context(|| format!("open {}", parent.display()))?;
        sync_handle(&dir, false).with_context(|| format!("fsync {}", parent.display()))?;
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
    fn atomic_replace_preserves_target_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool.sh");
        atomic_write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        for write in [atomic_write, atomic_write_sync] {
            write(&path, "#!/bin/sh\necho ok\n").unwrap();
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode, 0o755, "replace must keep the executable bit");
        }
    }

    #[test]
    fn fresh_create_has_no_exec_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.txt");
        atomic_write_sync(&path, "data").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        // Umask-proof: whatever the default is, it must not be executable.
        assert_eq!(mode & 0o111, 0);
    }

    #[test]
    fn barrier_and_full_fsync_writes_roundtrip() {
        // Behavioral power-loss durability is untestable in-process; this
        // pins that the F_FULLFSYNC paths (fresh subtree included) succeed
        // and fall back cleanly where the fcntl is unsupported.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal/fresh/entry.json");
        atomic_write_sync_barrier(&path, "{}").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}");
        set_full_fsync(true);
        let flagged = dir.path().join("flagged/real.rs");
        let result = atomic_write_sync(&flagged, "fn x() {}");
        set_full_fsync(false);
        result.unwrap();
        assert_eq!(fs::read_to_string(&flagged).unwrap(), "fn x() {}");
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
