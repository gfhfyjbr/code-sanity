use code_sanity::{index_workspace, verify_workspace};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

fn write_repo_files(root: &Path, count: usize) {
    fs::create_dir_all(root.join("src")).unwrap();
    for i in 0..count {
        fs::write(
            root.join(format!("src/f{i:04}.rs")),
            format!("fn value_{i}() -> usize {{\n    {i}\n}}\n"),
        )
        .unwrap();
    }
}

#[test]
fn incremental_index_is_fast_and_reindexes_exactly_one_changed_file() {
    let repo = tempfile::tempdir().unwrap();
    let count = 5000;
    write_repo_files(repo.path(), count);

    // count source files plus the .gitignore that init_workspace creates.
    let first = index_workspace(repo.path()).unwrap();
    assert!(first.indexed >= count);
    let tracked = first.indexed;

    // Unchanged re-run must ride the mtime/size pre-check: no reads, no
    // renders, under a second even in debug builds.
    let started = Instant::now();
    let repeat = index_workspace(repo.path()).unwrap();
    let elapsed = started.elapsed();
    assert_eq!(repeat.indexed, 0);
    assert_eq!(repeat.unchanged, tracked);
    assert!(
        elapsed < Duration::from_secs(1),
        "unchanged re-index took {elapsed:?}"
    );

    // Editing one file re-renders exactly that file.
    let target = repo.path().join("src/f0042.rs");
    let mut content = fs::read_to_string(&target).unwrap();
    content.push_str("// touched\n");
    fs::write(&target, content).unwrap();
    let after_edit = index_workspace(repo.path()).unwrap();
    assert_eq!(after_edit.indexed, 1, "expected exactly one reindexed file");
    assert_eq!(after_edit.unchanged, tracked - 1);

    // A deleted file takes its mirror, map, and db rows with it.
    fs::remove_file(repo.path().join("src/f0007.rs")).unwrap();
    let after_delete = index_workspace(repo.path()).unwrap();
    assert_eq!(after_delete.removed, 1);
    assert!(
        !repo
            .path()
            .join(".code-sanity/mirror/src/f0007.rs")
            .exists()
    );
    assert!(
        !repo
            .path()
            .join(".code-sanity/maps/src/f0007.rs.map.json")
            .exists()
    );
}

#[test]
fn logic_fingerprint_change_reindexes_everything() {
    let repo = tempfile::tempdir().unwrap();
    write_repo_files(repo.path(), 20);
    index_workspace(repo.path()).unwrap();

    // Changing the dictionary invalidates the logic fingerprint for all files.
    let layout = code_sanity::config::Layout::new(repo.path());
    let mut config = code_sanity::config::Config::load_or_default(&layout).unwrap();
    config
        .sanitizer
        .dictionary
        .insert("value".to_string(), "item".to_string());
    config.save(&layout).unwrap();

    let report = index_workspace(repo.path()).unwrap();
    assert_eq!(report.indexed, 20);
    assert!(verify_workspace(repo.path()).is_ok());
    let mirror = fs::read_to_string(repo.path().join(".code-sanity/mirror/src/f0000.rs")).unwrap();
    assert!(mirror.contains("item_0"));
}

#[test]
fn parallel_apply_and_sync_stress_keeps_consistency() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("src/counter.rs"),
        "fn value() -> usize {\n    0\n}\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("src/other.rs"),
        "// dangerous comment\nfn dangerous_helper() -> usize {\n    1\n}\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();

    let bin = assert_cmd::cargo::cargo_bin("code-sanity");
    let root = repo.path().to_path_buf();
    // Patches live outside the repo so sync never sees them as source files.
    let patches = tempfile::tempdir().unwrap();

    let apply_root = root.clone();
    let apply_bin = bin.clone();
    let patches_dir = patches.path().to_path_buf();
    let applier = std::thread::spawn(move || {
        for i in 0..50usize {
            let patch = format!(
                "--- a/src/counter.rs\n+++ b/src/counter.rs\n@@ -1,3 +1,3 @@\n fn value() -> usize {{\n-    {i}\n+    {}\n }}\n",
                i + 1
            );
            let patch_path = patches_dir.join(format!("patch-{i}"));
            fs::write(&patch_path, patch).unwrap();
            let out = std::process::Command::new(&apply_bin)
                .args([
                    "--root",
                    apply_root.to_str().unwrap(),
                    "apply-patch",
                    "--patch",
                    patch_path.to_str().unwrap(),
                ])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "apply {i} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            fs::remove_file(&patch_path).ok();
        }
    });

    let sync_root = root.clone();
    let syncer = std::thread::spawn(move || {
        for _ in 0..50usize {
            let out = std::process::Command::new(&bin)
                .args(["--root", sync_root.to_str().unwrap(), "sync"])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "sync failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    });

    applier.join().unwrap();
    syncer.join().unwrap();

    // Zero divergence between real, mirror, and db after the storm.
    let real = fs::read_to_string(repo.path().join("src/counter.rs")).unwrap();
    assert!(real.contains("    50"), "real: {real}");
    let mirror =
        fs::read_to_string(repo.path().join(".code-sanity/mirror/src/counter.rs")).unwrap();
    assert!(mirror.contains("    50"));
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn crashed_mirror_write_self_heals_instead_of_reading_as_pending() {
    let repo = tempfile::tempdir().unwrap();
    fs::write(
        repo.path().join("plain.rs"),
        "fn plain() -> usize {\n    1\n}\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();

    // Simulate a crash between the mirror write and the db commit: real file
    // and mirror already hold the new content, the db row still has the old
    // hashes. This must converge, not read as a pending agent edit forever.
    let next = "fn plain() -> usize {\n    2\n}\n";
    fs::write(repo.path().join("plain.rs"), next).unwrap();
    fs::write(repo.path().join(".code-sanity/mirror/plain.rs"), next).unwrap();

    let report = index_workspace(repo.path()).unwrap();
    assert_eq!(report.pending, 0, "stale db row read as a pending edit");
    assert!(verify_workspace(repo.path()).is_ok());
    let after = index_workspace(repo.path()).unwrap();
    assert_eq!(after.indexed, 0);
}

#[test]
fn sync_force_stashes_the_discarded_pending_edit() {
    let repo = tempfile::tempdir().unwrap();
    fs::write(
        repo.path().join("lib.rs"),
        "// dangerous comment\nfn calc() -> usize {\n    1\n}\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();

    // The agent edited the mirror; the edit is pending (not projected).
    let mirror_path = repo.path().join(".code-sanity/mirror/lib.rs");
    let mirror = fs::read_to_string(&mirror_path).unwrap();
    let edited = mirror.replace("    1\n", "    6\n");
    assert_ne!(mirror, edited);
    fs::write(&mirror_path, &edited).unwrap();

    // A force reset discards the edit but keeps a durable copy.
    let report = code_sanity::index::index_workspace_force(repo.path()).unwrap();
    assert_eq!(report.stashed.len(), 1, "{:?}", report.stashed);
    let stash = fs::read_to_string(&report.stashed[0]).unwrap();
    assert_eq!(stash, edited);
    assert_eq!(fs::read_to_string(&mirror_path).unwrap(), mirror);
    assert!(verify_workspace(repo.path()).is_ok());
}
