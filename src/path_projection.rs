//! Deterministic, reversible agent-facing path projection.
//!
//! Internal state keeps real repository-relative paths as the source of
//! truth. The mirror and every agent-facing surface use projected paths whose
//! directory components and filename stems pass through the same configured
//! term table as source content. A workspace-wide map proves reversibility:
//! two real files or directories may never collapse onto the same projected
//! path, including under ASCII case-insensitive comparison.

use crate::config::{Config, normalize_rel_path, normalize_safe_rel_path};
use crate::sanitize::{Term, path_term_table, sanitize_unprotected_text};
use anyhow::{Result, anyhow, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const PATH_PROJECTION_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathKind {
    Directory,
    File,
}

#[derive(Debug, Clone)]
struct PathEntry {
    real: String,
    projected: String,
    kind: PathKind,
}

/// Bidirectional map for every tracked file and directory prefix.
#[derive(Debug, Clone, Default)]
pub struct PathProjection {
    real_to_projected: BTreeMap<String, PathEntry>,
    projected_to_real: BTreeMap<String, PathEntry>,
}

impl PathProjection {
    pub fn from_connection(config: &Config, conn: &rusqlite::Connection) -> Result<Self> {
        Self::build(config, crate::db::tracked_files(conn)?.iter())
    }

    pub fn build<I, S>(config: &Config, real_files: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let terms = path_term_table(config);
        let mut projection = Self::default();
        let root = PathEntry {
            real: String::new(),
            projected: String::new(),
            kind: PathKind::Directory,
        };
        projection
            .real_to_projected
            .insert(String::new(), root.clone());
        projection.projected_to_real.insert(String::new(), root);
        for raw in real_files {
            let real = normalize_safe_rel_path(Path::new(raw.as_ref()), "real repo path")?;
            let projected = project_rel_path_with_terms(&real, &terms)?;
            let real_components = components(&real);
            let projected_components = components(&projected);
            debug_assert_eq!(real_components.len(), projected_components.len());

            let mut real_prefix = PathBuf::new();
            let mut projected_prefix = PathBuf::new();
            for (index, (real_component, projected_component)) in real_components
                .iter()
                .zip(projected_components.iter())
                .enumerate()
            {
                real_prefix.push(real_component);
                projected_prefix.push(projected_component);
                let kind = if index + 1 == real_components.len() {
                    PathKind::File
                } else {
                    PathKind::Directory
                };
                projection.insert(&real_prefix, &projected_prefix, kind)?;
            }
        }
        Ok(projection)
    }

    fn insert(&mut self, real: &Path, projected: &Path, kind: PathKind) -> Result<()> {
        let real = normalize_rel_path(real);
        let projected = normalize_rel_path(projected);
        let projected_key = portable_key(&projected);
        if let Some(existing) = self.projected_to_real.get(&projected_key) {
            if existing.real != real || existing.kind != kind {
                bail!(
                    "path projection collision: real paths {:?} and {:?} both map to {:?}; \
                     change the conflicting sanitizer alias before indexing",
                    existing.real,
                    real,
                    projected,
                );
            }
        }
        let entry = PathEntry {
            real: real.clone(),
            projected: projected.clone(),
            kind,
        };
        self.real_to_projected.insert(real, entry.clone());
        self.projected_to_real.insert(projected_key, entry);
        Ok(())
    }

    pub fn projected_for_real(&self, real: &Path) -> Result<PathBuf> {
        let real = normalize_safe_rel_path(real, "real repo path")?;
        let key = normalize_rel_path(&real);
        self.real_to_projected
            .get(&key)
            .map(|entry| PathBuf::from(&entry.projected))
            .ok_or_else(|| anyhow!("path is not tracked: {key}"))
    }

    /// Resolve an existing projected path. A real-path fallback keeps host
    /// CLI and old hooks compatible; projected lookup wins when both spellings
    /// happen to exist.
    pub fn real_for_agent(&self, path: &Path) -> Result<PathBuf> {
        let path = normalize_safe_rel_path(path, "sanitized path")?;
        let normalized = normalize_rel_path(&path);
        if let Some(entry) = self.projected_to_real.get(&portable_key(&normalized)) {
            return Ok(PathBuf::from(&entry.real));
        }
        if let Some(entry) = self.real_to_projected.get(&normalized) {
            return Ok(PathBuf::from(&entry.real));
        }
        bail!("path is not tracked in the sanitized projection: {normalized}")
    }

    pub fn projected_string_for_real(&self, real: &str) -> Result<String> {
        Ok(normalize_rel_path(
            &self.projected_for_real(Path::new(real))?,
        ))
    }

    /// Resolve a newly-created projected file. Its nearest tracked directory
    /// is reverse-mapped; every new suffix component must already be neutral,
    /// because no reversible mapping exists for a never-seen sensitive name.
    pub fn real_for_new_agent_path(&self, path: &Path, config: &Config) -> Result<PathBuf> {
        let (path, real) = self.real_candidate_for_new_agent_path(path)?;
        let projected_again = project_rel_path(&real, config)?;
        if projected_again != path {
            bail!(
                "new path {:?} is not already neutral; create files using the sanitized \
                 spelling returned by the path projection",
                normalize_rel_path(&path)
            );
        }
        Ok(real)
    }

    /// Resolve the tracked projected parent of a create target without yet
    /// enforcing that the new suffix is neutral. Callers use this candidate
    /// only for symlink/containment validation, which must take precedence
    /// over policy diagnostics.
    pub fn real_candidate_for_new_agent_path(&self, path: &Path) -> Result<(PathBuf, PathBuf)> {
        let path = normalize_safe_rel_path(path, "sanitized create path")?;
        if let Ok(existing) = self.real_for_agent(&path) {
            return Ok((path, existing));
        }

        let mut suffix = Vec::new();
        let mut cursor = path.as_path();
        let base_real = loop {
            let normalized = normalize_rel_path(cursor);
            if let Some(entry) = self.projected_to_real.get(&portable_key(&normalized)) {
                if entry.kind != PathKind::Directory {
                    bail!("create parent is an existing file: {normalized}");
                }
                break PathBuf::from(&entry.real);
            }
            let name = cursor
                .file_name()
                .ok_or_else(|| anyhow!("create path has no tracked parent"))?;
            suffix.push(name.to_os_string());
            cursor = cursor
                .parent()
                .ok_or_else(|| anyhow!("create path has no tracked projected parent"))?;
        };
        suffix.reverse();
        let mut real = base_real;
        for component in suffix {
            real.push(component);
        }
        Ok((path, real))
    }
}

pub fn project_rel_path(real: &Path, config: &Config) -> Result<PathBuf> {
    project_rel_path_with_terms(real, &path_term_table(config))
}

/// Project either a tracked real/projected spelling or an untracked neutral
/// path for host adapters. The returned value is always agent-facing.
pub fn project_workspace_path(root: &Path, path: &Path) -> Result<PathBuf> {
    let layout = crate::config::Layout::new(root);
    layout.require_initialized()?;
    let _lock = crate::lock::WorkspaceLock::acquire_shared(&layout)?;
    let config = Config::load_or_default(&layout)?;
    let conn = crate::db::connect(&layout)?;
    crate::db::check_schema(&conn)?;
    let projection = PathProjection::from_connection(&config, &conn)?;
    let safe = normalize_safe_rel_path(path, "path projection")?;
    match projection.real_for_agent(&safe) {
        Ok(real) => projection.projected_for_real(&real),
        Err(_) => project_rel_path(&safe, &config),
    }
}

fn project_rel_path_with_terms(real: &Path, terms: &[Term]) -> Result<PathBuf> {
    let real = normalize_safe_rel_path(real, "real repo path")?;
    let parts = components(&real);
    let mut projected = PathBuf::new();
    for (index, part) in parts.iter().enumerate() {
        let is_file = index + 1 == parts.len();
        projected.push(project_component(part, is_file, terms));
    }
    normalize_safe_rel_path(&projected, "sanitized path")
}

fn project_component(component: &str, is_file: bool, terms: &[Term]) -> String {
    if !is_file {
        return sanitize_unprotected_text(component, terms);
    }
    let path = Path::new(component);
    match (
        path.file_stem().and_then(|value| value.to_str()),
        path.extension().and_then(|value| value.to_str()),
    ) {
        (Some(stem), Some(extension)) if !stem.is_empty() => {
            format!("{}.{}", sanitize_unprotected_text(stem, terms), extension)
        }
        _ => sanitize_unprotected_text(component, terms),
    }
}

fn components(path: &Path) -> Vec<String> {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect()
}

fn portable_key(path: &str) -> String {
    path.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        let mut config = Config::default();
        config.sanitizer.dictionary.clear();
        config
            .sanitizer
            .dictionary
            .insert("dangerous".into(), "neutral_x1".into());
        config
            .sanitizer
            .dictionary
            .insert("client".into(), "consumer_y2".into());
        config
    }

    #[test]
    fn projects_directories_and_file_stems_but_preserves_extension() {
        let config = config();
        assert_eq!(
            project_rel_path(Path::new("dangerous/client_dangerous.mm"), &config).unwrap(),
            PathBuf::from("neutral_x1/consumer_y2_neutral_x1.mm")
        );
    }

    #[test]
    fn mapping_is_reversible_for_files_and_directories() {
        let config = config();
        let map = PathProjection::build(&config, ["dangerous/client.mm", "src/other.rs"]).unwrap();
        assert_eq!(
            map.real_for_agent(Path::new("neutral_x1/consumer_y2.mm"))
                .unwrap(),
            PathBuf::from("dangerous/client.mm")
        );
        assert_eq!(
            map.real_for_agent(Path::new("neutral_x1")).unwrap(),
            PathBuf::from("dangerous")
        );
    }

    #[test]
    fn rejects_file_and_directory_projection_collisions() {
        let mut config = config();
        config
            .sanitizer
            .dictionary
            .insert("hazard".into(), "neutral_x1".into());
        let err = PathProjection::build(&config, ["dangerous/a.rs", "hazard/b.rs"]).unwrap_err();
        assert!(err.to_string().contains("path projection collision"));
    }

    #[test]
    fn rejects_case_insensitive_collisions() {
        let config = config();
        let err = PathProjection::build(&config, ["Src/a.rs", "src/b.rs"]).unwrap_err();
        assert!(err.to_string().contains("path projection collision"));
    }
}
