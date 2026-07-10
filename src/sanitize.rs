//! Deterministic lexical sanitizer.
//!
//! One matching primitive is shared by the sanitizer and the verify leak
//! backstop: every dictionary / denylist / alias-registry term is matched
//! case-insensitively and underscore-insensitively inside word runs
//! (`[A-Za-z0-9_]+`), in comments, string literals, and identifiers alike.
//! `AcmeClient` therefore also catches `ACME_CLIENT` and `acmeClientFactory`.
//!
//! The only sanctioned residues of a term in the mirror are word runs in the
//! repo-wide protected identifier set (public declarations, import-position
//! names, and code dunders, collected from the real files). That set is
//! name-based: one symbol gets one decision across the whole mirror, and
//! `verify` can independently recompute it to tell a sanctioned residue from
//! a leak.
//!
//! Collection is CONTEXT-AWARE: only code positions can protect a name. Prose
//! formats contribute nothing, and inside code, runs within comments or string
//! literals never protect — English prose is full of `from`/`use`/`import`,
//! and a README sentence must not grant a term repo-wide immunity. Because
//! `verify` recomputes this same set, a leak it creates would be self-blessed.
//! A denylisted term that a protected identifier would keep alive is refused
//! outright rather than silently sanctioned.

use crate::config::Config;
use crate::map::{PendingReplacement, RenderedSanitization, render_with_map, sha256_hex};
use anyhow::Result;
use chrono::Utc;
use std::collections::BTreeSet;
use std::path::Path;

/// Bump when matching/rendering semantics change. Part of the logic
/// fingerprint, so an upgrade re-renders every mirror file.
/// v3: alias-collision hard errors + policy validation (the forced re-render
/// doubles as the migration sweep that checks every file for collisions).
/// v4: context-aware protected collection (prose, comments and string
/// literals no longer protect) + dunders dropped from the sanctioned-residue
/// guard + denylist∩protected hard error. The bump is required for
/// correctness, not convention: the guard change alters render output even
/// for files whose protected set is unchanged (`__acme__` in a python string
/// was skipped, now sanitizes), which a union-shrink alone would not
/// invalidate. The forced re-render sweeps legacy prose leaks out.
pub const SANITIZER_BEHAVIOR_VERSION: u32 = 4;

/// One term the sanitizer must remove, with its normalized matching form.
#[derive(Debug, Clone)]
pub struct Term {
    /// Original configured spelling, for reports.
    pub raw: String,
    /// ASCII-lowercased with `_`/`-` removed; the matching form.
    pub normalized: String,
    pub replacement: String,
    pub policy_source: &'static str,
}

/// Build the full term table from config: alias registry, dictionary, and
/// denylist (denylist terms get a deterministic salted alias so they are
/// removed even before a human approves a nicer name). Allowlisted terms are
/// excluded.
///
/// Deliberately infallible: the fail-closed MCP error redactor and the strict
/// runner must always be constructible. Policy enforcement (unmatchable
/// terms, alias injectivity) happens at config load/save, in `verify`, and in
/// proposal validation instead; a config that bypassed all of those still
/// surfaces here as a warning.
pub fn term_table(config: &Config) -> Vec<Term> {
    let allow: BTreeSet<String> = config
        .sanitizer
        .allowlist
        .iter()
        .map(|item| normalize_term(item))
        .collect();
    let mut seen = BTreeSet::new();
    let mut terms = Vec::new();
    let mut push = |raw: &str, replacement: String, policy_source: &'static str| {
        if let Some(reason) = matchability_error(raw) {
            log::warn!("skipping unmatchable sanitizer term ({policy_source}): {reason}");
            return;
        }
        let normalized = normalize_term(raw);
        if allow.contains(&normalized) || !seen.insert(normalized.clone()) {
            return;
        }
        terms.push(Term {
            raw: raw.to_string(),
            normalized,
            replacement,
            policy_source,
        });
    };

    for (term, alias) in &config.sanitizer.alias_registry {
        push(term, alias.clone(), "alias-registry");
    }
    for (term, replacement) in &config.sanitizer.dictionary {
        push(term, replacement.clone(), "static-dictionary");
    }
    for term in &config.sanitizer.denylist {
        push(term, derive_alias(&config.salt, term), "denylist-auto");
    }
    terms
}

