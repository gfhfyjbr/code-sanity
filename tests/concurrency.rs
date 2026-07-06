//! Concurrency: agent edits are never lost under sync storms, and readers
//! never observe torn state. Threads acquire the flock on separate file
//! descriptions, so they contend exactly like separate processes.

use code_sanity::{index_workspace, read_sanitized_file, verify_workspace};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn project_edit_never_loses_the_agents_edit_under_a_sync_storm() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("src/lib.rs"),
        "// dangerous comment\nfn calc() -> usize {\n    0\n}\n",
    )
    .unwrap();
    index_workspace(repo.path()).unwrap();
    let mirror_path = repo.path().join(".code-sanity/mirror/src/lib.rs");

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        // Background sync storm: full and single-path syncs in a tight loop.
        let root = repo.path().to_path_buf();
        let stop_ref = &stop;
        scope.spawn(move || {
            while !stop_ref.load(Ordering::Relaxed) {
                let _ = index_workspace(&root);
                let _ = code_sanity::index::sync_single_file(&root, Path::new("src/lib.rs"));
            }
        });

        for i in 1..=15u32 {
            // The agent edits the mirror in place (like an editor would)...
            let mirror = fs::read_to_string(&mirror_path).unwrap();
            let marker_old = format!("    {}\n", i - 1);
            let marker_new = format!("    {i}\n");
            let edited = mirror.replace(&marker_old, &marker_new);
            assert_ne!(mirror, edited, "iteration {i}: previous edit lost");
            fs::write(&mirror_path, &edited).unwrap();

            // ...and the bridge must project exactly that edit, sync storm or
            // not: the pending-edit protection plus the single project lock
            // guarantee it is never clobbered in between.
            code_sanity::project_mirror_edit(
                repo.path(),
                Path::new("src/lib.rs"),
                code_sanity::patch::ApplyOptions::default(),
            )
            .unwrap_or_else(|err| panic!("iteration {i}: projection failed: {err:#}"));
            let real = fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
            assert!(real.contains(&marker_new), "iteration {i}: edit lost");
        }
        stop.store(true, Ordering::Relaxed);
    });
    assert!(verify_workspace(repo.path()).is_ok());
}

#[test]
fn readers_never_observe_torn_mirrors_or_leaks_during_writes() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    let real_a = "// dangerous note\nfn value() -> usize {\n    1\n}\n";
    let real_b = "// dangerous note\nfn value() -> usize {\n    2\n}\n";
    fs::write(repo.path().join("src/x.rs"), real_a).unwrap();
    index_workspace(repo.path()).unwrap();

    // Capture the two full renders readers are allowed to see.
    let render_a = read_sanitized_file(repo.path(), Path::new("src/x.rs")).unwrap();
    fs::write(repo.path().join("src/x.rs"), real_b).unwrap();
    index_workspace(repo.path()).unwrap();
    let render_b = read_sanitized_file(repo.path(), Path::new("src/x.rs")).unwrap();
    assert_ne!(render_a, render_b);

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let root = repo.path().to_path_buf();
        let stop_ref = &stop;
        scope.spawn(move || {
            // Writer: flip the real file back and forth through the indexer.
            let mut flip = false;
            while !stop_ref.load(Ordering::Relaxed) {
                fs::write(root.join("src/x.rs"), if flip { real_a } else { real_b }).unwrap();
                index_workspace(&root).unwrap();
                flip = !flip;
            }
        });

        for _ in 0..60 {
            let seen = read_sanitized_file(repo.path(), Path::new("src/x.rs")).unwrap();
            assert!(
                seen == render_a || seen == render_b,
                "torn mirror read: {seen:?}"
            );
            assert!(
                !seen.to_lowercase().contains("dangerous"),
                "leak in mirror read"
            );
            let hits = code_sanity::search_mirror(repo.path(), "neutral", None).unwrap();
            assert!(!hits.is_empty());
        }
        stop.store(true, Ordering::Relaxed);
    });
    assert!(verify_workspace(repo.path()).is_ok());
}
