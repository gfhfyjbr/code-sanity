use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
    pub state_dir: PathBuf,
    pub config_path: PathBuf,
    pub db_path: PathBuf,
    pub mirror_dir: PathBuf,
    pub maps_dir: PathBuf,
    pub journal_dir: PathBuf,
    pub review_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub tmp_dir: PathBuf,
}

impl Layout {
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        let state_dir = root.join(".code-sanity");
        Self {
            config_path: state_dir.join("config.toml"),
            db_path: state_dir.join("db.sqlite"),
            mirror_dir: state_dir.join("mirror"),
            maps_dir: state_dir.join("maps"),
            journal_dir: state_dir.join("journal"),
            review_dir: state_dir.join("review"),
            logs_dir: state_dir.join("logs"),
            tmp_dir: state_dir.join("tmp"),
            state_dir,
            root,
        }
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.state_dir,
            &self.mirror_dir,
            &self.maps_dir,
            &self.journal_dir,
            &self.review_dir,
            &self.logs_dir,
            &self.tmp_dir,
        ] {
            fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        Ok(())
    }

    /// Read-path guard: fail without creating any state when the workspace
    /// has never been initialized. Write paths go through init and create the
    /// state dir; read commands pointed at a random directory must not conjure
    /// a `.code-sanity/` there just to report an error.
    pub fn require_initialized(&self) -> Result<()> {
        if self.state_dir.is_dir() {
            return Ok(());
        }
        bail!(
            "{} is not a code-sanity workspace; run `code-sanity init` \
             (or `code-sanity index`) first",
            self.root.display()
        )
    }

    /// Durable evidence this workspace was initialized before, independent of
    /// config.toml: the derived db, or any rendered mirror/map state. Init
    /// writes the config BEFORE creating the db, so a crashed init never
    /// leaves initialized state without a config — if this is true and
    /// config.toml is missing, the config was lost, not never written.
    pub fn has_initialized_state(&self) -> bool {
        self.db_path.exists()
            || dir_has_entries(&self.mirror_dir)
            || dir_has_entries(&self.maps_dir)
    }

    pub fn map_path(&self, rel_path: &Path) -> PathBuf {
        let mut out = self.maps_dir.join(rel_path);
        let file_name = rel_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file");
        out.set_file_name(format!("{file_name}.map.json"));
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    pub mode: Mode,
    pub salt: String,
    pub sanitizer: SanitizerConfig,
    pub ignore: IgnoreConfig,
    /// Optional semantic index over the sanitized mirror (disabled by default).
    #[serde(default)]
    pub embeddings: EmbeddingsConfig,
    #[serde(default)]
    pub journal: JournalConfig,
    #[serde(default)]
    pub durability: DurabilityConfig,
}

/// Storage-trust knobs: how hard to push writes toward the physical medium
/// and whether to tolerate filesystems where locking is unreliable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DurabilityConfig {
    /// macOS only: route every durable-tier fsync through `F_FULLFSYNC`
    /// (flushes the drive's own write cache; plain `fsync(2)` does not on
    /// macOS). The journal `applying` entry always gets the full flush
    /// regardless — this knob extends it to real-file writes, config saves,
    /// and stashes for full power-loss durability at ~10-100x fsync cost.
    #[serde(default)]
    pub full_fsync: bool,
    /// Permit exclusive (writer) workspace locks on a detected network
    /// filesystem (NFS/SMB/…), where flock may be host-local or a no-op and
    /// two hosts could silently corrupt the workspace. Off by default:
    /// writers on a network FS fail with an actionable error instead.
    #[serde(default)]
    pub allow_network_fs: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalConfig {
    /// Retention for apply history: terminal journal entries
    /// (success/conflict/rolled-back) on disk and in the `patch_journal`
    /// table, `journal/discarded/` stashes of force-reset mirror edits, and
    /// RESOLVED review-queue items — the oldest beyond this are pruned
    /// best-effort after each apply / force-sync / review resolution.
    /// Entries hold full before/after file snapshots, so unbounded history
    /// grows without limit on a busy workspace. `0` disables pruning.
    /// In-flight (`applying`) entries, pending review items, and unparseable
    /// files are never pruned.
    #[serde(default = "default_journal_max_entries")]
    pub max_entries: u64,
}