pub(crate) fn normalize_term(term: &str) -> String {
    term.chars()
        .filter(|ch| *ch != '_' && *ch != '-')
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

/// Deterministic alias for a term with no human-chosen mapping yet.
pub fn derive_alias(salt: &str, original: &str) -> String {
    format!(
        "sym_{}",
        &sha256_hex(format!("{salt}:{original}").as_bytes())[..8]
    )
}

/// `"{stem}_{4 hex}"` — a readable stem with a per-workspace salted suffix.
/// The default dictionary uses this: a bare English alias ("client") collides
/// with real code, a bare hash is unreadable; the suffix keeps natural
/// occurrence practically impossible while the stem keeps the mirror legible.
pub fn derive_stemmed_alias(salt: &str, original: &str, stem: &str) -> String {
    format!(
        "{stem}_{}",
        &sha256_hex(format!("{salt}:{original}").as_bytes())[..4]
    )
}

/// Why `raw` can never match, or `None` when the term is fine. Matching
/// happens only inside `[A-Za-z0-9_]+` word runs, so a term whose normalized
/// form is empty or contains anything outside `[a-z0-9]` is silently inert —
/// it looks configured but never fires, and `verify`'s leak backstop shares
/// the same blind spot. Such terms are rejected loudly instead.
pub fn matchability_error(raw: &str) -> Option<String> {
    let normalized = normalize_term(raw);
    if normalized.is_empty() {
        return Some(format!(
            "term {raw:?} normalizes to nothing (only '_'/'-' characters)"
        ));
    }
    if let Some(bad) = normalized
        .chars()
        .find(|ch| !ch.is_ascii_lowercase() && !ch.is_ascii_digit())
    {
        return Some(format!(
            "term {raw:?} contains {bad:?} after normalization; the sanitizer \
             matches only inside identifier word runs [A-Za-z0-9_]+ — split it \
             into single-word entries (e.g. \"acme\" + \"corp\") or remove it"
        ));
    }
    // Normalization strips '-', but '-' is a run BOUNDARY in text: a term like
    // "acme-corp" normalizes to a clean-looking needle that spans two word
    // runs and can never match. The raw form must be exactly one word run.
    if word_runs(raw) != [(0, raw.len())] {
        return Some(format!(
            "term {raw:?} spans more than one word run; the sanitizer matches \
             only inside identifier word runs [A-Za-z0-9_]+ — split it into \
             single-word entries (e.g. \"acme\" + \"corp\") or remove it"
        ));
    }
    None
}

/// Every policy violation in the configured term set: unmatchable terms and
/// aliases, non-injective aliases (two terms sharing one alias makes the
/// mirror ambiguous), and aliases that themselves contain a sanitizable term
/// (the sanitizer's own output would be sanitizable — including
/// alias == original). O(T²) in term count; runs at config load/save and in
/// verify, never per file.
pub fn sanitizer_policy_violations(config: &Config) -> Vec<String> {
    let mut violations = Vec::new();
    let mut entries: Vec<(&str, &String, Option<&String>)> = Vec::new();
    entries.extend(
        config
            .sanitizer
            .alias_registry
            .iter()
            .map(|(term, alias)| ("alias-registry", term, Some(alias))),
    );
    entries.extend(
        config
            .sanitizer
            .dictionary
            .iter()
            .map(|(term, alias)| ("dictionary", term, Some(alias))),
    );
    entries.extend(
        config
            .sanitizer
            .denylist
            .iter()
            .map(|term| ("denylist", term, None)),
    );
    for (source, term, alias) in entries {
        if let Some(reason) = matchability_error(term) {
            violations.push(format!("{source} {reason}"));
        }
        if let Some(alias) = alias
            && let Some(reason) = matchability_error(alias)
        {
            violations.push(format!("{source} alias for {term:?}: {reason}"));
        }
    }
    for item in &config.sanitizer.allowlist {
        if let Some(reason) = matchability_error(item) {
            violations.push(format!("allowlist {reason}"));
        }
    }

    // Injectivity and self-cleanliness over the effective term table (the
    // table itself already applies precedence and allowlist filtering).
    let terms = term_table(config);
    let mut alias_owner: std::collections::BTreeMap<String, &Term> =
        std::collections::BTreeMap::new();
    for term in &terms {
        let alias_normalized = normalize_term(&term.replacement);
        if let Some(existing) = alias_owner.get(alias_normalized.as_str()) {
            violations.push(format!(
                "alias {:?} is used for both {:?} and {:?}; the mirror would be ambiguous",
                term.replacement, existing.raw, term.raw
            ));
        } else {
            alias_owner.insert(alias_normalized, term);
        }
    }
    for term in &terms {
        let alias_normalized = normalize_term(&term.replacement);
        for other in &terms {
            if alias_normalized.contains(other.normalized.as_str()) {
                violations.push(format!(
                    "alias {:?} for {:?} still contains sanitizable term {:?}; \
                     the sanitizer's own output would be sanitizable",
                    term.replacement, term.raw, other.raw
                ));
            }
        }
    }

    // Numeric ranges: out-of-range values would not fail loudly anywhere —
    // they silently route everything (or nothing) to review, skip every file,
    // or degenerate the chunker — so they are policy violations with fix-its.
    let sanitizer = &config.sanitizer;
    if !(0.0..=1.0).contains(&sanitizer.confidence_threshold) {
        violations.push(format!(
            "sanitizer.confidence_threshold {} is outside 0.0..=1.0",
            sanitizer.confidence_threshold
        ));
    }
    if sanitizer.propose_max_file_bytes == 0 {
        violations.push(
            "sanitizer.propose_max_file_bytes is 0; every file would be silently skipped"
                .to_string(),
        );
    }
    if config.ignore.max_file_bytes == 0 {
        violations
            .push("ignore.max_file_bytes is 0; every file would be silently skipped".to_string());
    }
    let embeddings = &config.embeddings;
    if embeddings.chunk_lines == 0 {
        violations.push("embeddings.chunk_lines must be at least 1".to_string());
    }
    if embeddings.chunk_lines > 0 && embeddings.chunk_overlap >= embeddings.chunk_lines {
        violations.push(format!(
            "embeddings.chunk_overlap ({}) must be smaller than embeddings.chunk_lines ({})",
            embeddings.chunk_overlap, embeddings.chunk_lines
        ));
    }
    if embeddings.batch_size == 0 {
        violations.push("embeddings.batch_size must be at least 1".to_string());
    }
    if embeddings.timeout_secs == 0 {
        violations.push("embeddings.timeout_secs must be at least 1".to_string());
    }
    // A configured 0-second provider timeout is floored to 1s at use; flag it
    // here so the config says what actually happens.
    let provider_timeout = match &sanitizer.provider {
        crate::config::ProviderConfig::External { timeout_secs, .. }
        | crate::config::ProviderConfig::Llm { timeout_secs, .. }
        | crate::config::ProviderConfig::Openrouter { timeout_secs, .. }
        | crate::config::ProviderConfig::KouRouter { timeout_secs, .. } => *timeout_secs,
        _ => None,
    };
    if provider_timeout == Some(0) {
        violations.push("sanitizer.provider.timeout_secs must be at least 1".to_string());
    }
    violations
}

/// anyhow wrapper over [`sanitizer_policy_violations`], one bullet per issue.
pub fn validate_sanitizer_config(config: &Config) -> Result<()> {
    let violations = sanitizer_policy_violations(config);
    if violations.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "sanitizer config is not valid:\n  - {}\nfix these entries, then run `code-sanity sync`",
        violations.join("\n  - ")
    )
}

