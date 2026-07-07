use crate::index::{index_workspace, init_workspace};
use crate::patch::{
    ApplyOptions, apply_patch_text_with_options, project_mirror_edit, recover_workspace,
    rename_alias, write_sanitized_content,
};
use crate::search::read_sanitized_file;
use crate::verify::verify_workspace;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "code-sanity")]
#[command(version)]
#[command(about = "Sanitized mirror and patch bridge for agent code workflows")]
pub struct Cli {
    #[arg(long, global = true, default_value = ".")]
    root: PathBuf,

    /// Raise log verbosity (-v: debug to log file, info to stderr; -vv: trace).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init,
    Index,
    Read {
        path: PathBuf,
    },
    #[command(alias = "grep")]
    Search {
        query: String,
        #[arg(long)]
        glob: Option<String>,
        /// Cap on returned matches (default 200, hard max 1000).
        #[arg(long)]
        max_results: Option<usize>,
    },
    ApplyPatch {
        #[arg(long)]
        patch: Option<PathBuf>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        session_id: Option<String>,
    },
    Write {
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        sanitized_content: Option<PathBuf>,
    },
    /// Rename a sanitized alias to a new name (renames the real symbol).
    Rename {
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Back-project an in-place edit of a mirror file to the real repo.
    ProjectEdit {
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Replay or roll back an apply interrupted mid-write.
    Recover {
        #[arg(long)]
        rollback: bool,
        /// Overwrite files even when their content changed after the crash.
        #[arg(long)]
        force: bool,
    },
    /// Print the configured enforcement mode (soft|guided|strict).
    Mode,
    /// Run the configured proposal provider and queue proposals for review.
    ProposeSanitize {
        #[arg(long)]
        path: Option<PathBuf>,
        /// Confirm executing the provider command from repo-local config.
        #[arg(long)]
        allow_provider_command: bool,
        /// Confirm posting real file content to the LLM endpoint from
        /// repo-local config (e.g. a local kou-router gateway).
        #[arg(long)]
        allow_provider_endpoint: bool,
    },
    /// List or resolve queued sanitization proposals.
    Review {
        #[arg(long)]
        approve: Option<String>,
        #[arg(long)]
        reject: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Audit every applied replacement (from the span maps).
    ReviewSanitize {
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Run a command in the real repo, sanitizing its stdout/stderr.
    Sh {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 1..)]
        command: Vec<String>,
    },
    /// Run a command inside a sanitized worktree, sanitizing its output.
    StrictRun {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 1..)]
        command: Vec<String>,
    },
    Sync {
        /// Sync only this repo-relative path (used by agent hooks).
        #[arg(long)]
        path: Option<PathBuf>,
        /// Reset mirror files with pending (or tampered) edits to sanitize(real).
        #[arg(long)]
        force: bool,
    },
    /// Embed the sanitized mirror into the local vector index (incremental).
    EmbedIndex,
    /// Semantic (embedding) search over the sanitized mirror.
    SemanticSearch {
        query: String,
        /// Number of top-scoring chunks to return.
        #[arg(long, default_value_t = 10)]
        k: usize,
    },
    Verify,
    Doctor {
        #[arg(long)]
        agent: Option<Agent>,
    },
    InstallHooks {
        #[arg(long)]
        agent: Agent,
        /// Replace files even when the existing config cannot be merged.
        #[arg(long)]
        force: bool,
    },
    /// Remove code-sanity hooks, preserving foreign configuration.
    UninstallHooks {
        #[arg(long)]
        agent: Agent,
    },
    Serve {
        #[arg(long)]
        once: bool,
    },
}

#[derive(Debug, Clone, ValueEnum)]
enum Agent {
    Codex,
    Claude,
    Opencode,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let root = cli.root.canonicalize().unwrap_or(cli.root);
    crate::logging::init(&crate::config::Layout::new(&root), cli.verbose);

    match dispatch(cli.command, &root) {
        Ok(()) => Ok(()),
        Err(err) => {
            // Dedicated exit codes: 2 = patch conflict (real files untouched),
            // 3 = workspace broken (verify failed). Everything else is 1.
            if err.downcast_ref::<crate::patch::ConflictError>().is_some() {
                eprintln!("{err:#}");
                std::process::exit(2);
            }
            if let Some(failed) = err.downcast_ref::<crate::verify::VerifyFailed>() {
                eprint!("{failed}");
                std::process::exit(3);
            }
            Err(err)
        }
    }
}

