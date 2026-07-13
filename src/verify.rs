use crate::config::{Config, Layout};
use crate::db;
use crate::lock::WorkspaceLock;
use crate::map::{load_span_map, sha256_hex};
use crate::path_projection::PathProjection;
use crate::sanitize::{collect_protected_identifiers, find_leaks, sanitize_content, term_table};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, serde::Serialize)]
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
    layout.require_initialized()?;
    // Verify only reads; a shared lock keeps writers out for a consistent
    // snapshot while letting other readers proceed.
    let _lock = WorkspaceLock::acquire_shared(&layout)?;
    // A lost config is the "workspace broken" contract (exit 3), reported as
    // an actionable finding: a raw error would exit 1 without one, while
    // proceeding on stub-salt defaults would drown the report in per-file
    // mismatch noise.
    if !layout.config_path.exists() && layout.has_initialized_state() {
        return Err(anyhow::Error::new(VerifyFailed {
            report: VerifyReport {
                checked: 0,
                failures: vec![format!(
                    "config: {}",
                    crate::config::missing_config_error(&layout)
                )],
            },
        }));
    }
    // Lenient load: a policy-violating config is a FINDING verify reports,
    // not a reason verify cannot run.
    let config = Config::load_or_default_lenient(&layout)?;
    let conn = db::connect(&layout)?;
    db::check_schema(&conn)?;
    let mut report = VerifyReport::default();
    for violation in crate::sanitize::sanitizer_policy_violations(&config) {
        report.failures.push(format!("config: {violation}"));
    }

    let tracked = db::tracked_files(&conn)?;
    let tracked_set: BTreeSet<String> = tracked.iter().cloned().collect();
    let path_projection = match PathProjection::build(&config, tracked.iter()) {
        Ok(projection) => Some(projection),
        Err(err) => {
            report.failures.push(format!("path projection: {err:#}"));
            None
        }
    };
    let projected_set: BTreeSet<String> = tracked
        .iter()
        .filter_map(|rel| {
            path_projection
                .as_ref()?
                .projected_string_for_real(rel)
                .ok()
        })
        .collect();

    // Recompute the repo-wide protected identifier union from the REAL files
    // (the source of truth), independently of what index stored. Missing real
    // files are reported per-file below.
    let mut real_contents: BTreeMap<String, String> = BTreeMap::new();
    let mut protected_union: BTreeSet<String> = BTreeSet::new();
    let mut declared_in: BTreeMap<String, String> = BTreeMap::new();
    for rel in &tracked {
        if let Ok(real) = fs::read_to_string(root.join(rel)) {
            for name in collect_protected_identifiers(Path::new(rel), &real) {
                declared_in
                    .entry(name.clone())
                    .or_insert_with(|| rel.clone());
                protected_union.insert(name);
            }
            real_contents.insert(rel.clone(), real);
        }
    }
    let terms = term_table(&config);

    // A denylisted term kept alive by a protected identifier: index refuses to
    // render it, so verify must report it rather than silently sanction the
    // residue (find_leaks skips protected runs by construction).
    for conflict in crate::sanitize::denylist_protected_conflicts(&terms, &protected_union) {
        let origin = declared_in
            .get(&conflict.protected_name)
            .map(|rel| {
                format!(
                    " (declared in {})",
                    display_path(&config, path_projection.as_ref(), rel)
                )
            })
            .unwrap_or_default();
        report.failures.push(format!(
            "denylist term {:?} is protected as public identifier {:?}{origin}; it would \
             survive verbatim in the mirror — allowlist it or rename the public symbol",
            conflict.term, conflict.protected_name,
        ));
    }

    // Injectivity against content: an alias occurring naturally in a REAL
    // file makes the mirror ambiguous (the word survives rendering verbatim,
    // indistinguishable from the alias). Real contents are already in memory.
    for (rel, real) in &real_contents {
        let display = display_path(&config, path_projection.as_ref(), rel);
        for collision in crate::sanitize::alias_collisions(real, &terms) {
            report.failures.push(format!(
                "{display}: alias {:?} (for term {:?}) occurs in real content as {:?} at byte {}; \
                 mirror is ambiguous — change the alias in .code-sanity/config.toml",
                collision.alias, collision.term, collision.word, collision.offset
            ));
        }
    }

    for rel in &tracked {
        report.checked += 1;
        let projected = path_projection
            .as_ref()
            .and_then(|projection| projection.projected_for_real(Path::new(rel)).ok())
            .unwrap_or_else(|| PathBuf::from(rel));
        verify_file(
            root,
            &layout,
            &conn,
            &config,
            rel,
            &projected,
            real_contents.get(rel).map(String::as_str),
            &protected_union,
            &terms,
            &mut report,
        )
        .with_context(|| {
            format!(
                "verify {}",
                display_path(&config, path_projection.as_ref(), rel)
            )
        })?;
    }
    verify_semantic_index(
        root,
        &conn,
        &config,
        path_projection.as_ref(),
        &tracked_set,
        &mut report,
    )?;

    // Independent mirror sweep: a mirror file nobody tracks is either drift or
    // a plant; both are failures.
    if layout.mirror_dir.exists() {
        for entry in walkdir_files(&layout.mirror_dir)? {
            let rel = entry
                .strip_prefix(&layout.mirror_dir)
                .unwrap_or(&entry)
                .to_path_buf();
            let rel_string = crate::config::normalize_rel_path(&rel);
            if !projected_set.contains(&rel_string) {
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

fn verify_semantic_index(
    root: &Path,
    conn: &rusqlite::Connection,
    config: &Config,
    projection: Option<&PathProjection>,
    tracked: &BTreeSet<String>,
    report: &mut VerifyReport,
) -> Result<()> {
    let mut semantic_paths = BTreeSet::new();
    let mut statement = conn
        .prepare(
            "select rel_path, content_hash, parse_errors, capabilities_json from semantic_documents order by rel_path",
        )
        .context("prepare semantic verification query")?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as usize,
                row.get::<_, String>(3)?,
            ))
        })
        .context("query semantic documents for verification")?;
    for row in rows {
        let (rel, indexed_hash, indexed_errors, capabilities_json) =
            row.context("read semantic document")?;
        semantic_paths.insert(rel.clone());
        let display = display_path(config, projection, &rel);
        match serde_json::from_str::<crate::semantic::BackendCapabilities>(&capabilities_json) {
            Ok(capabilities)
                if capabilities.resolver_version < crate::semantic::SEMANTIC_RESOLVER_VERSION =>
            {
                report.failures.push(format!(
                    "{display}: semantic resolver version is {} (current={}); run code-sanity index",
                    capabilities.resolver_version,
                    crate::semantic::SEMANTIC_RESOLVER_VERSION
                ));
            }
            Err(err) => report
                .failures
                .push(format!("{display}: invalid semantic capabilities ({err})")),
            _ => {}
        }
        if !tracked.contains(&rel) {
            report
                .failures
                .push(format!("{display}: untracked semantic document"));
            continue;
        }
        let source = match fs::read_to_string(root.join(&rel)) {
            Ok(source) => source,
            Err(err) => {
                report
                    .failures
                    .push(format!("{display}: semantic source unreadable ({err})"));
                continue;
            }
        };
        if sha256_hex(source.as_bytes()) != indexed_hash {
            report.failures.push(format!(
                "{display}: semantic document hash differs from real file"
            ));
            continue;
        }
        let parsed = crate::semantic::parse_document(Path::new(&rel), &source)?;
        if parsed.parse_errors != indexed_errors {
            report.failures.push(format!(
                "{display}: semantic parse error count differs (db={indexed_errors}, current={})",
                parsed.parse_errors
            ));
        }
        for (table, expected) in [
            ("semantic_nodes", parsed.nodes.len()),
            ("semantic_occurrences", parsed.occurrences.len()),
        ] {
            let sql = format!("select count(*) from {table} where rel_path = ?1");
            let actual =
                conn.query_row(&sql, [rel.as_str()], |row| row.get::<_, i64>(0))
                    .with_context(|| format!("count {table} for {rel}"))? as usize;
            if actual != expected {
                report.failures.push(format!(
                    "{display}: {table} count differs (db={actual}, current={expected})"
                ));
            }
        }
        // Resolver upgrades preserve one symbol anchor per historical
        // prototype/definition declaration so accepted aliases and queued
        // review IDs remain valid. Fresh parsing may coalesce those anchors
        // into one semantic symbol, therefore verify symbol *coverage* rather
        // than requiring the grouped count to be byte-for-byte identical.
        let parsed_declarations = parsed
            .occurrences
            .iter()
            .filter(|occurrence| occurrence.role == crate::semantic::OccurrenceRole::Declaration)
            .map(|occurrence| {
                (
                    occurrence.range.start_byte,
                    occurrence.range.end_byte,
                    occurrence.name.clone(),
                )
            })
            .collect::<BTreeSet<_>>();
        let parsed_canonical = parsed
            .symbols
            .iter()
            .map(|symbol| {
                (
                    symbol.range.start_byte,
                    symbol.range.end_byte,
                    symbol.name.clone(),
                )
            })
            .collect::<BTreeSet<_>>();
        let mut symbol_statement = conn
            .prepare(
                r#"
                select n.start_byte, n.end_byte, s.name
                from semantic_symbols s
                join semantic_nodes n on n.node_id = s.node_id
                where s.rel_path = ?1
                "#,
            )
            .with_context(|| format!("prepare semantic symbol coverage for {rel}"))?;
        let stored_symbols = symbol_statement
            .query_map([rel.as_str()], |row| {
                Ok((
                    row.get::<_, i64>(0)? as usize,
                    row.get::<_, i64>(1)? as usize,
                    row.get::<_, String>(2)?,
                ))
            })
            .with_context(|| format!("query semantic symbol coverage for {rel}"))?
            .collect::<rusqlite::Result<BTreeSet<_>>>()
            .with_context(|| format!("collect semantic symbol coverage for {rel}"))?;
        if !parsed_canonical.is_subset(&stored_symbols)
            || !stored_symbols.is_subset(&parsed_declarations)
        {
            report.failures.push(format!(
                "{display}: semantic_symbols declaration coverage differs (db={}, current={}, declarations={})",
                stored_symbols.len(),
                parsed_canonical.len(),
                parsed_declarations.len()
            ));
        }
    }
    for missing in tracked.difference(&semantic_paths) {
        let display = display_path(config, projection, missing);
        report.failures.push(format!(
            "{display}: missing semantic document; run code-sanity index"
        ));
    }

    let orphan_aliases: i64 = conn
        .query_row(
            r#"
            select count(*) from semantic_aliases a
            left join semantic_symbols s on s.symbol_id = a.symbol_id
            where s.symbol_id is null or s.origin != 'owned'
            "#,
            [],
            |row| row.get(0),
        )
        .context("count orphan semantic aliases")?;
    if orphan_aliases != 0 {
        report.failures.push(format!(
            "semantic index has {orphan_aliases} orphan or non-owned alias(es)"
        ));
    }
    let orphan_proposals: i64 = conn
        .query_row(
            r#"
            select count(*) from semantic_proposals p
            left join semantic_symbols s on s.symbol_id = p.symbol_id
            left join semantic_occurrences o on o.occurrence_id = p.occurrence_id
            where p.status = 'pending'
              and (s.symbol_id is null or o.occurrence_id is null or o.symbol_id != p.symbol_id)
            "#,
            [],
            |row| row.get(0),
        )
        .context("count orphan semantic proposals")?;
    if orphan_proposals != 0 {
        report.failures.push(format!(
            "semantic index has {orphan_proposals} proposal(s) with missing target IDs"
        ));
    }
    let stale_aliases: i64 = conn
        .query_row(
            "select count(*) from semantic_aliases where status = 'stale'",
            [],
            |row| row.get(0),
        )
        .context("count stale semantic aliases")?;
    if stale_aliases != 0 {
        report.failures.push(format!(
            "semantic index has {stale_aliases} stale alias(es); restore the language server and run code-sanity index"
        ));
    }

    let aliases = crate::semantic_store::accepted_alias_bindings(conn)?;
    let mut lexical_owners = BTreeMap::<String, BTreeSet<String>>::new();
    {
        let mut statement = conn
            .prepare(
                r#"
                select distinct original_text, sanitized_text
                from replacements
                where policy_source != 'semantic-alias'
                "#,
            )
            .context("prepare lexical/semantic alias verification")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("query lexical/semantic alias verification")?;
        for row in rows {
            let (original, alias) = row.context("read lexical alias owner")?;
            lexical_owners
                .entry(crate::sanitize::normalize_term(&alias))
                .or_default()
                .insert(crate::sanitize::normalize_term(&original));
        }
    }
    let mut owners = BTreeMap::<String, String>::new();
    for alias in &aliases {
        let normalized = crate::sanitize::normalize_term(&alias.alias);
        if lexical_owners.get(&normalized).is_some_and(|originals| {
            !originals.contains(&crate::sanitize::normalize_term(&alias.original))
        }) {
            report.failures.push(format!(
                "semantic alias {:?} conflicts with a lexical alias owned by another term",
                alias.alias
            ));
        }
        if let Some(existing) = owners.insert(normalized, alias.original.clone()) {
            if crate::sanitize::normalize_term(&existing)
                != crate::sanitize::normalize_term(&alias.original)
            {
                report.failures.push(format!(
                    "semantic alias {:?} is non-injective for {:?} and {:?}",
                    alias.alias, existing, alias.original
                ));
            }
        }
        if alias.source == "proposal-v2"
            && !crate::semantic_store::symbol_projection_is_complete(conn, &alias.symbol_id)?
        {
            report.failures.push(format!(
                "semantic alias {:?} has an incomplete reference projection",
                alias.alias
            ));
        }
        let collision: i64 = conn
            .query_row(
                r#"
                select count(*) from semantic_occurrences
                where role in ('unresolved', 'external') and lower(name) = lower(?1)
                "#,
                [&alias.alias],
                |row| row.get(0),
            )
            .context("check unresolved semantic alias spelling")?;
        if collision != 0 {
            report.failures.push(format!(
                "semantic alias {:?} occurs as {collision} unresolved/external real identifier(s)",
                alias.alias
            ));
        }
    }

    for rel in &semantic_paths {
        let Some(document) = crate::semantic_store::load_document(conn, rel)? else {
            continue;
        };
        if !document.capabilities.parse {
            continue;
        }
        let projected = match crate::semantic_store::project_document(conn, root, rel) {
            Ok(projected) => projected,
            Err(err) => {
                // Verification must remain an exhaustive diagnostic surface.
                // A corrupted/stale mirror is already a finding; do not let
                // the semantic projection's fail-closed read path abort the
                // report before independent leak checks are returned.
                report.failures.push(format!(
                    "{}: semantic projection unavailable ({err:#})",
                    display_path(config, projection, rel)
                ));
                continue;
            }
        };
        let parsed = crate::semantic::parse_document(Path::new(rel), &projected.content)?;
        if parsed.parse_errors > document.parse_errors {
            report.failures.push(format!(
                "{}: semantic projection introduces {} parse error(s)",
                display_path(config, projection, rel),
                parsed.parse_errors - document.parse_errors
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn verify_file(
    root: &Path,
    layout: &Layout,
    conn: &rusqlite::Connection,
    config: &Config,
    rel: &str,
    projected_rel: &Path,
    real: Option<&str>,
    protected_union: &BTreeSet<String>,
    terms: &[crate::sanitize::Term],
    report: &mut VerifyReport,
) -> Result<()> {
    let _ = root;
    let rel_path = PathBuf::from(rel);
    let projected_string = crate::config::normalize_rel_path(projected_rel);
    let mirror_path = layout.mirror_dir.join(projected_rel);
    let map_path = layout.map_path(&rel_path);

    let Some(real) = real else {
        report.failures.push(format!(
            "{projected_string}: real file missing or unreadable"
        ));
        return Ok(());
    };
    if let Err(err) =
        crate::search::ensure_existing_path_inside(&mirror_path, &layout.mirror_dir, projected_rel)
    {
        report.failures.push(format!(
            "{projected_string}: mirror path is unsafe or missing ({err})"
        ));
        return Ok(());
    }
    let mirror = match fs::read_to_string(&mirror_path) {
        Ok(mirror) => mirror,
        Err(err) => {
            report
                .failures
                .push(format!("{projected_string}: missing mirror file ({err})"));
            return Ok(());
        }
    };
    let span_map = match load_span_map(&map_path) {
        Ok(map) => map,
        Err(err) => {
            report
                .failures
                .push(format!("{projected_string}: invalid map ({err})"));
            return Ok(());
        }
    };

    let lexical = sanitize_content(&rel_path, real, config, protected_union)?;
    let rendered = crate::semantic_store::merge_semantic_aliases(conn, rel, real, lexical)?;
    if rendered.sanitized != mirror {
        report.failures.push(format!(
            "{projected_string}: sanitize(real) differs from mirror"
        ));
    }
    if span_map.projected_path != projected_string {
        let stored_display = display_path(config, None, &span_map.projected_path);
        report.failures.push(format!(
            "{projected_string}: map projected path differs ({:?})",
            stored_display
        ));
    }
    if sha256_hex(real.as_bytes()) != span_map.original_hash {
        report.failures.push(format!(
            "{projected_string}: map original hash differs from real file"
        ));
    }
    if sha256_hex(mirror.as_bytes()) != span_map.sanitized_hash {
        report.failures.push(format!(
            "{projected_string}: map sanitized hash differs from mirror file"
        ));
    }
    if rendered.span_map.replacements.len() != span_map.replacements.len() {
        report.failures.push(format!(
            "{projected_string}: replacement count differs from fresh sanitize"
        ));
    }

    // Independent leak backstop: no dictionary/denylist/registry term may
    // survive into the mirror except inside a protected identifier.
    for leak in find_leaks(&mirror, terms, protected_union) {
        report.failures.push(format!(
            "{projected_string}: leak of term {:?} in mirror at byte {} (in {:?})",
            leak.term, leak.offset, leak.enclosing
        ));
    }
    // Replacement outputs themselves must be clean, unconditionally.
    for replacement in &span_map.replacements {
        for leak in find_leaks(&replacement.sanitized_text, terms, &BTreeSet::new()) {
            report.failures.push(format!(
                "{projected_string}: leak of term {:?} in span-map replacement output {:?}",
                leak.term, replacement.sanitized_text
            ));
        }
    }

    Ok(())
}

fn display_path(config: &Config, projection: Option<&PathProjection>, rel: &str) -> String {
    projection
        .and_then(|projection| projection.projected_string_for_real(rel).ok())
        .or_else(|| {
            crate::path_projection::project_rel_path(Path::new(rel), config)
                .ok()
                .map(|path| crate::config::normalize_rel_path(&path))
        })
        .unwrap_or_else(|| {
            crate::sanitize::sanitize_unprotected_text(rel, &crate::sanitize::term_table(config))
        })
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