impl Default for JournalConfig {
    fn default() -> Self {
        Self {
            max_entries: default_journal_max_entries(),
        }
    }
}

fn default_journal_max_entries() -> u64 {
    500
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    Soft,
    Guided,
    Strict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SanitizerConfig {
    pub provider: ProviderConfig,
    pub preserve_line_count: bool,
    pub dictionary: BTreeMap<String, String>,
    pub allowlist: Vec<String>,
    /// Terms that must never survive into the mirror. A proposal whose output
    /// still contains a denylisted term is rejected.
    #[serde(default)]
    pub denylist: Vec<String>,
    /// Proposals below this confidence are routed to the review queue instead of
    /// being eligible for approval-free handling.
    #[serde(default = "default_confidence_threshold")]
    pub confidence_threshold: f64,
    /// Max real-file size (bytes) sent to a proposal provider in one request;
    /// larger files are skipped and reported. Guards the model's context
    /// window: a max_file_bytes-sized file in one chat message overflows most
    /// models and used to abort the whole run with an HTTP 400.
    #[serde(default = "default_propose_max_file_bytes")]
    pub propose_max_file_bytes: u64,
    /// Deterministic alias registry: exact original term -> approved alias.
    /// Populated by approving model proposals; consulted by the deterministic
    /// engine during index so the model never writes the mirror directly.
    #[serde(default)]
    pub alias_registry: BTreeMap<String, String>,
}

fn default_confidence_threshold() -> f64 {
    0.8
}

fn default_propose_max_file_bytes() -> u64 {
    192 * 1024
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ProviderConfig {
    /// Deterministic dictionary + alias registry engine (default).
    Stub,
    /// Offline/local model provider: `command` is invoked with `{rel, content}`
    /// JSON on stdin and must emit a proposal batch on stdout. Used by
    /// `propose-sanitize` only; never during index/verify. Because the command
    /// comes from repo-local config, running it requires explicit confirmation
    /// (`--allow-provider-command`).
    External {
        command: Vec<String>,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
    LlmStub {
        endpoint: Option<String>,
    },
    /// OpenAI-compatible chat endpoint (e.g. a local kou-router gateway on
    /// `http://127.0.0.1:20128/v1`). `propose-sanitize` sends REAL file content
    /// to it, and the endpoint comes from repo-local config, so running it
    /// requires explicit confirmation (`--allow-provider-endpoint`). The API
    /// key is read from the env var named by `api_key_env`, never from config.
    Llm {
        base_url: String,
        model: String,
        #[serde(default = "default_llm_api_key_env")]
        api_key_env: String,
        #[serde(default)]
        timeout_secs: Option<u64>,
        /// Send `response_format: {"type": "json_object"}` with chat requests.
        /// Off by default: not every OpenAI-compatible gateway accepts the
        /// parameter, and fence-stripping already handles prose-wrapped JSON.
        #[serde(default)]
        json_mode: bool,
    },
    /// OpenRouter preset: the same wire protocol as `llm`, with `base_url`
    /// defaulting to the OpenRouter API and `api_key_env` to
    /// OPENROUTER_API_KEY. A remote endpoint receiving REAL file content, so
    /// the `--allow-provider-endpoint` confirmation applies here too.
    Openrouter {
        model: String,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default)]
        api_key_env: Option<String>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        json_mode: bool,
    },
    /// Local kou-router gateway preset: `base_url` defaults to
    /// `http://127.0.0.1:20128/v1` and `api_key_env` to KOU_ROUTER_API_KEY.
    /// Loopback is only a default — the URL is repo-configurable, so the
    /// same confirmation gate applies.
    KouRouter {
        model: String,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default)]
        api_key_env: Option<String>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        json_mode: bool,
    },
}

pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const KOU_ROUTER_BASE_URL: &str = "http://127.0.0.1:20128/v1";

const DEFAULT_LLM_TIMEOUT_SECS: u64 = 120;

/// A resolved OpenAI-compatible chat endpoint with preset defaults applied.
#[derive(Debug, Clone)]
pub struct LlmEndpoint {
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub timeout_secs: u64,
    /// Ask the endpoint for strict-JSON output (`response_format`); opt-in.
    pub json_mode: bool,
}

