//! Outbound-text sanitization boundary for agent-facing strings that are not
//! already derived from the sanitized mirror — today the MCP tool error path
//! (tool successes return mirror content; `sh`/`strict-run` streams go
//! through `strict::OutputSanitizer`). A real term must never ride out
//! inside an error message.
//!
//! Matching reuses the sanitizer's own primitive (`word_runs` +
//! case/underscore-insensitive term hits + casing-adaptive replacement), so
//! `ACME_CLIENT` and `acmeClientFactory` are redacted exactly like the mirror
//! renders them, and protected identifiers stay verbatim exactly like the
//! mirror keeps them.

use crate::config::{Config, Layout};
use crate::db;
use crate::map::load_span_map;
use crate::sanitize::{
    Term, normalize_term, path_term_table, sanitize_run_text, sanitize_unprotected_text,
    term_table, word_runs,
};
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::Path;

pub struct Redactor {
    terms: Vec<Term>,
    path_only_terms: Vec<Term>,
    protected: BTreeSet<String>,
}

impl Redactor {
    /// Config-only redactor: dictionary, alias registry, and denylist terms.
    /// The fallback when the workspace (db / span maps) cannot be read — still
    /// guarantees no configured term leaves the process verbatim.
    pub fn terms_only(config: &Config) -> Self {
        Self {
            terms: term_table(config),
            path_only_terms: Vec::new(),
            protected: BTreeSet::new(),
        }
    }

    /// Extend a redactor with accepted symbol-scoped aliases. This is used at
    /// every outbound boundary that starts from real source rather than the
    /// already-rendered unified mirror.
    pub fn with_semantic_aliases(mut self, conn: &rusqlite::Connection) -> Result<Self> {
        let pairs = crate::semantic_store::accepted_alias_pairs(conn)?
            .into_iter()
            .map(|pair| (pair.original, pair.alias));
        self.add_alias_pairs(pairs);
        Ok(self)
    }

    pub fn with_alias_pairs(mut self, pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        self.add_alias_pairs(pairs);
        self
    }

    fn add_alias_pairs(&mut self, pairs: impl IntoIterator<Item = (String, String)>) {
        let mut seen = self
            .terms
            .iter()
            .map(|term| term.normalized.clone())
            .collect::<BTreeSet<_>>();
        for (original, alias) in pairs {
            let normalized = normalize_term(&original);
            if normalized.len() < 2
                || normalized == normalize_term(&alias)
                || !seen.insert(normalized.clone())
            {
                continue;
            }
            self.terms.push(Term {
                raw: original,
                normalized,
                replacement: alias,
                policy_source: "semantic-alias",
            });
        }
    }

    /// Full workspace redactor: config terms plus every span-map replacement
    /// pair (so file-specific derived aliases redact too), with the stored
    /// protected union as sanctioned residue.
    ///
    /// Fails closed: a corrupt span map is an error, not a silently weaker
    /// redactor. Callers that may run concurrently with writers should hold at
    /// least a shared workspace lock while constructing this (never construct
    /// while already holding the exclusive lock on another fd in the same
    /// process — flock would self-deadlock).
    pub fn for_workspace(root: &Path) -> Result<Self> {
        let layout = Layout::new(root);
        layout.require_initialized()?;
        let config = Config::load_or_default(&layout)?;
        let conn = db::connect(&layout)?;
        db::check_schema(&conn)?;

        let mut terms = term_table(&config);
        let path_only_terms = path_term_table(&config)
            .into_iter()
            .filter(|term| term.policy_source == "path-alias-registry")
            .collect();
        let mut seen: BTreeSet<String> = terms.iter().map(|term| term.normalized.clone()).collect();
        for file in db::tracked_files(&conn)? {
            let map_path = layout.map_path(Path::new(&file));
            let span_map = load_span_map(&map_path).with_context(|| {
                format!(
                    "span map {} is missing or corrupt; run `code-sanity sync --force` \
                     before output can be sanitized",
                    map_path.display()
                )
            })?;
            for replacement in &span_map.replacements {
                let normalized = normalize_term(&replacement.original_text);
                if normalized.len() < 2
                    || normalized == normalize_term(&replacement.sanitized_text)
                    || !seen.insert(normalized.clone())
                {
                    continue;
                }
                terms.push(Term {
                    raw: replacement.original_text.clone(),
                    normalized,
                    replacement: replacement.sanitized_text.clone(),
                    policy_source: "span-map-reverse",
                });
            }
        }
        let protected = crate::index::stored_protected_union(&conn)?;
        Self {
            terms,
            path_only_terms,
            protected,
        }
        .with_semantic_aliases(&conn)
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.path_only_terms.is_empty()
    }

    /// Redact every term occurrence in `text`, leaving protected identifiers
    /// and keywords verbatim — the same residue rule as the mirror.
    pub fn redact(&self, text: &str) -> String {
        if self.terms.is_empty() && self.path_only_terms.is_empty() {
            return text.to_string();
        }
        let path_redacted = sanitize_unprotected_text(text, &self.path_only_terms);
        let mut out = String::with_capacity(path_redacted.len());
        let mut cursor = 0usize;
        for (run_start, run_end) in word_runs(&path_redacted) {
            out.push_str(&path_redacted[cursor..run_start]);
            out.push_str(&sanitize_run_text(
                &path_redacted[run_start..run_end],
                &self.terms,
                &self.protected,
            ));
            cursor = run_end;
        }
        out.push_str(&path_redacted[cursor..]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(dictionary: &[(&str, &str)]) -> Config {
        let mut config = Config::default();
        config.sanitizer.dictionary = dictionary
            .iter()
            .map(|(term, alias)| (term.to_string(), alias.to_string()))
            .collect();
        config
    }

    #[test]
    fn redacts_case_and_underscore_variants() {
        let redactor = Redactor::terms_only(&config_with(&[("acme", "client")]));
        assert_eq!(
            redactor.redact("error: ACME_CLIENT failed in acmeParser at Acme"),
            "error: CLIENT_CLIENT failed in clientParser at Client"
        );
    }

    #[test]
    fn protected_identifiers_stay_verbatim() {
        let config = config_with(&[("acme", "client")]);
        let mut redactor = Redactor::terms_only(&config);
        redactor.protected.insert("acme_public_api".to_string());
        assert_eq!(
            redactor.redact("call acme_public_api with acme_secret"),
            "call acme_public_api with client_secret"
        );
    }

    #[test]
    fn empty_terms_pass_through() {
        let redactor = Redactor::terms_only(&config_with(&[]));
        assert_eq!(redactor.redact("anything"), "anything");
    }
}