/// A word run in REAL content whose normalized form EQUALS a configured
/// alias's normalized form — a genuine ambiguity: after rendering, that word
/// survives verbatim into the mirror where it is indistinguishable from the
/// alias, so reads are misleading and agent edits reverse-map wrongly.
#[derive(Debug, Clone)]
pub struct AliasCollision {
    /// Byte offset of the colliding word run in the content.
    pub offset: usize,
    /// The run as spelled in the real file.
    pub word: String,
    /// The configured alias it collides with (raw spelling).
    pub alias: String,
    /// The term that alias replaces.
    pub term: String,
    pub policy_source: &'static str,
}

/// Scan REAL content for word runs colliding with configured aliases.
/// Equality of normalized forms (not substring): `client_x` does not collide
/// with alias `client`. Protected identifiers and keywords are NOT excluded —
/// a protected word equal to an alias is the worst ambiguity, not a
/// sanctioned one.
pub fn alias_collisions(content: &str, terms: &[Term]) -> Vec<AliasCollision> {
    let by_alias: std::collections::BTreeMap<String, &Term> = terms
        .iter()
        .map(|term| (normalize_term(&term.replacement), term))
        .collect();
    let mut collisions = Vec::new();
    for (start, end) in word_runs(content) {
        let run = &content[start..end];
        if let Some(term) = by_alias.get(normalize_term(run).as_str()) {
            collisions.push(AliasCollision {
                offset: start,
                word: run.to_string(),
                alias: term.replacement.clone(),
                term: term.raw.clone(),
                policy_source: term.policy_source,
            });
        }
    }
    collisions
}

/// A denylisted term that a protected identifier would keep in the mirror.
#[derive(Debug, Clone)]
pub struct ProtectedTermConflict {
    /// Raw denylist spelling.
    pub term: String,
    /// The protected word run that contains it.
    pub protected_name: String,
}

/// Denylisted terms that survive because a protected identifier contains them.
///
/// The protected set exists so public symbols stay real; the denylist exists
/// so a term NEVER reaches the agent. When they collide, one promise must
/// break silently — so instead both are refused and the human decides. Only
/// `denylist-auto` terms qualify: dictionary terms in public names
/// (`pub fn is_dangerous()`) must remain sanctioned residues, or the default
/// dictionary would brick ordinary repos, and registry terms already pass
/// through human review.
///
/// Uses `hits_in_run`, so `pub fn shadowfax_client()` is caught, not just an
/// exact `shadowfax`.
pub fn denylist_protected_conflicts(
    terms: &[Term],
    protected: &BTreeSet<String>,
) -> Vec<ProtectedTermConflict> {
    let denylist: Vec<Term> = terms
        .iter()
        .filter(|term| term.policy_source == "denylist-auto")
        .cloned()
        .collect();
    if denylist.is_empty() {
        return Vec::new();
    }
    let mut conflicts = Vec::new();
    let mut hits = Vec::new();
    for name in protected {
        hits.clear();
        hits_in_run(name, 0, &denylist, &mut hits);
        for hit in &hits {
            conflicts.push(ProtectedTermConflict {
                term: denylist[hit.term_index].raw.clone(),
                protected_name: name.clone(),
            });
        }
    }
    conflicts
}

/// A term occurrence inside a word run.
#[derive(Debug, Clone)]
pub struct TermHit {
    pub start: usize,
    pub end: usize,
    pub term_index: usize,
}

fn is_word_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

/// Byte ranges of maximal `[A-Za-z0-9_]+` runs. Word bytes are ASCII, so run
/// boundaries always sit on UTF-8 character boundaries.
pub fn word_runs(content: &str) -> Vec<(usize, usize)> {
    let bytes = content.as_bytes();
    let mut runs = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if is_word_byte(bytes[cursor]) {
            let start = cursor;
            while cursor < bytes.len() && is_word_byte(bytes[cursor]) {
                cursor += 1;
            }
            runs.push((start, cursor));
        } else {
            cursor += 1;
        }
    }
    runs
}

/// Find every term occurrence in one word run, case- and underscore-
/// insensitively. Hit offsets are absolute (based on `run_start`).
pub fn hits_in_run(run: &str, run_start: usize, terms: &[Term], out: &mut Vec<TermHit>) {
    let mut normalized = String::with_capacity(run.len());
    let mut offsets = Vec::with_capacity(run.len());
    for (idx, byte) in run.bytes().enumerate() {
        if byte == b'_' {
            continue;
        }
        normalized.push(byte.to_ascii_lowercase() as char);
        offsets.push(idx);
    }
    for (term_index, term) in terms.iter().enumerate() {
        let needle = term.normalized.as_str();
        if needle.is_empty() || needle.len() > normalized.len() {
            continue;
        }
        let mut from = 0usize;
        while let Some(found) = normalized[from..].find(needle) {
            let found = from + found;
            let start = offsets[found];
            let end = offsets[found + needle.len() - 1] + 1;
            out.push(TermHit {
                start: run_start + start,
                end: run_start + end,
                term_index,
            });
            from = found + needle.len();
        }
    }
}

/// The only sanctioned residues of a term in the mirror: language keywords
/// and repo-protected names. This single predicate is shared by the renderer
/// (`sanitize_content`), the patch bridge's roundtrip check
/// (`sanitize_run_text`), and the verify leak backstop (`find_leaks`) — the
/// two sides MUST agree or a leak becomes verify-blessed. Dunders are NOT
/// blanket-sanctioned: a genuine code dunder (`__init__`) reaches `protected`
/// via collection, while `__term__` prose (markdown bold) must sanitize.
fn is_sanctioned_run(run: &str, protected: &BTreeSet<String>) -> bool {
    is_keyword(run) || protected.contains(run)
}