impl ProviderConfig {
    /// Resolve any OpenAI-compatible chat kind (`llm`, `openrouter`,
    /// `kou-router`) into one endpoint description; other kinds return None.
    pub fn llm_endpoint(&self) -> Option<LlmEndpoint> {
        match self {
            ProviderConfig::Llm {
                base_url,
                model,
                api_key_env,
                timeout_secs,
                json_mode,
            } => Some(LlmEndpoint {
                base_url: base_url.clone(),
                model: model.clone(),
                api_key_env: api_key_env.clone(),
                timeout_secs: timeout_secs.unwrap_or(DEFAULT_LLM_TIMEOUT_SECS),
                json_mode: *json_mode,
            }),
            ProviderConfig::Openrouter {
                model,
                base_url,
                api_key_env,
                timeout_secs,
                json_mode,
            } => Some(LlmEndpoint {
                base_url: base_url
                    .clone()
                    .unwrap_or_else(|| OPENROUTER_BASE_URL.to_string()),
                model: model.clone(),
                api_key_env: api_key_env
                    .clone()
                    .unwrap_or_else(default_embeddings_api_key_env),
                timeout_secs: timeout_secs.unwrap_or(DEFAULT_LLM_TIMEOUT_SECS),
                json_mode: *json_mode,
            }),
            ProviderConfig::KouRouter {
                model,
                base_url,
                api_key_env,
                timeout_secs,
                json_mode,
            } => Some(LlmEndpoint {
                base_url: base_url
                    .clone()
                    .unwrap_or_else(|| KOU_ROUTER_BASE_URL.to_string()),
                model: model.clone(),
                api_key_env: api_key_env.clone().unwrap_or_else(default_llm_api_key_env),
                timeout_secs: timeout_secs.unwrap_or(DEFAULT_LLM_TIMEOUT_SECS),
                json_mode: *json_mode,
            }),
            ProviderConfig::Stub
            | ProviderConfig::External { .. }
            | ProviderConfig::LlmStub { .. } => None,
        }
    }
}

fn default_llm_api_key_env() -> String {
    "KOU_ROUTER_API_KEY".to_string()
}

/// Semantic index configuration. Vectors are always computed from the
/// **sanitized mirror** — the same text agents already read — so enabling this
/// sends no real names to the embedding endpoint. Defaults target OpenRouter's
/// OpenAI-compatible `/embeddings`; any compatible endpoint (including a local
/// kou-router route) works via `base_url`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_embeddings_base_url")]
    pub base_url: String,
    #[serde(default = "default_embeddings_model")]
    pub model: String,
    /// Env var holding the API key; the key itself never lives in the repo.
    #[serde(default = "default_embeddings_api_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_chunk_lines")]
    pub chunk_lines: usize,
    #[serde(default = "default_chunk_overlap")]
    pub chunk_overlap: usize,
    /// Chunks per embeddings request.
    #[serde(default = "default_embed_batch_size")]
    pub batch_size: usize,
    #[serde(default = "default_embed_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: default_embeddings_base_url(),
            model: default_embeddings_model(),
            api_key_env: default_embeddings_api_key_env(),
            chunk_lines: default_chunk_lines(),
            chunk_overlap: default_chunk_overlap(),
            batch_size: default_embed_batch_size(),
            timeout_secs: default_embed_timeout_secs(),
        }
    }
}

fn default_embeddings_base_url() -> String {
    OPENROUTER_BASE_URL.to_string()
}

fn default_embeddings_model() -> String {
    "openai/text-embedding-3-small".to_string()
}

fn default_embeddings_api_key_env() -> String {
    "OPENROUTER_API_KEY".to_string()
}

fn default_chunk_lines() -> usize {
    60
}

fn default_chunk_overlap() -> usize {
    10
}

fn default_embed_batch_size() -> usize {
    32
}