fn dispatch(command: Command, root: &std::path::Path) -> Result<()> {
    let root = root.to_path_buf();
    match command {
        Command::Init => {
            let layout = init_workspace(&root)?;
            println!("initialized {}", layout.state_dir.display());
        }
        Command::Index => {
            let started = std::time::Instant::now();
            let report = index_workspace(&root)?;
            println!(
                "indexed={} unchanged={} skipped={} removed={} pending={} elapsed={}",
                report.indexed,
                report.unchanged,
                report.skipped,
                report.removed,
                report.pending,
                format_elapsed(started.elapsed())
            );
        }
        Command::Read { path } => {
            print!("{}", read_sanitized_file(&root, &path)?);
        }
        Command::Search {
            query,
            glob,
            max_results,
        } => {
            let (hits, truncated) =
                crate::search::search_mirror_limited(&root, &query, glob.as_deref(), max_results)?;
            for hit in &hits {
                println!(
                    "{}:{}:{}:{}",
                    hit.rel_path, hit.line, hit.column, hit.line_text
                );
            }
            if truncated {
                eprintln!(
                    "[truncated to {} results; refine the query or raise --max-results]",
                    hits.len()
                );
            }
        }
        Command::ApplyPatch {
            patch,
            agent,
            session_id,
        } => {
            let patch_text = read_optional_file_or_stdin(patch.as_ref())?;
            let started = std::time::Instant::now();
            let report = apply_patch_text_with_options(
                &root,
                &patch_text,
                ApplyOptions { session_id, agent },
            )?;
            println!(
                "applied files={} journal={} elapsed={}",
                report.files.join(","),
                report.journal_path.display(),
                format_elapsed(started.elapsed())
            );
        }
        Command::Write {
            path,
            sanitized_content,
        } => {
            let content = read_optional_file_or_stdin(sanitized_content.as_ref())?;
            let report = write_sanitized_content(&root, &path, &content)?;
            println!(
                "wrote files={} journal={}",
                report.files.join(","),
                report.journal_path.display()
            );
        }
        Command::Rename {
            path,
            from,
            to,
            agent,
            session_id,
        } => {
            let report =
                rename_alias(&root, &path, &from, &to, ApplyOptions { session_id, agent })?;
            println!(
                "renamed real={} -> {} occurrences={} sanitized_now={} files={} journal={}",
                report.real_from,
                to,
                report.occurrences,
                report.sanitized_to,
                report.apply.files.join(","),
                report.apply.journal_path.display()
            );
        }
        Command::ProjectEdit {
            path,
            agent,
            session_id,
        } => {
            let report = project_mirror_edit(&root, &path, ApplyOptions { session_id, agent })?;
            println!(
                "projected files={} journal={}",
                report.files.join(","),
                report.journal_path.display()
            );
        }
        Command::Recover { rollback, force } => {
            let report = recover_workspace(&root, rollback, force)?;
            println!(
                "recovered entries={} rolled_back={} conflicts={} temp_files_removed={}",
                report.recovered.len(),
                report.rolled_back,
                report.conflicts.len(),
                report.temp_files_removed
            );
            for conflict in &report.conflicts {
                eprintln!("conflict: {conflict}");
            }
        }
        Command::Mode => {
            let layout = crate::config::Layout::new(&root);
            let config = crate::config::Config::load_or_default(&layout)?;
            let mode = match config.mode {
                crate::config::Mode::Soft => "soft",
                crate::config::Mode::Guided => "guided",
                crate::config::Mode::Strict => "strict",
            };
            println!("{mode}");
        }
        Command::ProposeSanitize {
            path,
            allow_provider_command,
            allow_provider_endpoint,
        } => {
            let report = crate::proposal::propose_sanitize(
                &root,
                path.as_deref(),
                crate::proposal::ProviderAllow {
                    command: allow_provider_command,
                    endpoint: allow_provider_endpoint,
                },
            )?;
            println!(
                "proposed={} queued={} rejected={}",
                report.proposed,
                report.queued,
                report.rejected.len()
            );
            for rejected in &report.rejected {
                println!("rejected: {rejected}");
            }
        }
        Command::Review {
            approve,
            reject,
            all,
        } => {
            if let Some(id) = approve {
                let item = crate::proposal::resolve_review(&root, &id, true)?;
                println!(
                    "approved {} {} -> {} (file {})",
                    item.id, item.proposal.original_text, item.proposal.sanitized_text, item.file
                );
            } else if let Some(id) = reject {
                let item = crate::proposal::resolve_review(&root, &id, false)?;
                println!("rejected {}", item.id);
            } else {
                let items = crate::proposal::list_review(&root, all)?;
                if items.is_empty() {
                    println!("review queue is empty");
                }
                for item in items {
                    println!(
                        "{}\t{:?}\t{}\t{} -> {}\t[{}]\t{}",
                        item.id,
                        item.status,
                        item.file,
                        item.proposal.original_text,
                        item.proposal.sanitized_text,
                        item.flag,
                        item.proposal.category
                    );
                }
            }
        }
        Command::ReviewSanitize { path } => {
            let rows = crate::proposal::audit_replacements(&root, path.as_deref())?;
            println!("replacements={}", rows.len());
            for row in rows {
                println!(
                    "{}:{}\t{}\t{} -> {}\t[{}]\tconf={:.2}",
                    row.file,
                    row.original_line,
                    row.category,
                    row.original_text,
                    row.sanitized_text,
                    row.policy_source,
                    row.confidence
                );
            }
        }
        Command::Sh { command } => {
            let code = crate::strict::run(&root, &command, false)?;
            std::process::exit(code);
        }
        Command::StrictRun { command } => {
            let code = crate::strict::run(&root, &command, true)?;
            std::process::exit(code);
        }
        Command::Sync { path, force } => {
            let started = std::time::Instant::now();
            let report = match (path, force) {
                (Some(path), false) => crate::index::sync_single_file(&root, &path)?,
                (Some(path), true) => {
                    crate::index::index_single_file(&root, &path)?;
                    crate::index::IndexReport {
                        indexed: 1,
                        ..Default::default()
                    }
                }
                (None, false) => index_workspace(&root)?,
                (None, true) => crate::index::index_workspace_force(&root)?,
            };
            println!(
                "synced indexed={} unchanged={} skipped={} removed={} pending={} stashed={} elapsed={}",
                report.indexed,
                report.unchanged,
                report.skipped,
                report.removed,
                report.pending,
                report.stashed.len(),
                format_elapsed(started.elapsed())
            );
            for stash in &report.stashed {
                eprintln!("stashed pending mirror edit: {stash}");
            }
        }
        Command::EmbedIndex => {
            let started = std::time::Instant::now();
            let report = crate::embed::embed_index(&root)?;
            println!(
                "embedded={} unchanged={} removed={} stale={} chunks={} elapsed={}",
                report.embedded,
                report.unchanged,
                report.removed,
                report.stale,
                report.chunks,
                format_elapsed(started.elapsed())
            );
        }
        Command::SemanticSearch { query, k } => {
            let started = std::time::Instant::now();
            let hits = crate::embed::semantic_search(&root, &query, k)?;
            for hit in &hits {
                println!(
                    "{}:{}-{}\t{:.3}\t{}",
                    hit.rel_path, hit.start_line, hit.end_line, hit.score, hit.preview
                );
            }
            // Stdout stays machine-parseable result lines; the summary goes to
            // stderr (most of the latency is the query embedding HTTP call).
            eprintln!(
                "[{} hit(s) elapsed={}]",
                hits.len(),
                format_elapsed(started.elapsed())
            );
        }
        Command::Verify => {
            let report = verify_workspace(&root)?;
            println!("verified tracked_files={}", report.checked);
        }
        Command::Doctor { agent } => {
            doctor(&root, agent)?;
        }
        Command::InstallHooks { agent, force } => {
            install_hooks(&root, agent, force)?;
        }
        Command::UninstallHooks { agent } => {
            uninstall_hooks(&root, agent)?;
        }
        Command::Serve { once } => {
            if once {
                // Inspection mode: print the tool manifest and exit without
                // blocking on stdio, so callers can verify the server wiring.
                println!("{}", crate::mcp::tools_manifest_json());
            } else {
                crate::mcp::serve_stdio(&root)?;
            }
        }
    }

    Ok(())
}

