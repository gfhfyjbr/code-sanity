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
    /// Deterministic alias registry: exact original term -> approved alias.
    /// Populated by approving model proposals; consulted by the deterministic
    /// engine during index so the model never writes the mirror directly.
    #[serde(default)]
    pub alias_registry: BTreeMap<String, String>,
}

fn default_confidence_threshold() -> f64 {
    0.8
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ProviderConfig {
    /// Deterministic dictionary + alias registry engine (default).
    Stub,
    /// Offline/local model provider: `command` is invoked with `{rel, content}`
    /// JSON on stdin and must emit a proposal batch on stdout. Used by
    /// `propose-sanitize` only; never during index/verify.
    External { command: Vec<String> },
    LlmStub { endpoint: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IgnoreConfig {
    pub extra_dirs: Vec<String>,
    pub lockfiles: Vec<String>,
    pub generated_dirs: Vec<String>,
    pub max_file_bytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        let dictionary = BTreeMap::from([
            ("acme".to_string(), "client".to_string()),
            ("attack".to_string(), "exercise".to_string()),
            ("dangerous".to_string(), "neutral".to_string()),
            ("evil".to_string(), "sample".to_string()),
            ("exfiltrate".to_string(), "transfer".to_string()),
            ("malware".to_string(), "diagnostic".to_string()),
            ("privatecorp".to_string(), "examplecorp".to_string()),
        ]);

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
        }
    }
}

impl Config {
    pub fn load_or_default(layout: &Layout) -> Result<Self> {
        if !layout.config_path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&layout.config_path)
            .with_context(|| format!("read {}", layout.config_path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parse {}", layout.config_path.display()))
    }

    pub fn write_if_missing(&self, layout: &Layout) -> Result<()> {
        if layout.config_path.exists() {
            return Ok(());
        }
        self.save(layout)
    }

    pub fn save(&self, layout: &Layout) -> Result<()> {
        let raw = toml::to_string_pretty(self).context("serialize config")?;
        fs::write(&layout.config_path, raw)
            .with_context(|| format!("write {}", layout.config_path.display()))
    }
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