/// Sanitize one word run exactly as `sanitize_content` would (same hit
/// selection order and case adaptation), returning the replaced text. The
/// patch bridge uses this to roundtrip-check reverse-mapped identifiers in
/// newly added lines.
pub fn sanitize_run_text(run: &str, terms: &[Term], protected: &BTreeSet<String>) -> String {
    if is_sanctioned_run(run, protected) {
        return run.to_string();
    }
    let mut hits = Vec::new();
    hits_in_run(run, 0, terms, &mut hits);
    hits.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then_with(|| (b.end - b.start).cmp(&(a.end - a.start)))
    });
    let mut out = String::with_capacity(run.len());
    let mut cursor = 0usize;
    for hit in &hits {
        if hit.start < cursor {
            continue;
        }
        out.push_str(&run[cursor..hit.start]);
        out.push_str(&adapt_replacement(
            &run[hit.start..hit.end],
            &terms[hit.term_index].replacement,
        ));
        cursor = hit.end;
    }
    out.push_str(&run[cursor..]);
    out
}

/// Formats with no code declarations: nothing in them can be a public symbol
/// the mirror must keep real, so they contribute NO protected identifiers.
/// Deliberately narrow — unknown extensions stay "text" (= code), because
/// classifying real code as prose would rename import-position names and
/// break the mirror, while the reverse error only over-protects and is
/// caught by the denylist∩protected check.
pub(crate) fn is_prose_language(language: &str) -> bool {
    matches!(language, "markdown" | "plaintext" | "json" | "toml")
}

/// Markers that open a declaration/import context. One list for both rules —
/// the old line/token rules disagreed (package/require vs crate/extern) by
/// accident, and the asymmetry made coverage arbitrary.
fn is_protection_marker(token: &str) -> bool {
    matches!(
        token,
        "use"
            | "import"
            | "from"
            | "mod"
            | "package"
            | "require"
            | "pub"
            | "export"
            | "crate"
            | "extern"
    )
}

/// Import-position markers: on these lines string-literal runs stay protected
/// too (JS/TS `import x from "acme_sdk"`, Go `import "acme/pkg"`) — external
/// module specifiers the mirror cannot rename consistently.
fn is_import_marker(token: &str) -> bool {
    matches!(token, "use" | "import" | "from" | "require" | "package")
}

/// Collect the identifiers this file protects from sanitization: public
/// declarations, import-position names, and code dunders (`__init__`).
/// Name-based on purpose: protecting the name everywhere is the only way
/// "one symbol, one decision" and an independent verify backstop can both
/// hold.
///
/// Collection is CONTEXT-AWARE: prose formats contribute nothing, and within
/// code, runs inside comments or string literals never protect. English
/// prose is full of `from`/`use`/`import` — "Data from shadowfax is loaded"
/// in a README used to protect the denylisted term repo-wide, and verify
/// blessed the leak because it recomputes this same set.
pub fn collect_protected_identifiers(rel_path: &Path, content: &str) -> BTreeSet<String> {
    let mut protected = BTreeSet::new();
    let language = detect_language(rel_path, content);
    if is_prose_language(&language) {
        return protected;
    }
    let strings = string_ranges(&language, content);
    let comments = comment_ranges(&language, content, &strings);

    // One classified run stream shared by both rules. Word runs never
    // contain '\n', so counting newlines in the gaps yields the line number.
    struct Run<'a> {
        text: &'a str,
        line: usize,
        /// Outside comments AND string literals.
        code: bool,
        in_comment: bool,
    }
    let bytes = content.as_bytes();
    let mut runs: Vec<Run> = Vec::new();
    let mut line = 0usize;
    let mut cursor = 0usize;
    for (start, end) in word_runs(content) {
        line += bytes[cursor..start]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count();
        cursor = start;
        let in_comment = range_contains(&comments, start);
        let in_string = range_contains(&strings, start);
        runs.push(Run {
            text: &content[start..end],
            line,
            code: !in_comment && !in_string,
            in_comment,
        });
    }

    // Line rule: a line whose first two CODE runs include a marker protects
    // every non-keyword code run on the line. Covers `use a::b::c::d;`,
    // `import x`, `pub fn name(args)` signatures, `export const x`. On
    // import-position lines, string runs are kept too (module specifiers) —
    // but never comment runs.
    let mut idx = 0usize;
    while idx < runs.len() {
        let line_no = runs[idx].line;
        let line_end = runs[idx..]
            .iter()
            .position(|run| run.line != line_no)
            .map_or(runs.len(), |offset| idx + offset);
        let line_runs = &runs[idx..line_end];
        let lead: Vec<&str> = line_runs
            .iter()
            .filter(|run| run.code)
            .take(2)
            .map(|run| run.text)
            .collect();
        if lead.iter().copied().any(is_protection_marker) {
            let import_line = lead.iter().copied().any(is_import_marker);
            for run in line_runs {
                let eligible = run.code || (import_line && !run.in_comment);
                if eligible && !is_keyword(run.text) {
                    protected.insert(run.text.to_string());
                }
            }
        }
        idx = line_end;
    }

    // Token rule: a code identifier within four code tokens after a marker ON
    // THE SAME LINE is protected (struct fields after `pub`, later path
    // segments, `extern` items). The lookback never crosses a newline and
    // never reads tokens out of comments or strings — prose like
    // "...comes from\nshadowfax handles it" must not protect anything.
    // Genuine code dunders stay protected here; dunder-shaped prose
    // (markdown bold `__term__`) never reaches this branch.
    for (index, run) in runs.iter().enumerate() {
        if !run.code || is_keyword(run.text) {
            continue;
        }
        if run.text.starts_with("__") {
            protected.insert(run.text.to_string());
            continue;
        }
        let marker_nearby = runs[..index]
            .iter()
            .rev()
            .take_while(|prev| prev.line == run.line)
            .filter(|prev| prev.code)
            .take(4)
            .any(|prev| is_protection_marker(prev.text));
        if marker_nearby {
            protected.insert(run.text.to_string());
        }
    }

    protected
}

/// A residual term occurrence found by the leak backstop.
#[derive(Debug, Clone)]
pub struct Leak {
    pub offset: usize,
    pub term: String,
    pub enclosing: String,
}