/// Human-scale wall time for report lines: `840ms` below a second, `1.2s`
/// above.
fn format_elapsed(elapsed: std::time::Duration) -> String {
    if elapsed < std::time::Duration::from_secs(1) {
        format!("{}ms", elapsed.as_millis())
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    }
}

fn read_optional_file_or_stdin(path: Option<&PathBuf>) -> Result<String> {
    if let Some(path) = path {
        return fs::read_to_string(path).with_context(|| format!("read {}", path.display()));
    }
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .context("read stdin")?;
    Ok(input)
}

fn doctor(root: &std::path::Path, agent: Option<Agent>) -> Result<()> {
    let layout = crate::config::Layout::new(root);
    println!("root={}", root.display());
    println!(
        "state_dir={} exists={}",
        layout.state_dir.display(),
        layout.state_dir.exists()
    );
    println!(
        "config={} exists={}",
        layout.config_path.display(),
        layout.config_path.exists()
    );
    println!(
        "db={} exists={}",
        layout.db_path.display(),
        layout.db_path.exists()
    );
    println!(
        "mirror={} exists={}",
        layout.mirror_dir.display(),
        layout.mirror_dir.exists()
    );
    println!(
        "maps={} exists={}",
        layout.maps_dir.display(),
        layout.maps_dir.exists()
    );
    match agent {
        Some(Agent::Codex) => {
            let hooks = root.join(".codex/hooks.json");
            let pre = root.join(".codex/hooks/pre_tool_use.py");
            let post = root.join(".codex/hooks/post_tool_use.py");
            let installed = hooks.exists()
                && pre.exists()
                && post.exists()
                && fs::read_to_string(&pre)
                    .map(|body| body.contains("permissionDecision"))
                    .unwrap_or(false);
            println!(
                "codex hooks.json={} exists={}",
                hooks.display(),
                hooks.exists()
            );
            println!("codex pre_tool_use.py exists={}", pre.exists());
            println!("codex post_tool_use.py exists={}", post.exists());
            println!(
                "codex hooks installed={} (run `code-sanity install-hooks --agent codex`)",
                installed
            );
            println!(
                "codex hooks deny raw edits in strict and steer to code_sanity MCP tools; PreToolUse is a guardrail, not a full enforcement boundary"
            );
        }
        Some(Agent::Claude) => {
            let settings = root.join(".claude/settings.json");
            let pre = root.join(".claude/hooks/pre_tool_use.py");
            let post = root.join(".claude/hooks/post_tool_use.py");
            let session = root.join(".claude/hooks/session_start.py");
            let installed = settings.exists()
                && pre.exists()
                && fs::read_to_string(&pre)
                    .map(|body| body.contains("permissionDecision"))
                    .unwrap_or(false);
            println!(
                "claude settings.json={} exists={}",
                settings.display(),
                settings.exists()
            );
            println!("claude pre_tool_use.py exists={}", pre.exists());
            println!("claude post_tool_use.py exists={}", post.exists());
            println!("claude session_start.py exists={}", session.exists());
            println!(
                "claude hooks installed={} (run `code-sanity install-hooks --agent claude`)",
                installed
            );
            println!(
                "claude hooks guard raw Read/Edit/Write in strict and steer to the code-sanity MCP server; hooks are a guardrail, not a hard boundary"
            );
        }
        Some(Agent::Opencode) => {
            let plugin = root.join(".opencode/plugins/code-sanity.ts");
            let pkg = root.join(".opencode/package.json");
            let plugin_ok = plugin.exists();
            let installed = plugin_ok
                && fs::read_to_string(&plugin)
                    .map(|body| body.contains("project-edit"))
                    .unwrap_or(false);
            println!("opencode plugin={} exists={}", plugin.display(), plugin_ok);
            println!(
                "opencode package.json={} exists={}",
                pkg.display(),
                pkg.exists()
            );
            println!(
                "opencode plugin installed={} (run `code-sanity install-hooks --agent opencode`)",
                installed
            );
            println!(
                "opencode bridges mirror edits via `code-sanity project-edit`; hooks are guardrails, not a hard boundary"
            );
        }
        None => {
            println!("agents: codex, claude, opencode");
        }
    }
    Ok(())
}

