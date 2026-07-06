use crate::config::{Config, Layout};
use crate::db;
use crate::lock::WorkspaceLock;
use crate::map::{load_span_map, sha256_hex};
use crate::sanitize::{collect_protected_identifiers, find_leaks, sanitize_content, term_table};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct VerifyReport {
    pub checked: usize,
    pub failures: Vec<String>,
}

impl VerifyReport {
    pub fn is_ok(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Typed error for a failed verification, so the CLI can print every failure
/// and exit with the dedicated "workspace broken" code.
#[derive(Debug)]
pub struct VerifyFailed {
    pub report: VerifyReport,
}

impl std::fmt::Display for VerifyFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "verify failed with {} issue(s)",
            self.report.failures.len()
        )?;
        for failure in &self.report.failures {
            writeln!(f, "  {failure}")?;
        }
        Ok(())
    }
}

impl std::error::Error for VerifyFailed {}

pub fn verify_workspace(root: &Path) -> Result<VerifyReport> {
    let layout = Layout::new(root);
    let config = Config::load_or_default(&layout)?;
    let conn = db::connect(&layout)?;
    db::init_schema(&conn)?;
    let _lock = WorkspaceLock::acquire(&layout)?;
    let mut report = VerifyReport::default();

    let tracked = db::tracked_files(&conn)?;
    let tracked_set: BTreeSet<String> = tracked.iter().cloned().collect();

    // Recompute the repo-wide protected identifier union from the REAL files
    // (the source of truth), independently of what index stored. Missing real
    // files are reported per-file below.
    let mut real_contents: BTreeMap<String, String> = BTreeMap::new();
    let mut protected_union: BTreeSet<String> = BTreeSet::new();
    for rel in &tracked {
        if let Ok(real) = fs::read_to_string(root.join(rel)) {
            protected_union.extend(collect_protected_identifiers(&real));
            real_contents.insert(rel.clone(), real);
        }
    }
    let terms = term_table(&config);

    for rel in &tracked {
        report.checked += 1;
        verify_file(
            root,
            &layout,
            &config,
            rel,
            real_contents.get(rel).map(String::as_str),
            &protected_union,
            &terms,
            &mut report,
        )
        .with_context(|| format!("verify {rel}"))?;
    }

    // Independent mirror sweep: a mirror file nobody tracks is either drift or
    // a plant; both are failures.
    if layout.mirror_dir.exists() {
        for entry in walkdir_files(&layout.mirror_dir)? {
            let rel = entry
                .strip_prefix(&layout.mirror_dir)
                .unwrap_or(&entry)
                .to_path_buf();
            let rel_string = crate::config::normalize_rel_path(&rel);
            if !tracked_set.contains(&rel_string) {
                report
                    .failures
                    .push(format!("{rel_string}: untracked file in mirror"));
            }
        }
    }

    if !report.failures.is_empty() {
        return Err(anyhow::Error::new(VerifyFailed {
            report: report.clone(),
        }));
    }
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn verify_file(
    root: &Path,
    layout: &Layout,
    config: &Config,
    rel: &str,
    real: Option<&str>,
    protected_union: &BTreeSet<String>,
    terms: &[crate::sanitize::Term],
    report: &mut VerifyReport,
) -> Result<()> {
    let _ = root;
    let rel_path = PathBuf::from(rel);
    let mirror_path = layout.mirror_dir.join(&rel_path);
    let map_path = layout.map_path(&rel_path);

    let Some(real) = real else {
        report.failures.push(format!("{rel}: missing real file"));
        return Ok(());
    };
    let mirror = match fs::read_to_string(&mirror_path) {
        Ok(mirror) => mirror,
        Err(err) => {
            report
                .failures
                .push(format!("{rel}: missing mirror file ({err})"));
            return Ok(());
        }
    };
    let span_map = match load_span_map(&map_path) {
        Ok(map) => map,
        Err(err) => {
            report.failures.push(format!("{rel}: invalid map ({err})"));
            return Ok(());
        }
    };

    let rendered = sanitize_content(&rel_path, real, config, protected_union)?;
    if rendered.sanitized != mirror {
        report
            .failures
            .push(format!("{rel}: sanitize(real) differs from mirror"));
    }
    if sha256_hex(real.as_bytes()) != span_map.original_hash {
        report
            .failures
            .push(format!("{rel}: map original hash differs from real file"));
    }
    if sha256_hex(mirror.as_bytes()) != span_map.sanitized_hash {
        report.failures.push(format!(
            "{rel}: map sanitized hash differs from mirror file"
        ));
    }
    if rendered.span_map.replacements.len() != span_map.replacements.len() {
        report.failures.push(format!(
            "{rel}: replacement count differs from fresh sanitize"
        ));
    }

    // Independent leak backstop: no dictionary/denylist/registry term may
    // survive into the mirror except inside a protected identifier.
    for leak in find_leaks(&mirror, terms, protected_union) {
        report.failures.push(format!(
            "{rel}: leak of term {:?} in mirror at byte {} (in {:?})",
            leak.term, leak.offset, leak.enclosing
        ));
    }
    // Replacement outputs themselves must be clean, unconditionally.
    for replacement in &span_map.replacements {
        for leak in find_leaks(&replacement.sanitized_text, terms, &BTreeSet::new()) {
            report.failures.push(format!(
                "{rel}: leak of term {:?} in span-map replacement output {:?}",
                leak.term, replacement.sanitized_text
            ));
        }
    }

    Ok(())
}

fn walkdir_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in
            fs::read_dir(&current).with_context(|| format!("read {}", current.display()))?
        {
            let entry = entry.context("read mirror dir entry")?;
            let path = entry.path();
            let file_type = entry.file_type().context("stat mirror entry")?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}