/// Scan text with the same primitive the sanitizer uses and report every term
/// occurrence whose enclosing word run is not a sanctioned residue (protected
/// identifier or keyword). Used by `verify` as an independent backstop over
/// the mirror and the span-map replacement outputs.
pub fn find_leaks(content: &str, terms: &[Term], protected: &BTreeSet<String>) -> Vec<Leak> {
    let mut leaks = Vec::new();
    let mut hits = Vec::new();
    for (run_start, run_end) in word_runs(content) {
        let run = &content[run_start..run_end];
        if is_sanctioned_run(run, protected) {
            continue;
        }
        hits.clear();
        hits_in_run(run, run_start, terms, &mut hits);
        for hit in &hits {
            leaks.push(Leak {
                offset: hit.start,
                term: terms[hit.term_index].raw.clone(),
                enclosing: run.to_string(),
            });
        }
    }
    leaks
}

pub fn sanitize_content(
    rel_path: &Path,
    content: &str,
    config: &Config,
    protected: &BTreeSet<String>,
) -> Result<RenderedSanitization> {
    let language = detect_language(rel_path, content);
    let string_ranges = string_ranges(&language, content);
    let comment_ranges = comment_ranges(&language, content, &string_ranges);
    let terms = term_table(config);

    let mut candidates = Vec::new();
    let mut hits = Vec::new();
    for (run_start, run_end) in word_runs(content) {
        let run = &content[run_start..run_end];
        if is_sanctioned_run(run, protected) {
            continue;
        }
        hits.clear();
        hits_in_run(run, run_start, &terms, &mut hits);
        for hit in &hits {
            let term = &terms[hit.term_index];
            let slice = &content[hit.start..hit.end];
            let category = if range_contains(&comment_ranges, hit.start) {
                "comment"
            } else if range_contains(&string_ranges, hit.start) {
                "string_literal"
            } else {
                "identifier"
            };
            let (policy_source, confidence) =
                if term.policy_source == "static-dictionary" && category == "identifier" {
                    ("static-dictionary-private-identifier", 0.85)
                } else {
                    (term.policy_source, 1.0)
                };
            candidates.push(PendingReplacement {
                category: category.to_string(),
                sanitized_text: adapt_replacement(slice, &term.replacement),
                stable_key: stable_key(rel_path, slice, category, hit.start),
                original_text: slice.to_string(),
                confidence,
                policy_source: policy_source.to_string(),
                original_start: hit.start,
                original_end: hit.end,
            });
        }
    }

    let replacements = select_non_overlapping(candidates);
    render_with_map(
        &crate::config::normalize_rel_path(rel_path),
        content,
        &language,
        replacements,
        Utc::now().to_rfc3339(),
    )
}

pub fn detect_language(rel_path: &Path, _content: &str) -> String {
    // Extension-less code files are named, not suffixed.
    let file_name = rel_path.file_name().and_then(|name| name.to_str());
    if matches!(file_name, Some("Dockerfile" | "Makefile" | "makefile")) {
        return "shell".to_string();
    }
    match rel_path.extension().and_then(|ext| ext.to_str()) {
        Some("rs") => "rust",
        Some("py") => "python",
        Some("ts") | Some("tsx") => "typescript",
        Some("js") | Some("jsx") => "javascript",
        Some("md") | Some("markdown") => "markdown",
        Some("txt") | Some("rst") | Some("adoc") => "plaintext",
        Some("toml") => "toml",
        Some("json") => "json",
        Some("go") => "go",
        Some("sh") | Some("bash") | Some("zsh") => "shell",
        Some("yml") | Some("yaml") => "yaml",
        // Unknown extensions are CODE (.java, .rb, .php, ...): when in doubt
        // treat a file as code — over-protection there is caught by the
        // denylist∩protected check, while classifying code as prose would
        // rename import-position names and break the mirror.
        _ => "text",
    }
    .to_string()
}

fn select_non_overlapping(mut candidates: Vec<PendingReplacement>) -> Vec<PendingReplacement> {
    candidates.sort_by(|a, b| {
        a.original_start.cmp(&b.original_start).then_with(|| {
            (b.original_end - b.original_start).cmp(&(a.original_end - a.original_start))
        })
    });

    let mut selected: Vec<PendingReplacement> = Vec::new();
    for candidate in candidates {
        if selected
            .last()
            .is_none_or(|last| candidate.original_start >= last.original_end)
        {
            selected.push(candidate);
        }
    }
    selected
}

fn is_keyword(ident: &str) -> bool {
    matches!(
        ident,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "class"
            | "def"
            | "import"
            | "from"
            | "export"
            | "function"
            | "var"
    )
}

pub(crate) fn comment_ranges(
    language: &str,
    content: &str,
    string_ranges: &[ByteRange],
) -> Vec<ByteRange> {
    if matches!(language, "markdown" | "plaintext") {
        return vec![ByteRange {
            start: 0,
            end: content.len(),
        }];
    }

    let mut ranges = Vec::new();
    let bytes = content.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if range_contains(string_ranges, cursor) {
            cursor += 1;
            continue;
        }
        if matches!(
            language,
            "rust" | "typescript" | "javascript" | "go" | "text"
        ) {
            if bytes[cursor..].starts_with(b"//") {
                let end = find_byte(bytes, cursor, b'\n').unwrap_or(content.len());
                ranges.push(ByteRange {
                    start: cursor + 2,
                    end,
                });
                cursor = end;
                continue;
            }
            if language != "text" && bytes[cursor..].starts_with(b"/*") {
                let end = find_bytes(bytes, cursor + 2, b"*/").unwrap_or(content.len());
                ranges.push(ByteRange {
                    start: cursor + 2,
                    end,
                });
                cursor = (end + 2).min(content.len());
                continue;
            }
        }
        // "text" (unknown extension = code) gets BOTH line-comment styles:
        // it approximates whatever language it really is, and misreading a
        // code byte as a comment only ever leads to sanitizing, never to
        // protecting — the fail-safe direction.
        if matches!(language, "python" | "shell" | "yaml" | "text") && bytes[cursor] == b'#' {
            let end = find_byte(bytes, cursor, b'\n').unwrap_or(content.len());
            ranges.push(ByteRange {
                start: cursor + 1,
                end,
            });
            cursor = end;
            continue;
        }
        cursor += 1;
    }
    ranges
}