/// Write `content` to `path`, keeping a `.bak` copy of any existing different
/// content so a user customization is never silently destroyed.
fn write_with_backup(path: &std::path::Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if let Ok(existing) = fs::read_to_string(path) {
        if existing == content {
            return Ok(());
        }
        let backup = path.with_extension(format!(
            "{}bak",
            path.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| format!("{ext}."))
                .unwrap_or_default()
        ));
        fs::write(&backup, existing).with_context(|| format!("write {}", backup.display()))?;
    }
    fs::write(path, content).with_context(|| format!("write {}", path.display()))
}

/// Merge our hook entries into an existing hooks JSON config, preserving every
/// foreign key and hook. Returns the merged document.
fn merge_hooks_json(
    path: &std::path::Path,
    ours_raw: &str,
    force: bool,
) -> Result<serde_json::Value> {
    let ours: serde_json::Value = serde_json::from_str(ours_raw).context("parse builtin hooks")?;
    let mut existing = match fs::read_to_string(path) {
        Err(_) => serde_json::json!({}),
        Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(value) => value,
            Err(err) if force => {
                eprintln!(
                    "warning: {} is not valid JSON ({err}); replacing (backup kept)",
                    path.display()
                );
                serde_json::json!({})
            }
            Err(err) => anyhow::bail!(
                "{} is not valid JSON ({err}); fix it or rerun with --force",
                path.display()
            ),
        },
    };

    if !existing.is_object() {
        anyhow::bail!("{} does not contain a JSON object", path.display());
    }
    let root_object = existing.as_object_mut().expect("checked object");
    let hooks_slot = root_object
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    if !hooks_slot.is_object() {
        anyhow::bail!("{}: \"hooks\" is not an object", path.display());
    }
    let hooks = hooks_slot.as_object_mut().expect("checked object");

    for (event, our_entries) in ours["hooks"].as_object().expect("builtin hooks object") {
        let slot = hooks
            .entry(event.clone())
            .or_insert_with(|| serde_json::json!([]));
        let Some(entries) = slot.as_array_mut() else {
            anyhow::bail!("{}: hooks.{event} is not an array", path.display());
        };
        for our_entry in our_entries.as_array().expect("builtin hook entries") {
            if !entries.iter().any(|entry| entry == our_entry) {
                entries.push(our_entry.clone());
            }
        }
    }
    Ok(existing)
}

/// Remove our hook entries from an existing hooks JSON config, leaving all
/// foreign configuration in place. Returns None if the file does not exist or
/// is not valid JSON.
fn strip_hooks_json(path: &std::path::Path, ours_raw: &str) -> Result<Option<serde_json::Value>> {
    let ours: serde_json::Value = serde_json::from_str(ours_raw).context("parse builtin hooks")?;
    let Ok(raw) = fs::read_to_string(path) else {
        return Ok(None);
    };
    let Ok(mut existing) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Ok(None);
    };
    let Some(hooks) = existing
        .get_mut("hooks")
        .and_then(|value| value.as_object_mut())
    else {
        return Ok(Some(existing));
    };
    for (event, our_entries) in ours["hooks"].as_object().expect("builtin hooks object") {
        if let Some(entries) = hooks.get_mut(event).and_then(|value| value.as_array_mut()) {
            entries.retain(|entry| {
                !our_entries
                    .as_array()
                    .expect("builtin hook entries")
                    .iter()
                    .any(|ours| ours == entry)
            });
        }
    }
    hooks.retain(|_, value| value.as_array().is_none_or(|entries| !entries.is_empty()));
    Ok(Some(existing))
}

