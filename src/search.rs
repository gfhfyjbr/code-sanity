use crate::config::{Layout, normalize_rel_path, normalize_safe_rel_path};
use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub rel_path: String,
    pub line: usize,
    pub column: usize,
    pub line_text: String,
}

pub fn read_sanitized_file(root: &Path, rel_path: &Path) -> Result<String> {
    let layout = Layout::new(root);
    let rel_path = normalize_safe_rel_path(rel_path, "sanitized mirror")?;
    let path = layout.mirror_dir.join(&rel_path);
    ensure_existing_path_inside(&path, &layout.mirror_dir, &rel_path)?;
    fs::read_to_string(&path).with_context(|| {
        format!(
            "read sanitized file {}; run `code-sanity index` first if missing",
            path.display()
        )
    })
}

pub fn search_mirror(root: &Path, query: &str, glob: Option<&str>) -> Result<Vec<SearchMatch>> {
    if query.is_empty() {
        bail!("search query must not be empty");
    }
    let layout = Layout::new(root);
    let mut matches = Vec::new();
    if !layout.mirror_dir.exists() {
        bail!("sanitized mirror is missing; run `code-sanity index` first");
    }
    let glob = glob.map(ToOwned::to_owned);
    for entry in WalkBuilder::new(&layout.mirror_dir)
        .hidden(false)
        .git_ignore(false)
        .build()
    {
        let entry = entry.context("walk sanitized mirror")?;
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }
        let rel = entry.path().strip_prefix(&layout.mirror_dir)?.to_path_buf();
        if !matches_glob(&rel, glob.as_deref()) {
            continue;
        }
        let content = fs::read_to_string(entry.path())
            .with_context(|| format!("read {}", entry.path().display()))?;
        for (line_idx, line) in content.lines().enumerate() {
            let mut search_at = 0usize;
            while let Some(found) = line[search_at..].find(query) {
                let byte_col = search_at + found;
                matches.push(SearchMatch {
                    rel_path: normalize_rel_path(&rel),
                    line: line_idx + 1,
                    column: line[..byte_col].chars().count() + 1,
                    line_text: line.to_string(),
                });
                search_at = byte_col + query.len();
            }
        }
    }
    Ok(matches)
}

pub(crate) fn normalize_sanitized_rel_path(path: &Path) -> Result<PathBuf> {
    normalize_safe_rel_path(path, "sanitized mirror")
}

pub(crate) fn ensure_existing_path_inside(path: &Path, base: &Path, rel_path: &Path) -> Result<()> {
    let canonical_base = base
        .canonicalize()
        .with_context(|| format!("canonicalize sanitized mirror {}", base.display()))?;
    let canonical_path = path
        .canonicalize()
        .with_context(|| format!("canonicalize sanitized file {}", rel_path.display()))?;
    if !canonical_path.starts_with(&canonical_base) {
        bail!("path escapes sanitized mirror: {}", rel_path.display());
    }
    Ok(())
}

fn matches_glob(rel: &Path, glob: Option<&str>) -> bool {
    let Some(glob) = glob else {
        return true;
    };
    let path = normalize_rel_path(rel);
    if glob == "*" || glob == "**/*" {
        return true;
    }
    if let Some(suffix) = glob.strip_prefix("*.") {
        return path.ends_with(&format!(".{suffix}"));
    }
    if let Some(prefix) = glob.strip_suffix("/**") {
        return path.starts_with(prefix);
    }
    path.contains(glob.trim_matches('*'))
}