#[derive(Debug, Clone)]
pub(crate) struct ByteRange {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// String literal ranges (for categorization only; sanitization no longer
/// depends on zone detection). `'` is not a string delimiter in Rust or Go —
/// lifetimes (`&'a str`) and runes would otherwise open phantom strings.
pub(crate) fn string_ranges(language: &str, content: &str) -> Vec<ByteRange> {
    if matches!(language, "markdown" | "plaintext" | "text") {
        return Vec::new();
    }

    let mut ranges = Vec::new();
    let bytes = content.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let quote = bytes[cursor];
        let is_quote = match language {
            "rust" => quote == b'"',
            "go" => quote == b'"' || quote == b'`',
            "typescript" | "javascript" => quote == b'"' || quote == b'\'' || quote == b'`',
            "json" => quote == b'"',
            _ => quote == b'"' || quote == b'\'',
        };
        if !is_quote {
            cursor += 1;
            continue;
        }

        if language == "python"
            && cursor + 2 < bytes.len()
            && bytes[cursor + 1] == quote
            && bytes[cursor + 2] == quote
        {
            let content_start = cursor + 3;
            let mut end = content_start;
            while end + 2 < bytes.len() {
                if bytes[end] == quote && bytes[end + 1] == quote && bytes[end + 2] == quote {
                    ranges.push(ByteRange {
                        start: content_start,
                        end,
                    });
                    cursor = end + 3;
                    break;
                }
                end += 1;
            }
            if end + 2 >= bytes.len() {
                ranges.push(ByteRange {
                    start: content_start,
                    end: content.len(),
                });
                break;
            }
            continue;
        }

        // Go raw strings have no escapes and may span lines.
        if language == "go" && quote == b'`' {
            let content_start = cursor + 1;
            let end = find_byte(bytes, content_start, b'`').unwrap_or(content.len());
            ranges.push(ByteRange {
                start: content_start,
                end,
            });
            cursor = (end + 1).min(content.len());
            continue;
        }

        let content_start = cursor + 1;
        cursor += 1;
        let mut escaped = false;
        while cursor < bytes.len() {
            if escaped {
                escaped = false;
                cursor += 1;
                continue;
            }
            if bytes[cursor] == b'\\' {
                escaped = true;
                cursor += 1;
                continue;
            }
            if bytes[cursor] == quote {
                ranges.push(ByteRange {
                    start: content_start,
                    end: cursor,
                });
                cursor += 1;
                break;
            }
            if bytes[cursor] == b'\n' && quote != b'`' {
                break;
            }
            cursor += 1;
        }
    }
    ranges
}

fn stable_key(rel_path: &Path, original_text: &str, category: &str, offset: usize) -> String {
    sha256_hex(
        format!(
            "{}:{category}:{offset}:{original_text}",
            crate::config::normalize_rel_path(rel_path)
        )
        .as_bytes(),
    )
}

/// Adapt a replacement to the casing of the matched slice: `ACME_CLIENT` gets
/// an upper-cased alias, `Acme` a capitalized one, `acme` the plain form.
/// Non-identifier characters in the replacement become `_` so identifiers
/// stay valid.
pub fn adapt_replacement(original: &str, replacement: &str) -> String {
    let base = to_identifier_word(replacement);
    let has_letters = original.chars().any(|ch| ch.is_ascii_alphabetic());
    let has_lower = original.chars().any(|ch| ch.is_ascii_lowercase());
    let has_upper = original.chars().any(|ch| ch.is_ascii_uppercase());
    if has_letters && !has_lower {
        return base.to_ascii_uppercase();
    }
    if has_letters && !has_upper {
        return base.to_ascii_lowercase();
    }
    if original
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase())
    {
        let mut chars = base.chars();
        return match chars.next() {
            Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
            None => String::new(),
        };
    }
    base
}