fn install_hooks(root: &std::path::Path, agent: Agent, force: bool) -> Result<()> {
    let installed = format!("{agent:?}");
    match agent {
        Agent::Codex => {
            let config_path = root.join(".codex/hooks.json");
            let merged = merge_hooks_json(&config_path, CODEX_HOOKS_JSON, force)?;
            write_with_backup(
                &config_path,
                &format!("{}\n", serde_json::to_string_pretty(&merged)?),
            )?;
            let dir = root.join(".codex/hooks");
            write_with_backup(&dir.join("pre_tool_use.py"), CODEX_PRE_TOOL_USE_PY)?;
            write_with_backup(&dir.join("post_tool_use.py"), POST_TOOL_USE_PY)?;
        }
        Agent::Claude => {
            let config_path = root.join(".claude/settings.json");
            let merged = merge_hooks_json(&config_path, CLAUDE_SETTINGS_JSON, force)?;
            write_with_backup(
                &config_path,
                &format!("{}\n", serde_json::to_string_pretty(&merged)?),
            )?;
            let dir = root.join(".claude/hooks");
            write_with_backup(&dir.join("pre_tool_use.py"), CLAUDE_PRE_TOOL_USE_PY)?;
            write_with_backup(&dir.join("post_tool_use.py"), POST_TOOL_USE_PY)?;
            write_with_backup(&dir.join("session_start.py"), CLAUDE_SESSION_START_PY)?;
        }
        Agent::Opencode => {
            let dir = root.join(".opencode/plugins");
            write_with_backup(&dir.join("code-sanity.ts"), OPENCODE_PLUGIN_TS)?;
            write_with_backup(&root.join(".opencode/package.json"), OPENCODE_PACKAGE_JSON)?;
        }
    }
    println!("installed hooks for {installed}");
    Ok(())
}

fn uninstall_hooks(root: &std::path::Path, agent: Agent) -> Result<()> {
    let name = format!("{agent:?}");
    let remove_if_present = |path: &std::path::Path| -> Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
        }
    };
    match agent {
        Agent::Codex => {
            let config_path = root.join(".codex/hooks.json");
            if let Some(stripped) = strip_hooks_json(&config_path, CODEX_HOOKS_JSON)? {
                write_with_backup(
                    &config_path,
                    &format!("{}\n", serde_json::to_string_pretty(&stripped)?),
                )?;
            }
            remove_if_present(&root.join(".codex/hooks/pre_tool_use.py"))?;
            remove_if_present(&root.join(".codex/hooks/post_tool_use.py"))?;
        }
        Agent::Claude => {
            let config_path = root.join(".claude/settings.json");
            if let Some(stripped) = strip_hooks_json(&config_path, CLAUDE_SETTINGS_JSON)? {
                write_with_backup(
                    &config_path,
                    &format!("{}\n", serde_json::to_string_pretty(&stripped)?),
                )?;
            }
            remove_if_present(&root.join(".claude/hooks/pre_tool_use.py"))?;
            remove_if_present(&root.join(".claude/hooks/post_tool_use.py"))?;
            remove_if_present(&root.join(".claude/hooks/session_start.py"))?;
        }
        Agent::Opencode => {
            remove_if_present(&root.join(".opencode/plugins/code-sanity.ts"))?;
            // package.json is removed only when it is exactly ours.
            let pkg = root.join(".opencode/package.json");
            if fs::read_to_string(&pkg).is_ok_and(|body| body == OPENCODE_PACKAGE_JSON) {
                remove_if_present(&pkg)?;
            }
        }
    }
    println!("uninstalled hooks for {name}");
    Ok(())
}

const OPENCODE_PACKAGE_JSON: &str = "{\n  \"name\": \"code-sanity-opencode-plugin\",\n  \"version\": \"0.1.0\",\n  \"private\": true,\n  \"type\": \"module\"\n}\n";

/// Generated opencode plugin. Redirects reads/search to the sanitized mirror,
/// bridges mirror edits back to the real repo via `code-sanity project-edit`,
/// and blocks raw real-repo edits in strict mode. Hooks are a guardrail, not a
/// hard boundary — strict isolation still needs the agent inside the mirror.
const OPENCODE_PLUGIN_TS: &str = r#"// code-sanity opencode plugin (generated by `code-sanity install-hooks --agent opencode`).
//
// - read/grep/glob/list are redirected to the sanitized mirror (.code-sanity/mirror)
// - edit/write land on the mirror, then are back-projected to the real repo via
//   `code-sanity project-edit` (span-aware, conflict-safe)
// - strict mode blocks edits that target the real repo instead of the mirror
//
// Hooks are a guardrail, not a hard boundary. Reads via bash/other tools are not
// intercepted; for hard isolation run the agent inside the mirror or an overlay.
//
// Requires the `code-sanity` binary on PATH, or set CODE_SANITY_BIN.
import { join, relative, isAbsolute } from "node:path"
import { appendFileSync, mkdirSync } from "node:fs"

