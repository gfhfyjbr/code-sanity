use crate::config::{Config, Layout};
use crate::db;
use crate::map::{load_span_map, sha256_hex};
use crate::sanitize::sanitize_content;
use anyhow::{Context, Result, bail};
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

pub fn verify_workspace(root: &Path) -> Result<VerifyReport> {
    let layout = Layout::new(root);
    let config = Config::load_or_default(&layout)?;
    let conn = db::connect(&layout)?;
    db::init_schema(&conn)?;
    let mut report = VerifyReport::default();

    for rel in db::tracked_files(&conn)? {
        report.checked += 1;
        verify_file(root, &layout, &config, &rel, &mut report)
            .with_context(|| format!("verify {rel}"))?;
    }

    if !report.failures.is_empty() {
        bail!("verify failed with {} issue(s)", report.failures.len());
    }
    Ok(report)
}

fn verify_file(
    root: &Path,
    layout: &Layout,
    config: &Config,
    rel: &str,
    report: &mut VerifyReport,
) -> Result<()> {
    let rel_path = PathBuf::from(rel);
    let real_path = root.join(&rel_path);
    let mirror_path = layout.mirror_dir.join(&rel_path);
    let map_path = layout.map_path(&rel_path);

    let real = match fs::read_to_string(&real_path) {
        Ok(real) => real,
        Err(err) => {
            report
                .failures
                .push(format!("{rel}: missing real file ({err})"));
            return Ok(());
        }
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

    let rendered = sanitize_content(&rel_path, &real, config)?;
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

    Ok(())
}
