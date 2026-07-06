use crate::config::{Config, Layout, normalize_rel_path, rel_path};
use crate::db;
use crate::map::{SpanMap, load_span_map};
use crate::sanitize::sanitize_content;
use anyhow::{Context, Result};
use ignore::{DirEntry, WalkBuilder};
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct IndexReport {
    pub indexed: usize,
    pub skipped: usize,
    pub removed: usize,
    pub unchanged: usize,
}

pub fn init_workspace(root: &Path) -> Result<Layout> {
    let layout = Layout::new(root);
    layout.ensure_dirs()?;
    let config = Config::default();
    config.write_if_missing(&layout)?;
    ensure_gitignore_entry(root, ".code-sanity/")?;
    let conn = db::connect(&layout)?;
    db::init_schema(&conn)?;
    Ok(layout)
}

pub fn index_workspace(root: &Path) -> Result<IndexReport> {
    let layout = init_workspace(root)?;
    let config = Config::load_or_default(&layout)?;
    let mut conn = db::connect(&layout)?;
    db::init_schema(&conn)?;

    let mut report = IndexReport::default();
    let mut seen = BTreeSet::new();
    for entry in walk_repo(root, &config)? {
        let entry = entry?;
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }
        let rel = rel_path(root, entry.path())?;
        if should_skip_file(&rel, entry.path(), &config)? {
            report.skipped += 1;
            continue;
        }
        let rel_string = normalize_rel_path(&rel);
        seen.insert(rel_string);
        match index_file_with_config(root, &layout, &config, &mut conn, &rel)? {
            FileIndexStatus::Updated => report.indexed += 1,
            FileIndexStatus::Unchanged => report.unchanged += 1,
        }
    }

    for tracked in db::tracked_files(&conn)? {
        if !seen.contains(&tracked) {
            db::remove_file(&conn, &tracked)?;
            remove_if_exists(layout.mirror_dir.join(&tracked))?;
            remove_if_exists(layout.map_path(Path::new(&tracked)))?;
            report.removed += 1;
        }
    }

    Ok(report)
}

pub fn index_single_file(root: &Path, rel: &Path) -> Result<SpanMap> {
    let layout = init_workspace(root)?;
    let config = Config::load_or_default(&layout)?;
    let mut conn = db::connect(&layout)?;
    db::init_schema(&conn)?;
    index_file_with_config(root, &layout, &config, &mut conn, rel)?;
    load_span_map(&layout.map_path(rel))
}

enum FileIndexStatus {
    Updated,
    Unchanged,
}

fn index_file_with_config(
    root: &Path,
    layout: &Layout,
    config: &Config,
    conn: &mut rusqlite::Connection,
    rel: &Path,
) -> Result<FileIndexStatus> {
    let source_path = root.join(rel);
    let content = fs::read_to_string(&source_path)
        .with_context(|| format!("read source {}", source_path.display()))?;
    let mut rendered = sanitize_content(rel, &content, config)
        .with_context(|| format!("sanitize {}", rel.display()))?;

    let mirror_path = layout.mirror_dir.join(rel);
    let map_path = layout.map_path(rel);
    let old_mirror = fs::read_to_string(&mirror_path).ok();
    let old_map_raw = fs::read_to_string(&map_path).ok();
    if let Some(old_map) = old_map_raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<SpanMap>(raw).ok())
        && old_map.original_hash == rendered.span_map.original_hash
        && old_map.sanitized_hash == rendered.span_map.sanitized_hash
        && old_map.replacements == rendered.span_map.replacements
        && old_map.spans == rendered.span_map.spans
    {
        rendered.span_map.updated_at = old_map.updated_at;
    }
    let next_map = serde_json::to_string_pretty(&rendered.span_map).context("serialize map")?;
    let unchanged = old_mirror.as_deref() == Some(rendered.sanitized.as_str())
        && old_map_raw.as_deref() == Some(next_map.as_str());

    write_if_changed(&mirror_path, &rendered.sanitized)?;
    write_if_changed(&map_path, &next_map)?;
    db::upsert_span_map(conn, &rendered.span_map)?;

    Ok(if unchanged {
        FileIndexStatus::Unchanged
    } else {
        FileIndexStatus::Updated
    })
}

fn walk_repo(
    root: &Path,
    config: &Config,
) -> Result<Vec<std::result::Result<DirEntry, ignore::Error>>> {
    let extra_dirs = config
        .ignore
        .extra_dirs
        .iter()
        .chain(config.ignore.generated_dirs.iter())
        .cloned()
        .collect::<BTreeSet<_>>();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .parents(false)
        .require_git(false)
        .filter_entry(move |entry| {
            let name = entry.file_name().to_string_lossy();
            !extra_dirs.contains(name.as_ref())
        })
        .build()
        .collect::<Vec<_>>();
    Ok(walker)
}

fn should_skip_file(rel: &Path, path: &Path, config: &Config) -> Result<bool> {
    let Some(file_name) = rel.file_name().and_then(|name| name.to_str()) else {
        return Ok(true);
    };
    if config.ignore.lockfiles.iter().any(|lock| lock == file_name) {
        return Ok(true);
    }
    if fs::metadata(path)
        .with_context(|| format!("metadata {}", path.display()))?
        .len()
        > config.ignore.max_file_bytes
    {
        return Ok(true);
    }
    is_binary(path)
}

fn is_binary(path: &Path) -> Result<bool> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut buf = [0u8; 8192];
    let read = file
        .read(&mut buf)
        .with_context(|| format!("read {}", path.display()))?;
    if buf[..read].contains(&0) {
        return Ok(true);
    }
    Ok(std::str::from_utf8(&buf[..read]).is_err())
}

fn write_if_changed(path: &Path, content: &str) -> Result<()> {
    if fs::read_to_string(path).ok().as_deref() == Some(content) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(path, content).with_context(|| format!("write {}", path.display()))
}

fn remove_if_exists(path: PathBuf) -> Result<()> {
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

fn ensure_gitignore_entry(root: &Path, entry: &str) -> Result<()> {
    let path = root.join(".gitignore");
    let current = fs::read_to_string(&path).unwrap_or_default();
    if current
        .lines()
        .any(|line| line.trim() == entry.trim_end_matches('/'))
        || current.lines().any(|line| line.trim() == entry)
    {
        return Ok(());
    }

    let mut next = current;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(entry);
    next.push('\n');
    fs::write(&path, next).with_context(|| format!("write {}", path.display()))
}