const BIN = process.env.CODE_SANITY_BIN || "code-sanity"
const MIRROR_REL = ".code-sanity/mirror"
const READ_TOOLS = new Set(["read", "grep", "glob", "list"])
const EDIT_TOOLS = new Set(["edit", "write", "patch"])

export const CodeSanityPlugin = async ({ directory, $ }: any) => {
  const root = directory
  const mirrorRoot = join(root, MIRROR_REL)

  // Failures are logged, never silently swallowed.
  const log = (message: string) => {
    try {
      const dir = join(root, ".code-sanity", "logs")
      mkdirSync(dir, { recursive: true })
      appendFileSync(
        join(dir, "hooks.log"),
        `${new Date().toISOString()} opencode: ${message}\n`,
      )
    } catch (err) {
      console.error(`code-sanity plugin: ${message} (log failed: ${err})`)
    }
  }

  const run = async (args: string[]) => {
    try {
      const out = await $`${BIN} --root ${root} ${args}`.quiet()
      return out.stdout.toString().trim()
    } catch (e: any) {
      log(`${args.join(" ")} failed: ${e?.stderr?.toString?.() ?? e}`)
      return ""
    }
  }

  // Keep the mirror fresh at session start (best-effort).
  await run(["index"])
  const mode = (await run(["mode"])) || "guided"

  const toRel = (p?: string) => {
    if (!p) return undefined
    const abs = isAbsolute(p) ? p : join(root, p)
    if (abs.startsWith(mirrorRoot)) return relative(mirrorRoot, abs)
    if (abs.startsWith(root)) return relative(root, abs)
    return undefined
  }
  const toMirror = (p?: string) => {
    const rel = toRel(p)
    return rel ? join(mirrorRoot, rel) : p
  }
  const inMirror = (p?: string) => {
    if (!p) return false
    const abs = isAbsolute(p) ? p : join(root, p)
    return abs.startsWith(mirrorRoot)
  }
  const redirect = (args: any) => {
    if (args?.filePath) args.filePath = toMirror(args.filePath)
    if (args?.path) args.path = toMirror(args.path)
  }

  return {
    "tool.execute.before": async (input: any, output: any) => {
      const tool = input?.tool
      const args = output?.args
      if (!tool || !args) return
      if (READ_TOOLS.has(tool)) {
        redirect(args)
        return
      }
      if (EDIT_TOOLS.has(tool)) {
        const target = args.filePath || args.path
        if (mode === "strict" && !inMirror(target)) {
          throw new Error(
            "code-sanity strict mode: edit the sanitized mirror (" +
              MIRROR_REL +
              ") or use `code-sanity apply-patch`; raw real-repo edits are blocked.",
          )
        }
        redirect(args)
      }
    },
    "tool.execute.after": async (input: any, output: any) => {
      const tool = input?.tool
      if (!EDIT_TOOLS.has(tool)) return
      const args = input?.args || output?.args || {}
      const rel = toRel(args.filePath || args.path)
      if (!rel) return
      // Mirror edits are back-projected first, then only the touched path is
      // re-synced; a full-repo sync here would clobber concurrent work.
      await run(["project-edit", "--path", rel, "--agent", "opencode"])
      await run(["sync", "--path", rel])
    },
    "file.edited": async (input: any) => {
      const rel = toRel(input?.file || input?.path)
      if (rel) await run(["sync", "--path", rel])
    },
  }
}

export default CodeSanityPlugin
"#;

const CODEX_HOOKS_JSON: &str = r##"{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "edit|write|patch|apply_patch|bash|shell|Edit|Write|MultiEdit|NotebookEdit",
        "hooks": [
          { "type": "command", "command": "python3 .codex/hooks/pre_tool_use.py" }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "edit|write|patch|apply_patch|Edit|Write|MultiEdit|NotebookEdit",
        "hooks": [
          { "type": "command", "command": "python3 .codex/hooks/post_tool_use.py" }
        ]
      }
    ]
  }
}
"##;

/// Codex PreToolUse guard. Denies raw real-repo edits in strict, nudges toward
/// the code_sanity MCP tools in guided, and best-effort redirects obvious shell
/// reads to the sanitized mirror. Guardrail only: PreToolUse does not intercept
/// every shell path, so strict isolation still needs the mirror/overlay.
const CODEX_PRE_TOOL_USE_PY: &str = r##"#!/usr/bin/env python3
# code-sanity Codex PreToolUse guard (generated by `code-sanity install-hooks --agent codex`).
# Guardrail, not a hard boundary: PreToolUse does not intercept every shell path.
import json, os, re, sys

MIRROR = ".code-sanity/mirror"