fn to_identifier_word(replacement: &str) -> String {
    replacement
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub(crate) fn range_contains(ranges: &[ByteRange], point: usize) -> bool {
    ranges
        .iter()
        .any(|range| point >= range.start && point < range.end)
}

fn find_byte(bytes: &[u8], from: usize, needle: u8) -> Option<usize> {
    bytes[from..]
        .iter()
        .position(|byte| *byte == needle)
        .map(|idx| from + idx)
}

fn find_bytes(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > bytes.len().saturating_sub(from) {
        return None;
    }
    (from..=bytes.len() - needle.len()).find(|idx| &bytes[*idx..*idx + needle.len()] == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use proptest::prelude::*;

    fn sanitize(rel: &str, content: &str) -> RenderedSanitization {
        let config = Config::default();
        sanitize_content(Path::new(rel), content, &config, &BTreeSet::new()).unwrap()
    }

    /// The default alias for `term` (stemmed + salted suffix; deterministic
    /// under Config::default()'s stub salt).
    fn default_alias(term: &str) -> String {
        Config::default()
            .sanitizer
            .dictionary
            .get(term)
            .cloned()
            .unwrap_or_else(|| panic!("{term} is not in the default dictionary"))
    }

    #[test]
    fn sanitizes_comments_and_private_identifiers() {
        let rendered = sanitize(
            "src/lib.rs",
            "// dangerous comment\nfn dangerous_parser() {}\n",
        );
        let alias = default_alias("dangerous");
        assert!(rendered.sanitized.contains(&format!("{alias} comment")));
        assert!(rendered.sanitized.contains(&format!("fn {alias}_parser()")));
        assert!(!rendered.sanitized.contains("dangerous"));
    }

    #[test]
    fn sanitizes_all_string_literals() {
        let rendered = sanitize("src/lib.rs", "let s = \"dangerous\";\n");
        assert_eq!(
            rendered.sanitized,
            format!("let s = \"{}\";\n", default_alias("dangerous"))
        );
        assert_eq!(rendered.span_map.replacements[0].category, "string_literal");
    }

    #[test]
    fn lifetimes_do_not_open_phantom_strings() {
        let rendered = sanitize(
            "src/lib.rs",
            "fn f<'a>(acme_x: &'a str) -> &'a str {\n    acme_x\n}\n",
        );
        assert!(
            rendered
                .sanitized
                .contains(&format!("{}_x", default_alias("acme")))
        );
        assert!(!rendered.sanitized.contains("acme_x"));
    }

    #[test]
    fn multi_token_terms_are_rejected_by_policy_validation() {
        // Hyphenated terms normalize to a clean-looking needle but span two
        // word runs in text — silently inert, so they must be rejected too.
        for term in [
            "Acme Corp.",
            "acme.example.com",
            "a@b",
            "--",
            "com/acme",
            "acme-corp",
            "Acme_Client-2",
            "-acme",
            "acme-",
        ] {
            let reason = matchability_error(term);
            assert!(reason.is_some(), "{term:?} must be unmatchable");
        }
        assert!(matchability_error("acme").is_none());
        // '_' is a word byte: underscore-joined terms match fine.
        assert!(matchability_error("acme_corp").is_none());

        let mut config = Config::default();
        config.sanitizer.denylist = vec!["secret.internal.key".to_string()];
        let violations = sanitizer_policy_violations(&config);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("secret.internal.key")),
            "{violations:?}"
        );
        assert!(crate::sanitize::validate_sanitizer_config(&config).is_err());
    }

    #[test]
    fn alias_injectivity_and_self_cleanliness_violations_are_reported() {
        // Two terms -> one alias.
        let mut config = Config::default();
        config.sanitizer.dictionary = [("acme", "shared"), ("initech", "shared")]
            .into_iter()
            .map(|(term, alias)| (term.to_string(), alias.to_string()))
            .collect();
        let violations = sanitizer_policy_violations(&config);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("used for both")),
            "{violations:?}"
        );

        // Alias contains a (different) sanitizable term.
        let mut config = Config::default();
        config.sanitizer.dictionary = [("acme", "gadget"), ("initech", "acme_service")]
            .into_iter()
            .map(|(term, alias)| (term.to_string(), alias.to_string()))
            .collect();
        let violations = sanitizer_policy_violations(&config);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("still contains sanitizable term")),
            "{violations:?}"
        );

        // The shipped defaults must be self-clean.
        assert_eq!(
            sanitizer_policy_violations(&Config::default()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn numeric_ranges_are_policy_violations() {
        let mut config = Config::default();
        config.sanitizer.confidence_threshold = 5.0;
        config.sanitizer.propose_max_file_bytes = 0;
        config.ignore.max_file_bytes = 0;
        config.embeddings.chunk_lines = 4;
        config.embeddings.chunk_overlap = 4; // must be < chunk_lines
        config.embeddings.batch_size = 0;
        let violations = sanitizer_policy_violations(&config);
        for needle in [
            "confidence_threshold",
            "propose_max_file_bytes",
            "ignore.max_file_bytes",
            "chunk_overlap",
            "batch_size",
        ] {
            assert!(
                violations.iter().any(|v| v.contains(needle)),
                "missing {needle} violation in {violations:?}"
            );
        }
        assert!(crate::sanitize::validate_sanitizer_config(&config).is_err());

        // In-range values stay clean.
        let mut config = Config::default();
        config.sanitizer.confidence_threshold = 0.0;
        assert_eq!(sanitizer_policy_violations(&config), Vec::<String>::new());
        config.sanitizer.confidence_threshold = 1.0;
        assert_eq!(sanitizer_policy_violations(&config), Vec::<String>::new());
    }

    #[test]
    fn alias_collisions_find_case_and_separator_variants() {
        let mut config = Config::default();
        config.sanitizer.dictionary =
            std::iter::once(("acme".to_string(), "client".to_string())).collect();
        let terms = term_table(&config);

        let hits = alias_collisions("fn client() {}\nlet CLI_ENT = c_lient;\n", &terms);
        // `client`, `CLI_ENT` and `c_lient` all normalize to the alias.
        assert_eq!(hits.len(), 3, "{hits:?}");
        assert_eq!(hits[0].word, "client");
        assert_eq!(hits[0].term, "acme");

        // Equality, not substring: client_x is a different identifier.
        assert!(alias_collisions("let client_x = 1;\n", &terms).is_empty());
        // The shipped stemmed defaults do not collide with plain English.
        let default_terms = term_table(&Config::default());
        assert!(alias_collisions("let client = 1; // neutral sample\n", &default_terms).is_empty());
    }

    #[test]
    fn registry_matches_case_and_subword_variants() {
        let mut config = Config::default();
        config
            .sanitizer
            .alias_registry
            .insert("AcmeClient".to_string(), "GadgetSvc".to_string());
        let content = "fn a() { AcmeClient::new(); }\nconst ACME_CLIENT: u8 = 1;\nlet f = acmeClientFactory;\n";
        let rendered =
            sanitize_content(Path::new("src/lib.rs"), content, &config, &BTreeSet::new()).unwrap();
        assert!(rendered.sanitized.contains("GadgetSvc::new()"));
        assert!(rendered.sanitized.contains("const GADGETSVC: u8"));
        assert!(rendered.sanitized.contains("GadgetSvcFactory"));
        assert!(!rendered.sanitized.to_lowercase().contains("acme"));
    }

    #[test]
    fn denylist_terms_get_derived_aliases_everywhere() {
        let mut config = Config::default();
        config.sanitizer.denylist = vec!["shadowfax".to_string()];
        let content = "// shadowfax rollout\nlet shadowfax_kill = 1;\nlet s = \"shadowfax\";\n";
        let rendered =
            sanitize_content(Path::new("src/lib.rs"), content, &config, &BTreeSet::new()).unwrap();
        assert!(!rendered.sanitized.to_lowercase().contains("shadowfax"));
        assert!(rendered.sanitized.contains("sym_"));
    }

    #[test]
    fn protected_identifiers_are_kept_repo_wide() {
        let config = Config::default();
        let mut protected = BTreeSet::new();
        protected.insert("dangerous_parser".to_string());
        let rendered = sanitize_content(
            Path::new("src/lib.rs"),
            "fn call() { dangerous_parser(); }\n",
            &config,
            &protected,
        )
        .unwrap();
        assert!(rendered.sanitized.contains("dangerous_parser()"));
    }

    fn protected_in(rel: &str, content: &str) -> BTreeSet<String> {
        collect_protected_identifiers(Path::new(rel), content)
    }

    #[test]
    fn public_declarations_and_imports_are_collected_as_protected() {
        let protected = protected_in(
            "src/lib.rs",
            "pub fn dangerous_parser() {}\nuse dangerous_lib::helper::thing;\nfn private_one() {}\n",
        );
        assert!(protected.contains("dangerous_parser"));
        assert!(protected.contains("dangerous_lib"));
        assert!(protected.contains("thing"));
        assert!(!protected.contains("private_one"));
    }

    #[test]
    fn prose_formats_protect_nothing() {
        // English prose is full of `from`/`use`/`import`. A README mentioning
        // a denylisted term used to protect it repo-wide, and verify blessed
        // the leak because it recomputes this same set.
        for rel in [
            "README.md",
            "notes.txt",
            "doc.rst",
            "pkg.json",
            "Cargo.toml",
        ] {
            let protected = protected_in(rel, "Data from shadowfax is loaded nightly.\n");
            assert!(protected.is_empty(), "{rel} protected {protected:?}");
        }
    }

    #[test]
    fn markdown_bold_dunder_is_not_protected_and_leaks_are_reported() {
        assert!(protected_in("README.md", "__shadowfax__ is the codename.\n").is_empty());
        // The guard change is what makes this actually sanitize: an
        // unprotected `__term__` run must be reported by the backstop.
        let mut config = Config::default();
        config.sanitizer.denylist = vec!["shadowfax".to_string()];
        let terms = term_table(&config);
        let leaks = find_leaks("__shadowfax__", &terms, &BTreeSet::new());
        assert_eq!(leaks.len(), 1, "{leaks:?}");
        assert_eq!(leaks[0].term, "shadowfax");
    }

    #[test]
    fn genuine_code_dunders_stay_protected() {
        let protected = protected_in(
            "app.py",
            "def __init__(self):\n    pass\n# note __shadowfax__ here\n",
        );
        assert!(protected.contains("__init__"));
        assert!(
            !protected.contains("__shadowfax__"),
            "a dunder inside a comment must not protect"
        );
    }

    #[test]
    fn token_lookback_never_crosses_a_newline() {
        let protected = protected_in("src/lib.rs", "use foo::bar;\nshadowfax_thing();\n");
        assert!(protected.contains("foo"));
        assert!(
            !protected.contains("shadowfax_thing"),
            "the marker was on the previous line"
        );
    }

    #[test]
    fn comments_never_protect() {
        // Rule C used to read tokens straight out of comments.
        let protected = protected_in("src/lib.rs", "// migrated from acme_v1\nlet x = 1;\n");
        assert!(!protected.contains("acme_v1"));
        // Rule A's trailing-comment strip is now the real comment scanner.
        let protected = protected_in(
            "src/lib.rs",
            "pub fn get(url: usize) {} // from shadowfax\n",
        );
        assert!(protected.contains("get"));
        assert!(protected.contains("url"));
        assert!(!protected.contains("shadowfax"));
        // A python trailing comment on a non-declaration line protects nothing.
        assert!(protected_in("app.py", "x = 1  # from shadowfax\n").is_empty());
    }

    #[test]
    fn string_literals_never_protect_except_import_specifiers() {
        // A term in a string literal on a `pub` line must still sanitize.
        let protected = protected_in("src/lib.rs", "pub const G: &str = \"shadowfax rollout\";\n");
        assert!(protected.contains("G"));
        assert!(!protected.contains("shadowfax"));
        assert!(!protected.contains("rollout"));
        // Import-position specifiers are external names the mirror cannot
        // rename consistently, so they stay protected.
        let protected = protected_in("app.ts", "import { thing } from \"acme_sdk\";\n");
        assert!(protected.contains("thing"));
        assert!(protected.contains("acme_sdk"));
        let protected = protected_in("main.go", "import \"acme/pkg\"\n");
        assert!(protected.contains("acme"));
        assert!(protected.contains("pkg"));
    }

    #[test]
    fn unified_marker_list_covers_both_rules() {
        assert!(protected_in("src/lib.rs", "extern crate acme_lib;\n").contains("acme_lib"));
        assert!(protected_in("main.go", "package acmemain\n").contains("acmemain"));
        // Unknown extensions are code: shell-style exports keep protection.
        assert!(protected_in("deploy.sh", "export ACME_TOKEN=x\n").contains("ACME_TOKEN"));
        assert!(protected_in("Main.java", "import com.acme.Foo;\n").contains("acme"));
    }

    #[test]
    fn find_leaks_reports_unsanctioned_terms_only() {
        let config = Config::default();
        let terms = term_table(&config);
        let mut protected = BTreeSet::new();
        protected.insert("dangerous_parser".to_string());
        let leaks = find_leaks("dangerous_parser and dangerous_thing", &terms, &protected);
        assert_eq!(leaks.len(), 1);
        assert_eq!(leaks[0].enclosing, "dangerous_thing");
    }

    proptest! {
        #[test]
        fn rendered_replacements_are_monotonic(prefix in ".*", suffix in ".*") {
            let config = Config::default();
            let input = format!("// {prefix} dangerous {suffix}\n");
            let rendered = sanitize_content(
                Path::new("src/lib.rs"),
                &input,
                &config,
                &BTreeSet::new(),
            ).unwrap();
            let mut last_original = 0usize;
            let mut last_sanitized = 0usize;
            for span in rendered.span_map.spans {
                prop_assert!(span.original_start >= last_original);
                prop_assert!(span.sanitized_start >= last_sanitized);
                last_original = span.original_end;
                last_sanitized = span.sanitized_end;
            }
        }
    }
}