fn default_embed_timeout_secs() -> u64 {
    120
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IgnoreConfig {
    pub extra_dirs: Vec<String>,
    pub lockfiles: Vec<String>,
    pub generated_dirs: Vec<String>,
    pub max_file_bytes: u64,
}

/// Default dictionary with per-salt synthetic aliases (`neutral_3fd1`-style,
/// see `derive_stemmed_alias`). Bare English aliases ("client", "neutral")
/// collide with words that occur naturally in real code — and a collision
/// makes the mirror ambiguous and reverse-maps agent-typed words into real
/// terms. The stem keeps the mirror readable; the salted suffix makes natural
/// occurrence practically impossible.
pub fn default_dictionary(salt: &str) -> BTreeMap<String, String> {
    [
        ("acme", "client"),
        ("attack", "exercise"),
        ("dangerous", "neutral"),
        ("evil", "sample"),
        ("exfiltrate", "transfer"),
        ("malware", "diagnostic"),
        ("privatecorp", "examplecorp"),
    ]
    .into_iter()
    .map(|(term, stem)| {
        (
            term.to_string(),
            crate::sanitize::derive_stemmed_alias(salt, term, stem),
        )
    })
    .collect()
}

impl Default for Config {
    fn default() -> Self {
        // The stub salt keeps unit tests deterministic; init_workspace
        // re-derives the dictionary from the real random salt.
        let dictionary = default_dictionary("code-sanity-local-stub");

        Self {
            version: 1,
            mode: Mode::Guided,
            salt: "code-sanity-local-stub".to_string(),
            sanitizer: SanitizerConfig {
                provider: ProviderConfig::Stub,
                preserve_line_count: true,
                dictionary,
                allowlist: vec![
                    "delete".to_string(),
                    "drop".to_string(),
                    "encrypt".to_string(),
                    "decrypt".to_string(),
                    "auth".to_string(),
                    "token".to_string(),
                ],
                denylist: Vec::new(),
                confidence_threshold: default_confidence_threshold(),
                propose_max_file_bytes: default_propose_max_file_bytes(),
                alias_registry: BTreeMap::new(),
            },
            ignore: IgnoreConfig {
                extra_dirs: vec![
                    ".git".to_string(),
                    ".code-sanity".to_string(),
                    "target".to_string(),
                    "node_modules".to_string(),
                    ".venv".to_string(),
                ],
                generated_dirs: vec![
                    "dist".to_string(),
                    "build".to_string(),
                    "coverage".to_string(),
                    "__pycache__".to_string(),
                ],
                lockfiles: vec![
                    "Cargo.lock".to_string(),
                    "package-lock.json".to_string(),
                    "pnpm-lock.yaml".to_string(),
                    "yarn.lock".to_string(),
                    "bun.lockb".to_string(),
                    "poetry.lock".to_string(),
                    "Pipfile.lock".to_string(),
                ],
                max_file_bytes: 1024 * 1024,
            },
            embeddings: EmbeddingsConfig::default(),
            journal: JournalConfig::default(),
            durability: DurabilityConfig::default(),
        }
    }
}

impl Config {
    /// Load and policy-validate the config. The validation error names every
    /// offending entry (this is the actionable upgrade path for a persisted
    /// config that predates validation), never a panic.
    pub fn load_or_default(layout: &Layout) -> Result<Self> {
        let config = Self::load_or_default_lenient(layout)?;
        crate::sanitize::validate_sanitizer_config(&config)
            .with_context(|| format!("{} failed validation", layout.config_path.display()))?;
        Ok(config)
    }

    /// Load without policy validation (TOML parse errors still fail). Only
    /// for `verify`, which reports violations as findings instead of dying,
    /// and for other paths that must observe a broken config to explain it.
    ///
    /// A MISSING config on an already-initialized workspace is a hard error,
    /// not a silent default: the config holds the workspace salt and the
    /// human-approved alias registry, and proceeding with defaults would
    /// re-render the mirror without the user's sanitization policy —
    /// previously hidden terms would surface in the agent-facing view.
    pub fn load_or_default_lenient(layout: &Layout) -> Result<Self> {
        let config = if layout.config_path.exists() {
            let raw = fs::read_to_string(&layout.config_path)
                .with_context(|| format!("read {}", layout.config_path.display()))?;
            toml::from_str(&raw)
                .with_context(|| format!("parse {}", layout.config_path.display()))?
        } else if layout.has_initialized_state() {
            return Err(missing_config_error(layout));
        } else {
            Self::default()
        };
        // The write primitives are free functions far from any config; arm
        // the process-wide full-fsync switch at the single load chokepoint.
        crate::fsutil::set_full_fsync(config.durability.full_fsync);
        Ok(config)
    }

    // NOTE: there is deliberately no write_if_missing convenience — a missing
    // config on an initialized workspace must be a hard error (see
    // load_or_default_lenient), never a silent regeneration.

    /// Durable atomic save with a `.bak` copy of previous, different content —
    /// the config holds the salt and the human-approved alias registry, which
    /// are not derivable from anything else. Validates first: programmatic
    /// writers (proposal approval) must not persist a policy violation.
    pub fn save(&self, layout: &Layout) -> Result<()> {
        crate::sanitize::validate_sanitizer_config(self).context("refusing to save config")?;
        let raw = toml::to_string_pretty(self).context("serialize config")?;
        crate::fsutil::write_with_backup_sync(&layout.config_path, &raw)
            .with_context(|| format!("write {}", layout.config_path.display()))
    }
}

fn dir_has_entries(dir: &Path) -> bool {
    fs::read_dir(dir)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

/// The error every path raises when config.toml is missing on a workspace
/// that has initialized state. One message, one remedy, everywhere.
///
/// `Config::save` only writes a `.bak` when it replaces DIFFERENT previous
/// content, so a workspace whose config was never re-saved has none — point
/// at it only when it is actually there, or the remedy sends the user chasing
/// a file that does not exist.
pub fn missing_config_error(layout: &Layout) -> anyhow::Error {
    let backup = layout.config_path.with_extension("toml.bak");
    let restore = if backup.exists() {
        format!(
            "Restore it from {} or from version control",
            backup.display()
        )
    } else {
        "Restore it from version control".to_string()
    };
    anyhow::anyhow!(
        "{} is missing but this workspace is already initialized; the config \
         holds the workspace salt and the approved alias registry, which \
         cannot be re-derived. {restore}; or delete the entire {} directory \
         and re-run `code-sanity init` to reset deliberately (new salt, \
         default policy, full re-render)",
        layout.config_path.display(),
        layout.state_dir.display(),
    )
}

/// A per-workspace random salt so derived aliases are not guessable or
/// comparable across repositories. 16 bytes from /dev/urandom (the project is
/// Unix-only), 32 hex chars. Fallback: a time+pid hash — LOW ENTROPY (an
/// offline attacker with a candidate term list could confirm guesses against
/// sym_/stemmed aliases), kept only so init cannot fail on salt generation in
/// a broken chroot; it warns loudly.
pub fn random_salt() -> String {
    if let Ok(mut file) = fs::File::open("/dev/urandom") {
        let mut bytes = [0u8; 16];
        if std::io::Read::read_exact(&mut file, &mut bytes).is_ok() {
            return bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        }
    }
    log::warn!(
        "/dev/urandom unavailable; falling back to a LOW-ENTROPY time-based \
         workspace salt — derived aliases may be guessable"
    );
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let seed = format!("code-sanity-salt:{nanos}:{}", std::process::id());
    crate::map::sha256_hex(seed.as_bytes())[..16].to_string()
}

pub fn rel_path(root: &Path, path: &Path) -> Result<PathBuf> {
    path.strip_prefix(root)
        .with_context(|| format!("{} is not under {}", path.display(), root.display()))
        .map(PathBuf::from)
}

pub fn normalize_rel_path(path: &Path) -> String {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

pub fn normalize_safe_rel_path(path: &Path, boundary: &str) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("path escapes {boundary}: {}", path.display());
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        bail!("empty path for {boundary}");
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llm_provider_toml_stays_backward_compatible() {
        let provider: ProviderConfig = toml::from_str(
            "kind = \"llm\"\n\
             base_url = \"http://127.0.0.1:20128/v1\"\n\
             model = \"claude-sonnet-5\"\n",
        )
        .unwrap();
        let endpoint = provider.llm_endpoint().unwrap();
        assert_eq!(endpoint.base_url, "http://127.0.0.1:20128/v1");
        assert_eq!(endpoint.model, "claude-sonnet-5");
        assert_eq!(endpoint.api_key_env, "KOU_ROUTER_API_KEY");
        assert_eq!(endpoint.timeout_secs, 120);
    }

    #[test]
    fn openrouter_and_kou_router_presets_fill_defaults() {
        let provider: ProviderConfig =
            toml::from_str("kind = \"openrouter\"\nmodel = \"anthropic/claude-sonnet-4.5\"\n")
                .unwrap();
        let endpoint = provider.llm_endpoint().unwrap();
        assert_eq!(endpoint.base_url, OPENROUTER_BASE_URL);
        assert_eq!(endpoint.api_key_env, "OPENROUTER_API_KEY");
        assert_eq!(endpoint.model, "anthropic/claude-sonnet-4.5");

        let provider: ProviderConfig = toml::from_str(
            "kind = \"kou-router\"\nmodel = \"claude-sonnet-5\"\ntimeout_secs = 30\n",
        )
        .unwrap();
        let endpoint = provider.llm_endpoint().unwrap();
        assert_eq!(endpoint.base_url, KOU_ROUTER_BASE_URL);
        assert_eq!(endpoint.api_key_env, "KOU_ROUTER_API_KEY");
        assert_eq!(endpoint.timeout_secs, 30);
    }

    #[test]
    fn preset_overrides_beat_defaults() {
        let provider: ProviderConfig = toml::from_str(
            "kind = \"openrouter\"\n\
             model = \"m\"\n\
             base_url = \"http://127.0.0.1:9999/v1\"\n\
             api_key_env = \"MY_KEY\"\n",
        )
        .unwrap();
        let endpoint = provider.llm_endpoint().unwrap();
        assert_eq!(endpoint.base_url, "http://127.0.0.1:9999/v1");
        assert_eq!(endpoint.api_key_env, "MY_KEY");
    }

    #[test]
    fn non_llm_kinds_have_no_endpoint() {
        assert!(ProviderConfig::Stub.llm_endpoint().is_none());
        assert!(
            ProviderConfig::External {
                command: vec!["true".to_string()],
                timeout_secs: None,
            }
            .llm_endpoint()
            .is_none()
        );
    }

    #[test]
    fn load_or_default_falls_back_and_surfaces_parse_errors() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        // Missing file: defaults, no error.
        let config = Config::load_or_default(&layout).unwrap();
        assert!(!config.embeddings.enabled);
        // Malformed TOML: a named parse error, not silent defaults.
        layout.ensure_dirs().unwrap();
        std::fs::write(&layout.config_path, "version = [not toml").unwrap();
        let err = Config::load_or_default(&layout).unwrap_err();
        assert!(err.to_string().contains("config.toml"));
    }

    #[test]
    fn unknown_provider_kind_is_a_parse_error() {
        let err = toml::from_str::<ProviderConfig>("kind = \"open-router\"\nmodel = \"m\"\n")
            .unwrap_err();
        // The kebab-case tag for the preset is exactly "openrouter".
        assert!(err.to_string().contains("open-router"));
        assert!(toml::from_str::<ProviderConfig>("kind = \"openrouter\"\nmodel = \"m\"\n").is_ok());
    }

    #[test]
    fn random_salt_is_hex_and_distinct() {
        let first = random_salt();
        let second = random_salt();
        assert_ne!(first, second, "salt must not be constant");
        // /dev/urandom path: 16 bytes -> 32 lowercase hex chars.
        assert_eq!(first.len(), 32);
        assert!(first.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn normalize_safe_rel_path_blocks_escapes() {
        use std::path::Path;
        assert_eq!(
            normalize_safe_rel_path(Path::new("./src/lib.rs"), "mirror").unwrap(),
            PathBuf::from("src/lib.rs")
        );
        assert!(normalize_safe_rel_path(Path::new("../evil"), "mirror").is_err());
        assert!(normalize_safe_rel_path(Path::new("/abs/path"), "mirror").is_err());
        assert!(normalize_safe_rel_path(Path::new(""), "mirror").is_err());
    }

    #[test]
    fn preset_config_survives_save_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        layout.ensure_dirs().unwrap();
        let mut config = Config::default();
        config.sanitizer.provider = ProviderConfig::Openrouter {
            model: "anthropic/claude-sonnet-4.5".to_string(),
            base_url: None,
            api_key_env: None,
            timeout_secs: None,
            json_mode: false,
        };
        config.save(&layout).unwrap();
        let reloaded = Config::load_or_default(&layout).unwrap();
        let endpoint = reloaded.sanitizer.provider.llm_endpoint().unwrap();
        assert_eq!(endpoint.base_url, OPENROUTER_BASE_URL);
        assert_eq!(endpoint.model, "anthropic/claude-sonnet-4.5");
    }
}