def read_mode(cwd):
    path = os.path.join(cwd or ".", ".code-sanity", "config.toml")
    try:
        with open(path, "r", encoding="utf-8") as handle:
            for line in handle:
                match = re.match(r'\s*mode\s*=\s*"(\w+)"', line)
                if match:
                    return match.group(1)
    except OSError:
        pass
    return "guided"


def rewrite_read(cmd):
    match = re.match(r"^\s*(cat|nl|head|tail)\s+(\S+)\s*$", cmd or "")
    if not match:
        return None
    path = match.group(2)
    if path.startswith("/") or path.startswith("-") or ".." in path or MIRROR in path:
        return None
    return "code-sanity read " + path


def log_line(cwd, message):
    import datetime
    line = "%s pre_tool_use: %s\n" % (
        datetime.datetime.now(datetime.timezone.utc).isoformat(),
        message,
    )
    try:
        log_dir = os.path.join(cwd, ".code-sanity", "logs")
        os.makedirs(log_dir, exist_ok=True)
        with open(os.path.join(log_dir, "hooks.log"), "a", encoding="utf-8") as handle:
            handle.write(line)
    except OSError:
        sys.stderr.write("code-sanity hook: " + line)


def main():
    try:
        payload = json.load(sys.stdin)
    except Exception as exc:
        log_line(os.getcwd(), "invalid hook payload: %r" % (exc,))
        print(json.dumps({"permissionDecision": "allow"}))
        return
    cwd = payload.get("cwd") or os.getcwd()
    mode = read_mode(cwd)
    tool = (payload.get("tool_name") or payload.get("tool") or "")
    tinput = payload.get("tool_input") or payload.get("input") or {}
    lname = tool.lower()

    # code_sanity MCP tools are always allowed.
    if "code_sanity" in lname or "code-sanity" in lname or lname.startswith("mcp__code"):
        print(json.dumps({"permissionDecision": "allow"}))
        return

    decision = {"permissionDecision": "allow"}
    is_edit = ("apply_patch" in lname) or lname in ("edit", "write", "patch")
    if is_edit:
        path = str(tinput.get("file_path") or tinput.get("path") or "").replace("\\", "/")
        edits_mirror = MIRROR in path
        if mode == "strict" and not edits_mirror:
            decision = {
                "permissionDecision": "deny",
                "message": "code-sanity strict mode: edit via the code_sanity MCP apply_patch tool "
                "or the sanitized mirror (" + MIRROR + "); raw real-repo edits are blocked.",
            }
        elif mode == "guided" and not edits_mirror:
            decision = {
                "permissionDecision": "allow",
                "message": "code-sanity: prefer code_sanity apply_patch so edits round-trip through the sanitized bridge.",
            }
    elif lname in ("bash", "shell") and mode != "soft":
        rewritten = rewrite_read(tinput.get("command", ""))
        if rewritten:
            decision = {
                "permissionDecision": "allow",
                "updatedInput": {"command": rewritten},
                "message": "code-sanity: redirected read to the sanitized mirror.",
            }

    print(json.dumps(decision))


if __name__ == "__main__":
    main()
"##;

/// Shared PostToolUse hook (Codex and Claude payloads are shape-compatible).
/// Mirrors edited in place are back-projected first (`project-edit`), then the
/// touched path is synced; only the changed path is reindexed. Failures are
/// logged to `.code-sanity/logs/hooks.log`, never swallowed.
const POST_TOOL_USE_PY: &str = r##"#!/usr/bin/env python3
# code-sanity PostToolUse hook (generated by `code-sanity install-hooks`).
# Back-projects mirror edits (project-edit), then syncs only the edited path.
import datetime
import json
import os
import subprocess
import sys

MIRROR = ".code-sanity/mirror"


def log_line(cwd, message):
    line = "%s post_tool_use: %s\n" % (
        datetime.datetime.now(datetime.timezone.utc).isoformat(),
        message,
    )
    try:
        log_dir = os.path.join(cwd, ".code-sanity", "logs")
        os.makedirs(log_dir, exist_ok=True)
        with open(os.path.join(log_dir, "hooks.log"), "a", encoding="utf-8") as handle:
            handle.write(line)
    except OSError:
        sys.stderr.write("code-sanity hook: " + line)


def rel_paths(payload, cwd):
    tool_input = payload.get("tool_input") or payload.get("input") or {}
    raw = tool_input.get("file_path") or tool_input.get("path") or ""
    raw = str(raw).replace("\\", "/")
    if not raw:
        return []
    if os.path.isabs(raw):
        try:
            raw = os.path.relpath(raw, cwd).replace("\\", "/")
        except ValueError:
            return []
    if raw.startswith(".."):
        return []
    return [raw]


def main():
    cwd = os.getcwd()
    try:
        payload = json.load(sys.stdin)
    except Exception as exc:  # log, never silently drop
        log_line(cwd, "invalid hook payload: %r" % (exc,))
        return
    cwd = payload.get("cwd") or cwd
    binary = os.environ.get("CODE_SANITY_BIN", "code-sanity")

    for path in rel_paths(payload, cwd):
        commands = []
        if path.startswith(MIRROR + "/"):
            rel = path[len(MIRROR) + 1 :]
            # Mirror was edited in place: project the edit to the real repo
            # FIRST, then refresh the mirror for that path.
            commands.append(["project-edit", "--path", rel])
            commands.append(["sync", "--path", rel])
        elif path.startswith(".code-sanity/"):
            continue
        else:
            commands.append(["sync", "--path", path])
        for args in commands:
            try:
                proc = subprocess.run(
                    [binary, *args],
                    cwd=cwd,
                    capture_output=True,
                    text=True,
                    timeout=120,
                )
                if proc.returncode != 0:
                    log_line(
                        cwd,
                        "%s failed (%d): %s"
                        % (" ".join(args), proc.returncode, proc.stderr.strip()),
                    )
            except Exception as exc:
                log_line(cwd, "%s error: %r" % (" ".join(args), exc))


if __name__ == "__main__":
    main()
"##;

const CLAUDE_SETTINGS_JSON: &str = r##"{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Read|Edit|Write|MultiEdit|NotebookEdit",
        "hooks": [
          { "type": "command", "command": "python3 .claude/hooks/pre_tool_use.py" }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "Edit|Write|MultiEdit|NotebookEdit",
        "hooks": [
          { "type": "command", "command": "python3 .claude/hooks/post_tool_use.py" }
        ]
      }
    ],
    "SessionStart": [
      {
        "hooks": [
          { "type": "command", "command": "python3 .claude/hooks/session_start.py" }
        ]
      }
    ]
  }
}
"##;

/// Claude Code PreToolUse guard. Denies raw real-repo Read/Edit/Write in strict
/// (guided denies edits) and steers toward the code-sanity MCP server. Emits a
/// deny decision only; allowed tools fall through to normal permission flow.
const CLAUDE_PRE_TOOL_USE_PY: &str = r##"#!/usr/bin/env python3
# code-sanity Claude Code PreToolUse guard (generated by `code-sanity install-hooks --agent claude`).
# Guardrail, not a hard boundary: hooks steer tools but do not transparently
# rewrite every read. For hard isolation run the agent inside the mirror/overlay.
import json, os, re, sys

MIRROR = ".code-sanity/mirror"


def read_mode(cwd):
    path = os.path.join(cwd or ".", ".code-sanity", "config.toml")
    try:
        with open(path, "r", encoding="utf-8") as handle:
            for line in handle:
                match = re.match(r'\s*mode\s*=\s*"(\w+)"', line)
                if match:
                    return match.group(1)
    except OSError:
        pass
    return "guided"


def deny(reason):
    return {
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    }


def log_line(cwd, message):
    import datetime
    line = "%s pre_tool_use: %s\n" % (
        datetime.datetime.now(datetime.timezone.utc).isoformat(),
        message,
    )
    try:
        log_dir = os.path.join(cwd, ".code-sanity", "logs")
        os.makedirs(log_dir, exist_ok=True)
        with open(os.path.join(log_dir, "hooks.log"), "a", encoding="utf-8") as handle:
            handle.write(line)
    except OSError:
        sys.stderr.write("code-sanity hook: " + line)


def main():
    cwd = os.getcwd()
    try:
        payload = json.load(sys.stdin)
    except Exception as exc:
        log_line(cwd, "invalid hook payload: %r" % (exc,))
        return
    cwd = payload.get("cwd") or cwd
    mode = read_mode(cwd)
    tool = payload.get("tool_name") or ""
    tinput = payload.get("tool_input") or {}
    path = str(tinput.get("file_path") or tinput.get("path") or "").replace("\\", "/")
    edits_mirror = MIRROR in path

    reason_edit = (
        "code-sanity strict mode: use the code-sanity MCP apply_patch tool or edit the "
        "sanitized mirror (" + MIRROR + "); raw real-repo edits are blocked."
    )
    reason_read = (
        "code-sanity strict mode: read via the code-sanity MCP read_file/search tools; "
        "raw reads of the real repo are blocked."
    )

    decision = None
    if tool in ("Edit", "Write", "MultiEdit", "NotebookEdit"):
        if not edits_mirror and mode in ("strict", "guided"):
            decision = deny(reason_edit)
    elif tool == "Read":
        if not edits_mirror and mode == "strict":
            decision = deny(reason_read)

    if decision:
        print(json.dumps(decision))


if __name__ == "__main__":
    main()
"##;

const CLAUDE_SESSION_START_PY: &str = r##"#!/usr/bin/env python3
# code-sanity Claude Code SessionStart: inject guidance to use the code-sanity tools.
import json, sys

CONTEXT = (
    "This repository uses code-sanity: a sanitized mirror is the agent-facing view of the "
    "real code. Prefer the code-sanity MCP tools (read_file, search, list_files, apply_patch, "
    "verify) for reads and edits so changes round-trip through the sanitized bridge. In strict "
    "mode, raw reads/edits of the real repo are blocked; edit the mirror or use apply_patch."
)


def main():
    print(
        json.dumps(
            {
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": CONTEXT,
                }
            }
        )
    )


if __name__ == "__main__":
    main()
"##;
