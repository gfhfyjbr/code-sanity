use crate::index::{index_workspace, init_workspace};
use crate::patch::{
    ApplyOptions, apply_patch_text_with_options, project_mirror_edit, recover_workspace,
    rename_alias, write_sanitized_content,
};
use crate::search::read_sanitized_file;
use crate::verify::verify_workspace;
use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{generate, shells::Zsh};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "code-sanity")]
#[command(version)]
#[command(about = "Sanitized mirror and patch bridge for agent code workflows")]
pub struct Cli {
    /// Workspace root (defaults to the current directory).
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    /// Raise log verbosity (-v: debug to log file, info to stderr; -vv: trace).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Emit exactly one machine-readable JSON object on stdout (stderr and
    /// exit codes are unchanged). Not supported for
    /// sh/strict-run/completions/serve.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

/// The kebab-case clap name of a command, for the `--json` envelope.
fn command_name(command: Option<&Command>) -> &'static str {
    match command {
        None => "tui",
        Some(Command::Init) => "init",
        Some(Command::Index) => "index",
        Some(Command::Read { .. }) => "read",
        Some(Command::ProjectPath { .. }) => "project-path",
        Some(Command::Search { .. }) => "search",
        Some(Command::ApplyPatch { .. }) => "apply-patch",
        Some(Command::Write { .. }) => "write",
        Some(Command::Rename { .. }) => "rename",
        Some(Command::ProjectEdit { .. }) => "project-edit",
        Some(Command::Recover { .. }) => "recover",
        Some(Command::Mode) => "mode",
        Some(Command::ProposeSanitize { .. }) => "propose-sanitize",
        Some(Command::Review { .. }) => "review",
        Some(Command::ReviewSanitize { .. }) => "review-sanitize",
        Some(Command::Sh { .. }) => "sh",
        Some(Command::StrictRun { .. }) => "strict-run",
        Some(Command::Sync { .. }) => "sync",
        Some(Command::EmbedIndex) => "embed-index",
        Some(Command::SemanticSearch { .. }) => "semantic-search",
        Some(Command::WorkspaceSnapshot) => "workspace-snapshot",
        Some(Command::FindCode { .. }) => "find-code",
        Some(Command::ReadCode { .. }) => "read-code",
        Some(Command::EditNode { .. }) => "edit-node",
        Some(Command::RenameSymbol { .. }) => "rename-symbol",
        Some(Command::PreviewTransaction { .. }) => "preview-transaction",
        Some(Command::CommitTransaction { .. }) => "commit-transaction",
        Some(Command::Verify) => "verify",
        Some(Command::Doctor { .. }) => "doctor",
        Some(Command::InstallHooks { .. }) => "install-hooks",
        Some(Command::UninstallHooks { .. }) => "uninstall-hooks",
        Some(Command::Completions { .. }) => "completions",
        Some(Command::Serve { .. }) => "serve",
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create the .code-sanity workspace (config, salt, mirror, db).
    Init,
    /// Build or refresh the sanitized mirror from the real files (incremental).
    Index,
    /// Print a file from the sanitized mirror.
    Read {
        /// Repo-relative path, e.g. src/lib.rs.
        path: PathBuf,
    },
    /// Print the agent-facing sanitized spelling of a repository path.
    ProjectPath {
        /// Real or already-projected repo-relative path.
        path: PathBuf,
    },
    /// Substring-search the sanitized mirror (alias: grep).
    #[command(alias = "grep")]
    Search {
        /// Substring to look for.
        query: String,
        /// Glob filter: without '/' matches file names at any depth (*.rs);
        /// with '/' matches the repo-relative path (src/**/*.rs).
        #[arg(long)]
        glob: Option<String>,
        /// Cap on returned matches (default 200, hard max 1000).
        #[arg(long)]
        max_results: Option<usize>,
    },
    /// Apply a unified diff written against mirror paths to the real repo.
    ApplyPatch {
        /// Patch file (defaults to stdin).
        #[arg(long)]
        patch: Option<PathBuf>,
        /// Agent name recorded in the journal.
        #[arg(long)]
        agent: Option<String>,
        /// Session id recorded in the journal.
        #[arg(long)]
        session_id: Option<String>,
        /// Plan and validate only; report what would change without writing
        /// (conflicts still exit 2 and record a conflict journal entry).
        #[arg(long)]
        dry_run: bool,
    },
    /// Replace one mirror file's content and project it to the real repo.
    Write {
        /// Repo-relative target path.
        #[arg(long)]
        path: PathBuf,
        /// Content file (defaults to stdin).
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
        /// Maximum concurrent provider requests (default from config, normally 4).
        #[arg(short = 'j', long, value_parser = parse_propose_jobs)]
        jobs: Option<usize>,
        /// Disable live progress on stderr.
        #[arg(long)]
        no_progress: bool,
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
    /// Exits with the wrapped command's code verbatim (the 2/3/64 exit-code
    /// contract does not apply here).
    Sh {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 1..)]
        command: Vec<String>,
    },
    /// Run a command inside a sanitized worktree, sanitizing its output.
    /// Exits with the wrapped command's code verbatim (the 2/3/64 exit-code
    /// contract does not apply here).
    StrictRun {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 1..)]
        command: Vec<String>,
    },
    /// Reconcile the mirror with the real files (one path or the whole repo).
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
    /// Show the revision and counts of the AST/semantic v2 index.
    WorkspaceSnapshot,
    /// Find stable semantic symbols by name or qualified name.
    FindCode {
        query: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Read one AST-projected document with stable IDs and capabilities.
    ReadCode { path: PathBuf },
    /// Preview an edit anchored to one AST node.
    EditNode {
        #[arg(long)]
        node_id: String,
        #[arg(long)]
        replacement: String,
        #[arg(long)]
        expected_revision: u64,
    },
    /// Preview a compiler/LSP-backed semantic rename.
    RenameSymbol {
        #[arg(long)]
        symbol_id: String,
        #[arg(long)]
        new_name: String,
        #[arg(long)]
        expected_revision: u64,
    },
    /// Preview multiple structured intents from a JSON file or stdin.
    PreviewTransaction {
        #[arg(long)]
        intents: Option<PathBuf>,
        #[arg(long)]
        expected_revision: u64,
    },
    /// Commit a previously previewed semantic transaction.
    CommitTransaction {
        transaction_id: String,
        #[arg(long)]
        expected_revision: u64,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Check mirror/map/db consistency and scan for term leaks (exit 3 on failure).
    Verify,
    /// Print workspace status and agent hook installation state.
    Doctor {
        /// Agent whose hooks to inspect.
        #[arg(long)]
        agent: Option<Agent>,
    },
    /// Generate agent hooks that route reads and edits through the mirror.
    InstallHooks {
        /// Agent to install hooks for.
        #[arg(long)]
        agent: Agent,
        /// Replace files even when the existing config cannot be merged.
        #[arg(long)]
        force: bool,
    },
    /// Remove code-sanity hooks, preserving foreign configuration.
    UninstallHooks {
        /// Agent to remove hooks from.
        #[arg(long)]
        agent: Agent,
    },
    /// Generate shell completion definitions on stdout.
    Completions {
        #[arg(value_enum)]
        shell: CompletionShell,
    },
    /// Run the MCP server over stdio.
    Serve {
        /// Print the tool manifest and exit (wiring check).
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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CompletionShell {
    Zsh,
}

fn write_completions(shell: CompletionShell) -> Result<()> {
    let mut command = Cli::command();
    let name = command.get_name().to_string();
    let mut output = Vec::new();
    match shell {
        CompletionShell::Zsh => generate(Zsh, &mut command, name, &mut output),
    }
    io::stdout()
        .write_all(&output)
        .context("write shell completions")
}

pub fn run() -> Result<()> {
    // Usage errors exit 64 (EX_USAGE), NOT clap's default of 2: 2 is the
    // documented "patch conflict" contract agents script against, and a typo
    // in the flags must not read as a conflict.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            use clap::error::ErrorKind;
            let code = match err.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => 0,
                _ => 64,
            };
            let _ = err.print(); // clap routes help to stdout, errors to stderr
            std::process::exit(code);
        }
    };
    // Completion generation is workspace-independent. Keep it ahead of root
    // canonicalization, dotenv loading, and logging so installers can invoke
    // it reliably from any directory.
    if let Some(Command::Completions { shell }) = cli.command.as_ref() {
        if cli.json {
            eprintln!("--json is not supported for completions: stdout is the shell script");
            std::process::exit(64);
        }
        return write_completions(*shell);
    }
    let out = if cli.json {
        crate::output::Output::Json
    } else {
        crate::output::Output::Human
    };
    let name = command_name(cli.command.as_ref());
    // sh/strict-run hand stdout and the exit code to the wrapped command, and
    // serve's stdout is the MCP protocol stream (or the --once manifest);
    // wrapping either would corrupt the stream, so a harness that sets --json
    // globally must hear a loud refusal, not trust garbage.
    if out.is_json()
        && matches!(
            cli.command,
            Some(Command::Sh { .. })
                | Some(Command::StrictRun { .. })
                | Some(Command::Serve { .. })
        )
    {
        eprintln!(
            "--json is not supported for {name}: stdout already belongs to \
             the wrapped command (sh/strict-run) or the MCP stream (serve)"
        );
        std::process::exit(64);
    }
    let (raw_root, explicit_root) = match cli.root {
        Some(path) => (path, true),
        None => (PathBuf::from("."), false),
    };
    let root = match raw_root.canonicalize() {
        Ok(root) => root,
        // An explicitly given root that does not resolve is a user error to
        // report now — downstream errors ("create /nonexistent/...") mislead.
        // Post-clap, every error must honor the --json envelope contract.
        Err(err) if explicit_root => {
            let message = format!("--root {}: {err}", raw_root.display());
            if out.is_json() {
                crate::output::emit_error(name, "error", &message, serde_json::json!({}));
            }
            anyhow::bail!(message);
        }
        Err(_) => raw_root,
    };
    if let Err(err) = crate::envfile::load_workspace_env(&root) {
        if out.is_json() {
            crate::output::emit_error(name, "error", &format!("{err:#}"), serde_json::json!({}));
        }
        return Err(err);
    }
    // The stdio MCP server logs file-only: hosts capture server stderr into
    // their own logs, and warn/error lines can carry unredacted real terms.
    let stderr_logging = !matches!(cli.command, Some(Command::Serve { once: false }));
    crate::logging::init(
        &crate::config::Layout::new(&root),
        cli.verbose,
        stderr_logging,
    );

    let result = match cli.command {
        Some(command) => dispatch(command, &root, out),
        None if out.is_json() => Err(anyhow::anyhow!("--json requires a subcommand")),
        None if !io::stdin().is_terminal() || !io::stdout().is_terminal() => {
            eprintln!(
                "interactive mode requires a terminal; run code-sanity --help to list commands"
            );
            std::process::exit(64);
        }
        None => crate::tui::run(&root),
    };
    match result {
        Ok(()) => Ok(()),
        Err(err) => {
            // Dedicated exit codes: 2 = patch conflict (real files untouched),
            // 3 = workspace broken (verify failed). Everything else is 1.
            // In JSON mode the error envelope goes to stdout (the machine
            // contract) while the human rendering stays on stderr.
            if let Some(conflict) = err.downcast_ref::<crate::patch::ConflictError>() {
                if out.is_json() {
                    crate::output::emit_error(
                        name,
                        "conflict",
                        &conflict.message,
                        serde_json::json!({
                            "journal_path": conflict.journal_path.display().to_string(),
                        }),
                    );
                }
                eprintln!("{err:#}");
                std::process::exit(2);
            }
            if let Some(failed) = err.downcast_ref::<crate::verify::VerifyFailed>() {
                if out.is_json() {
                    crate::output::emit_error(
                        name,
                        "verify_failed",
                        &format!(
                            "verify failed with {} issue(s)",
                            failed.report.failures.len()
                        ),
                        serde_json::json!({
                            "checked": failed.report.checked,
                            "failures": failed.report.failures,
                        }),
                    );
                }
                eprint!("{failed}");
                std::process::exit(3);
            }
            // A corrupt db.sqlite would otherwise surface as a raw rusqlite
            // error; the database is derived state, so name the remedy.
            let err = if crate::db::is_corruption_error(&err) {
                err.context(
                    "db.sqlite appears corrupt; the database is derived state — \
                     delete .code-sanity/db.sqlite and run `code-sanity sync --force` to \
                     rebuild (pending mirror edits are stashed under \
                     .code-sanity/journal/discarded/)",
                )
            } else {
                err
            };
            if out.is_json() {
                crate::output::emit_error(
                    name,
                    "error",
                    &format!("{err:#}"),
                    serde_json::json!({}),
                );
            }
            Err(err)
        }
    }
}

struct ProposeProgressUi {
    enabled: bool,
    terminal: bool,
    total: usize,
    done: usize,
    total_requests: usize,
    done_requests: usize,
    active: BTreeSet<String>,
    queued: usize,
    duplicates: usize,
    errors: usize,
    chunk_errors: usize,
    skipped: usize,
    last_file: Option<String>,
}

impl ProposeProgressUi {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            terminal: io::stderr().is_terminal(),
            total: 0,
            done: 0,
            total_requests: 0,
            done_requests: 0,
            active: BTreeSet::new(),
            queued: 0,
            duplicates: 0,
            errors: 0,
            chunk_errors: 0,
            skipped: 0,
            last_file: None,
        }
    }

    fn handle(&mut self, event: crate::proposal::ProposeProgress) {
        if !self.enabled {
            return;
        }
        use crate::proposal::{ProposeFileOutcome, ProposeProgress};
        match event {
            ProposeProgress::Started {
                total,
                jobs,
                requests,
            } => {
                self.total = total;
                self.total_requests = requests;
                eprintln!("provider scan: {total} files, {requests} request(s), {jobs} concurrent");
            }
            ProposeProgress::FileStarted { file, chunks, .. } => {
                self.last_file = Some(file.clone());
                if chunks == 0 {
                    self.active.insert(file);
                }
                if self.terminal {
                    self.render_terminal();
                }
            }
            ProposeProgress::ChunkStarted {
                file,
                chunk,
                chunks,
            } => {
                let label = chunk_label(&file, chunk, chunks);
                self.last_file = Some(label.clone());
                self.active.insert(label);
                if self.terminal {
                    self.render_terminal();
                }
            }
            ProposeProgress::ChunkFinished {
                completed,
                total,
                file,
                chunk,
                chunks,
                elapsed_ms,
                outcome,
            } => {
                self.done_requests = completed;
                self.total_requests = total;
                let label = chunk_label(&file, chunk, chunks);
                self.active.remove(&label);
                self.last_file = self.active.iter().next_back().cloned();
                if matches!(outcome, crate::proposal::ProposeChunkOutcome::Error) {
                    self.chunk_errors += 1;
                }
                if self.terminal {
                    self.render_terminal();
                } else {
                    let status = match outcome {
                        crate::proposal::ProposeChunkOutcome::Completed => "ok",
                        crate::proposal::ProposeChunkOutcome::Error => "error",
                    };
                    eprintln!(
                        "[req {completed:>3}/{total}] {status:<5} {:>7}  {}",
                        format_duration(elapsed_ms),
                        display_path(&label)
                    );
                }
            }
            ProposeProgress::FileFinished {
                completed,
                total,
                file,
                elapsed_ms,
                outcome,
                proposed,
                queued,
                duplicates,
                rejected,
            } => {
                self.done = completed;
                self.total = total;
                self.active.remove(&file);
                self.last_file = self.active.iter().next_back().cloned();
                self.queued += queued;
                self.duplicates += duplicates;
                match outcome {
                    ProposeFileOutcome::Error => self.errors += 1,
                    ProposeFileOutcome::Skipped => self.skipped += 1,
                    ProposeFileOutcome::Completed => {}
                }
                if self.terminal {
                    self.render_terminal();
                } else {
                    let status = match outcome {
                        ProposeFileOutcome::Completed => "ok",
                        ProposeFileOutcome::Skipped => "skip",
                        ProposeFileOutcome::Error => "error",
                    };
                    eprintln!(
                        "[{completed:>3}/{total}] {status:<5} {:>7}  proposals={proposed} queued={queued} duplicates={duplicates} rejected={rejected}  {}",
                        format_duration(elapsed_ms),
                        display_path(&file)
                    );
                }
            }
            ProposeProgress::Finished {
                total,
                requests,
                elapsed_ms,
                proposed,
                queued,
                duplicates,
                rejected,
                skipped,
                errors,
            } => {
                if self.terminal {
                    clear_progress_line();
                }
                eprintln!(
                    "provider scan finished: files={total} requests={requests} proposed={proposed} queued={queued} duplicates={duplicates} rejected={rejected} skipped={skipped} errors={errors} elapsed={}",
                    format_duration(elapsed_ms)
                );
            }
        }
    }

    fn render_terminal(&self) {
        const WIDTH: usize = 24;
        let (done, total) = if self.total_requests == 0 {
            (self.done, self.total)
        } else {
            (self.done_requests, self.total_requests)
        };
        let filled = done
            .saturating_mul(WIDTH)
            .checked_div(total)
            .unwrap_or(WIDTH);
        let mut bar = String::with_capacity(WIDTH);
        bar.push_str(&"=".repeat(filled.min(WIDTH)));
        if filled < WIDTH {
            bar.push('>');
            bar.push_str(&" ".repeat(WIDTH - filled - 1));
        }
        let file = self
            .last_file
            .as_deref()
            .map(display_path)
            .unwrap_or_default();
        eprint!(
            "\r\x1b[2K[{bar}] req={done:>3}/{total} files={files_done}/{files_total} active={active} queued={queued} dup={duplicates} errors={errors} {file}",
            files_done = self.done,
            files_total = self.total,
            active = self.active.len(),
            queued = self.queued,
            duplicates = self.duplicates,
            errors = self.errors + self.chunk_errors,
        );
        let _ = io::stderr().flush();
    }
}

fn chunk_label(file: &str, chunk: usize, chunks: usize) -> String {
    if chunks == 1 {
        file.to_string()
    } else {
        format!("{file} [{chunk}/{chunks}]")
    }
}

fn clear_progress_line() {
    eprint!("\r\x1b[2K");
    let _ = io::stderr().flush();
}

fn display_path(path: &str) -> String {
    path.chars().flat_map(char::escape_default).collect()
}

fn format_duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1_000.0)
    }
}

fn parse_propose_jobs(raw: &str) -> std::result::Result<usize, String> {
    let jobs = raw
        .parse::<usize>()
        .map_err(|_| "jobs must be an integer between 1 and 32".to_string())?;
    if (1..=32).contains(&jobs) {
        Ok(jobs)
    } else {
        Err("jobs must be between 1 and 32".to_string())
    }
}

fn dispatch(command: Command, root: &std::path::Path, out: crate::output::Output) -> Result<()> {
    use serde_json::json;
    let root = root.to_path_buf();
    match command {
        Command::Init => {
            let progress =
                crate::presentation::TaskProgress::start("Initializing workspace", !out.is_json());
            let layout = init_workspace(&root)?;
            progress.finish();
            if out.is_json() {
                out.emit(
                    "init",
                    json!({ "state_dir": layout.state_dir.display().to_string() }),
                    None,
                );
            } else {
                let rows = [("State directory", layout.state_dir.display().to_string())];
                if !crate::presentation::summary("Workspace initialized", &rows) {
                    println!("initialized {}", layout.state_dir.display());
                }
            }
        }
        Command::Index => {
            let started = std::time::Instant::now();
            let progress = crate::presentation::TaskProgress::start(
                "Indexing sanitized mirror",
                !out.is_json(),
            );
            let report = index_workspace(&root)?;
            progress.finish();
            for (path, reason) in &report.errors {
                eprintln!("error: {path}: {reason}");
            }
            for error in &report.semantic.errors {
                eprintln!("semantic error: {error}");
            }
            if out.is_json() {
                out.emit(
                    "index",
                    serde_json::to_value(&report)?,
                    Some(started.elapsed().as_millis()),
                );
            } else {
                let rows = [
                    ("Indexed", report.indexed.to_string()),
                    ("Unchanged", report.unchanged.to_string()),
                    ("Skipped", report.skipped.to_string()),
                    ("Removed", report.removed.to_string()),
                    ("Pending edits", report.pending.to_string()),
                    ("Symlinks", report.skipped_symlinks.to_string()),
                    ("Errors", report.errors.len().to_string()),
                    ("AST revision", report.semantic.revision.to_string()),
                    ("AST indexed", report.semantic.indexed.to_string()),
                    ("AST unchanged", report.semantic.unchanged.to_string()),
                    ("AST read-only", report.semantic.read_only.to_string()),
                    (
                        "Aliases reconciled",
                        report.semantic.reconciled_aliases.to_string(),
                    ),
                    (
                        "Aliases quarantined",
                        report.semantic.quarantined_aliases.to_string(),
                    ),
                    ("Elapsed", format_elapsed(started.elapsed())),
                ];
                if !crate::presentation::summary("Index complete", &rows) {
                    println!(
                        "indexed={} unchanged={} skipped={} removed={} pending={} symlinks={} errors={} elapsed={}",
                        report.indexed,
                        report.unchanged,
                        report.skipped,
                        report.removed,
                        report.pending,
                        report.skipped_symlinks,
                        report.errors.len(),
                        format_elapsed(started.elapsed())
                    );
                }
            }
        }
        Command::Read { path } => {
            let content = read_sanitized_file(&root, &path)?;
            let projected = crate::path_projection::project_workspace_path(&root, &path)?;
            if out.is_json() {
                out.emit(
                    "read",
                    json!({ "path": projected.display().to_string(), "content": content }),
                    None,
                );
            } else {
                print!("{content}");
            }
        }
        Command::ProjectPath { path } => {
            let projected = crate::path_projection::project_workspace_path(&root, &path)?;
            if out.is_json() {
                out.emit(
                    "project-path",
                    json!({ "path": projected.display().to_string() }),
                    None,
                );
            } else {
                println!("{}", projected.display());
            }
        }
        Command::Search {
            query,
            glob,
            max_results,
        } => {
            let progress = crate::presentation::TaskProgress::start(
                "Searching sanitized mirror",
                !out.is_json(),
            );
            let (hits, truncated) =
                crate::search::search_mirror_limited(&root, &query, glob.as_deref(), max_results)?;
            progress.finish();
            if out.is_json() {
                out.emit(
                    "search",
                    json!({ "hits": hits, "truncated": truncated }),
                    None,
                );
            } else {
                let rows = hits
                    .iter()
                    .map(|hit| {
                        vec![
                            hit.rel_path.clone(),
                            hit.line.to_string(),
                            hit.column.to_string(),
                            hit.line_text.clone(),
                        ]
                    })
                    .collect::<Vec<_>>();
                if !crate::presentation::table(
                    &format!("Search results | {} hit(s)", hits.len()),
                    &["File", "Line", "Col", "Preview"],
                    &rows,
                ) {
                    for hit in &hits {
                        println!(
                            "{}:{}:{}:{}",
                            hit.rel_path, hit.line, hit.column, hit.line_text
                        );
                    }
                }
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
            dry_run,
        } => {
            let patch_text = read_optional_file_or_stdin(patch.as_ref())?;
            let started = std::time::Instant::now();
            let progress = crate::presentation::TaskProgress::start(
                if dry_run {
                    "Validating patch"
                } else {
                    "Applying sanitized patch"
                },
                !out.is_json(),
            );
            let report = apply_patch_text_with_options(
                &root,
                &patch_text,
                ApplyOptions {
                    session_id,
                    agent,
                    dry_run,
                },
            )?;
            progress.finish();
            if out.is_json() {
                let mut data = serde_json::to_value(&report)?;
                data["dry_run"] = json!(dry_run);
                out.emit("apply-patch", data, Some(started.elapsed().as_millis()));
            } else {
                let rows = [
                    (
                        "Mode",
                        if dry_run { "dry run" } else { "applied" }.to_string(),
                    ),
                    ("Files", report.files.join(", ")),
                    ("Journal", display_journal(&report.journal_path)),
                    ("Elapsed", format_elapsed(started.elapsed())),
                ];
                if !crate::presentation::summary("Patch complete", &rows) {
                    match &report.journal_path {
                        Some(journal) => println!(
                            "applied files={} journal={} elapsed={}",
                            report.files.join(","),
                            journal.display(),
                            format_elapsed(started.elapsed())
                        ),
                        None => println!(
                            "dry-run ok files={} (no changes written) elapsed={}",
                            report.files.join(","),
                            format_elapsed(started.elapsed())
                        ),
                    }
                }
            }
        }
        Command::Write {
            path,
            sanitized_content,
        } => {
            let content = read_optional_file_or_stdin(sanitized_content.as_ref())?;
            let progress =
                crate::presentation::TaskProgress::start("Projecting mirror write", !out.is_json());
            let report = write_sanitized_content(&root, &path, &content)?;
            progress.finish();
            if out.is_json() {
                out.emit("write", serde_json::to_value(&report)?, None);
            } else {
                let rows = [
                    ("Files", report.files.join(", ")),
                    ("Journal", display_journal(&report.journal_path)),
                ];
                if !crate::presentation::summary("Write projected", &rows) {
                    println!(
                        "wrote files={} journal={}",
                        report.files.join(","),
                        display_journal(&report.journal_path)
                    );
                }
            }
        }
        Command::Rename {
            path,
            from,
            to,
            agent,
            session_id,
        } => {
            let progress = crate::presentation::TaskProgress::start(
                "Renaming through span maps",
                !out.is_json(),
            );
            let report = rename_alias(
                &root,
                &path,
                &from,
                &to,
                ApplyOptions {
                    session_id,
                    agent,
                    dry_run: false,
                },
            )?;
            progress.finish();
            if out.is_json() {
                out.emit("rename", serde_json::to_value(&report)?, None);
            } else {
                let rows = [
                    ("Original", report.real_from.clone()),
                    ("Renamed to", to.clone()),
                    ("Sanitized alias", report.sanitized_to.clone()),
                    ("Occurrences", report.occurrences.to_string()),
                    ("Files", report.apply.files.join(", ")),
                    ("Journal", display_journal(&report.apply.journal_path)),
                ];
                if !crate::presentation::summary("Rename complete", &rows) {
                    println!(
                        "renamed real={} -> {} occurrences={} sanitized_now={} files={} journal={}",
                        report.real_from,
                        to,
                        report.occurrences,
                        report.sanitized_to,
                        report.apply.files.join(","),
                        display_journal(&report.apply.journal_path)
                    );
                }
            }
        }
        Command::ProjectEdit {
            path,
            agent,
            session_id,
        } => {
            let progress =
                crate::presentation::TaskProgress::start("Projecting mirror edit", !out.is_json());
            let report = project_mirror_edit(
                &root,
                &path,
                ApplyOptions {
                    session_id,
                    agent,
                    dry_run: false,
                },
            )?;
            progress.finish();
            if out.is_json() {
                out.emit("project-edit", serde_json::to_value(&report)?, None);
            } else {
                let rows = [
                    ("Files", report.files.join(", ")),
                    ("Journal", display_journal(&report.journal_path)),
                ];
                if !crate::presentation::summary("Mirror edit projected", &rows) {
                    println!(
                        "projected files={} journal={}",
                        report.files.join(","),
                        display_journal(&report.journal_path)
                    );
                }
            }
        }
        Command::Recover { rollback, force } => {
            let progress =
                crate::presentation::TaskProgress::start("Recovering workspace", !out.is_json());
            let report = recover_workspace(&root, rollback, force)?;
            progress.finish();
            if out.is_json() {
                out.emit("recover", serde_json::to_value(&report)?, None);
            } else {
                let rows = [
                    ("Recovered", report.recovered.len().to_string()),
                    ("Rolled back", report.rolled_back.to_string()),
                    ("Conflicts", report.conflicts.len().to_string()),
                    ("Temp files removed", report.temp_files_removed.to_string()),
                ];
                if !crate::presentation::summary("Recovery complete", &rows) {
                    println!(
                        "recovered entries={} rolled_back={} conflicts={} temp_files_removed={}",
                        report.recovered.len(),
                        report.rolled_back,
                        report.conflicts.len(),
                        report.temp_files_removed
                    );
                }
            }
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
            if out.is_json() {
                out.emit("mode", json!({ "mode": mode }), None);
            } else {
                let rows = [("Enforcement mode", mode.to_string())];
                if !crate::presentation::summary("Workspace policy", &rows) {
                    println!("{mode}");
                }
            }
        }
        Command::ProposeSanitize {
            path,
            jobs,
            no_progress,
            allow_provider_command,
            allow_provider_endpoint,
        } => {
            let mut progress = ProposeProgressUi::new(!no_progress);
            let report = crate::proposal::propose_sanitize_with_progress(
                &root,
                path.as_deref(),
                crate::proposal::ProviderAllow {
                    command: allow_provider_command,
                    endpoint: allow_provider_endpoint,
                },
                jobs,
                |event| progress.handle(event),
            )?;
            for error in &report.errors {
                eprintln!("error: {error}");
            }
            if out.is_json() {
                out.emit("propose-sanitize", serde_json::to_value(&report)?, None);
            } else {
                let rows = [
                    ("Proposed", report.proposed.to_string()),
                    ("Queued", report.queued.to_string()),
                    ("Duplicates", report.duplicates.to_string()),
                    ("Rejected", report.rejected.len().to_string()),
                    ("Skipped", report.skipped.len().to_string()),
                    ("Errors", report.errors.len().to_string()),
                    (
                        "Semantic candidates",
                        format!(
                            "{}/{} sent",
                            report.eligibility.sent_symbol_candidates,
                            report.eligibility.owned_symbols
                        ),
                    ),
                    (
                        "Excluded unresolved",
                        report.eligibility.excluded_unresolved.to_string(),
                    ),
                    (
                        "Compiler-resolvable",
                        report.eligibility.compiler_resolvable_symbols.to_string(),
                    ),
                    (
                        "Excluded API boundary",
                        report.eligibility.excluded_api_boundary.to_string(),
                    ),
                    (
                        "Excluded existing alias",
                        report.eligibility.excluded_existing_alias.to_string(),
                    ),
                    (
                        "Already pending",
                        report.eligibility.pending_symbol_decisions.to_string(),
                    ),
                    (
                        "Path candidates",
                        report.eligibility.unique_path_candidates.to_string(),
                    ),
                ];
                if !crate::presentation::summary("Proposal scan complete", &rows) {
                    println!(
                        "proposed={} queued={} duplicates={} rejected={} skipped={} errors={} semantic_candidates={}/{} compiler_resolvable={} excluded_unresolved={} excluded_api_boundary={} excluded_existing_alias={} already_pending={} path_candidates={}",
                        report.proposed,
                        report.queued,
                        report.duplicates,
                        report.rejected.len(),
                        report.skipped.len(),
                        report.errors.len(),
                        report.eligibility.sent_symbol_candidates,
                        report.eligibility.owned_symbols,
                        report.eligibility.compiler_resolvable_symbols,
                        report.eligibility.excluded_unresolved,
                        report.eligibility.excluded_api_boundary,
                        report.eligibility.excluded_existing_alias,
                        report.eligibility.pending_symbol_decisions,
                        report.eligibility.unique_path_candidates,
                    );
                }
                for rejected in &report.rejected {
                    println!("rejected: {rejected}");
                }
                for skipped in &report.skipped {
                    println!("skipped: {skipped}");
                }
            }
        }
        Command::Review {
            approve,
            reject,
            all,
        } => {
            if let Some(id) = approve {
                let progress = crate::presentation::TaskProgress::start(
                    "Approving and reindexing",
                    !out.is_json(),
                );
                let item = crate::proposal::resolve_review(&root, &id, true)?;
                progress.finish();
                if out.is_json() {
                    out.emit(
                        "review",
                        json!({ "action": "approved", "item": item }),
                        None,
                    );
                } else {
                    let rows = [
                        ("Proposal", item.id.clone()),
                        ("Original", item.proposal.original_text.clone()),
                        ("Replacement", item.proposal.sanitized_text.clone()),
                        ("File", item.file.clone()),
                    ];
                    if !crate::presentation::summary("Proposal approved", &rows) {
                        println!(
                            "approved {} {} -> {} (file {})",
                            item.id,
                            item.proposal.original_text,
                            item.proposal.sanitized_text,
                            item.file
                        );
                    }
                }
            } else if let Some(id) = reject {
                let item = crate::proposal::resolve_review(&root, &id, false)?;
                if out.is_json() {
                    out.emit(
                        "review",
                        json!({ "action": "rejected", "item": item }),
                        None,
                    );
                } else {
                    if crate::presentation::stdout_is_tty() {
                        crate::presentation::success(&format!("rejected {}", item.id));
                    } else {
                        println!("rejected {}", item.id);
                    }
                }
            } else {
                let items = crate::proposal::list_review(&root, all)?;
                if out.is_json() {
                    out.emit("review", json!({ "items": items }), None);
                } else {
                    let rendered = crate::presentation::review_queue(&items);
                    if !rendered && items.is_empty() {
                        println!("review queue is empty");
                    } else if !rendered {
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
            }
        }
        Command::ReviewSanitize { path } => {
            let rows = crate::proposal::audit_replacements(&root, path.as_deref())?;
            if out.is_json() {
                out.emit("review-sanitize", json!({ "replacements": rows }), None);
            } else {
                let table_rows = rows
                    .iter()
                    .map(|row| {
                        vec![
                            row.file.clone(),
                            row.original_line.to_string(),
                            row.category.clone(),
                            row.original_text.clone(),
                            row.sanitized_text.clone(),
                            row.policy_source.clone(),
                        ]
                    })
                    .collect::<Vec<_>>();
                if !crate::presentation::table(
                    &format!("Applied replacements | {}", rows.len()),
                    &[
                        "File",
                        "Line",
                        "Category",
                        "Original",
                        "Replacement",
                        "Policy",
                    ],
                    &table_rows,
                ) {
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
            let progress =
                crate::presentation::TaskProgress::start("Synchronizing mirror", !out.is_json());
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
            progress.finish();
            for (path, reason) in &report.errors {
                eprintln!("error: {path}: {reason}");
            }
            for error in &report.semantic.errors {
                eprintln!("semantic error: {error}");
            }
            if out.is_json() {
                out.emit(
                    "sync",
                    serde_json::to_value(&report)?,
                    Some(started.elapsed().as_millis()),
                );
            } else {
                let rows = [
                    ("Indexed", report.indexed.to_string()),
                    ("Unchanged", report.unchanged.to_string()),
                    ("Skipped", report.skipped.to_string()),
                    ("Removed", report.removed.to_string()),
                    ("Pending edits", report.pending.to_string()),
                    ("Stashed", report.stashed.len().to_string()),
                    ("Errors", report.errors.len().to_string()),
                    ("AST revision", report.semantic.revision.to_string()),
                    ("AST indexed", report.semantic.indexed.to_string()),
                    ("AST read-only", report.semantic.read_only.to_string()),
                    (
                        "Aliases reconciled",
                        report.semantic.reconciled_aliases.to_string(),
                    ),
                    (
                        "Aliases quarantined",
                        report.semantic.quarantined_aliases.to_string(),
                    ),
                    ("Elapsed", format_elapsed(started.elapsed())),
                ];
                if !crate::presentation::summary("Sync complete", &rows) {
                    println!(
                        "synced indexed={} unchanged={} skipped={} removed={} pending={} stashed={} symlinks={} errors={} elapsed={}",
                        report.indexed,
                        report.unchanged,
                        report.skipped,
                        report.removed,
                        report.pending,
                        report.stashed.len(),
                        report.skipped_symlinks,
                        report.errors.len(),
                        format_elapsed(started.elapsed())
                    );
                }
            }
            for stash in &report.stashed {
                eprintln!("stashed pending mirror edit: {stash}");
            }
        }
        Command::EmbedIndex => {
            let started = std::time::Instant::now();
            let progress = crate::presentation::TaskProgress::start(
                "Embedding sanitized mirror",
                !out.is_json(),
            );
            let report = crate::embed::embed_index(&root)?;
            progress.finish();
            if out.is_json() {
                out.emit(
                    "embed-index",
                    serde_json::to_value(&report)?,
                    Some(started.elapsed().as_millis()),
                );
            } else {
                let rows = [
                    ("Embedded", report.embedded.to_string()),
                    ("Unchanged", report.unchanged.to_string()),
                    ("Removed", report.removed.to_string()),
                    ("Stale", report.stale.to_string()),
                    ("Chunks", report.chunks.to_string()),
                    ("Elapsed", format_elapsed(started.elapsed())),
                ];
                if !crate::presentation::summary("Embedding index complete", &rows) {
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
            }
        }
        Command::SemanticSearch { query, k } => {
            let started = std::time::Instant::now();
            let progress =
                crate::presentation::TaskProgress::start("Running semantic search", !out.is_json());
            let hits = crate::embed::semantic_search(&root, &query, k)?;
            progress.finish();
            if out.is_json() {
                out.emit(
                    "semantic-search",
                    json!({ "hits": hits }),
                    Some(started.elapsed().as_millis()),
                );
            } else {
                let rows = hits
                    .iter()
                    .map(|hit| {
                        vec![
                            hit.rel_path.clone(),
                            format!("{}-{}", hit.start_line, hit.end_line),
                            format!("{:.3}", hit.score),
                            hit.preview.clone(),
                        ]
                    })
                    .collect::<Vec<_>>();
                if !crate::presentation::table(
                    &format!("Semantic search | {} hit(s)", hits.len()),
                    &["File", "Lines", "Score", "Preview"],
                    &rows,
                ) {
                    for hit in &hits {
                        println!(
                            "{}:{}-{}\t{:.3}\t{}",
                            hit.rel_path, hit.start_line, hit.end_line, hit.score, hit.preview
                        );
                    }
                }
            }
            // Stdout stays machine-parseable result lines; the summary goes to
            // stderr (most of the latency is the query embedding HTTP call).
            eprintln!(
                "[{} hit(s) elapsed={}]",
                hits.len(),
                format_elapsed(started.elapsed())
            );
        }
        Command::WorkspaceSnapshot => {
            let snapshot = semantic_read(&root, crate::semantic_store::snapshot)?;
            emit_semantic_value(out, "workspace-snapshot", serde_json::to_value(snapshot)?)?;
        }
        Command::FindCode { query, limit } => {
            let result = semantic_read(&root, |conn| {
                let layout = crate::config::Layout::new(&root);
                let config = crate::config::Config::load_or_default(&layout)?;
                let projection =
                    crate::path_projection::PathProjection::from_connection(&config, conn)?;
                Ok(json!({
                    "revision": crate::semantic_store::current_revision(conn)?,
                    "symbols": crate::semantic_store::find_symbols(conn, &root, &query, limit)?
                        .into_iter()
                        .map(|(path, symbol)| {
                            Ok(json!({
                                "path": projection.projected_string_for_real(&path)?,
                                "symbol": symbol,
                            }))
                        })
                        .collect::<Result<Vec<_>>>()?
                }))
            })?;
            emit_semantic_value(out, "find-code", result)?;
        }
        Command::ReadCode { path } => {
            let path = crate::config::normalize_safe_rel_path(&path, "read-code path")?;
            let document = semantic_read(&root, |conn| {
                let layout = crate::config::Layout::new(&root);
                let config = crate::config::Config::load_or_default(&layout)?;
                let projection =
                    crate::path_projection::PathProjection::from_connection(&config, conn)?;
                let real = projection.real_for_agent(&path)?;
                let rel = crate::config::normalize_rel_path(&real);
                let mut document = crate::semantic_store::project_document(conn, &root, &rel)?;
                document.rel_path = projection.projected_string_for_real(&rel)?;
                Ok(document)
            })?;
            emit_semantic_value(out, "read-code", serde_json::to_value(document)?)?;
        }
        Command::EditNode {
            node_id,
            replacement,
            expected_revision,
        } => {
            let preview = crate::transaction::preview_transaction(
                &root,
                expected_revision,
                vec![crate::transaction::EditIntent::EditNode {
                    node_id,
                    replacement,
                }],
            )?;
            emit_semantic_value(out, "edit-node", serde_json::to_value(preview)?)?;
        }
        Command::RenameSymbol {
            symbol_id,
            new_name,
            expected_revision,
        } => {
            let preview = crate::transaction::preview_transaction(
                &root,
                expected_revision,
                vec![crate::transaction::EditIntent::RenameSymbol {
                    symbol_id,
                    new_name,
                }],
            )?;
            emit_semantic_value(out, "rename-symbol", serde_json::to_value(preview)?)?;
        }
        Command::PreviewTransaction {
            intents,
            expected_revision,
        } => {
            let raw = read_optional_file_or_stdin(intents.as_ref())?;
            let intents = serde_json::from_str::<Vec<crate::transaction::EditIntent>>(&raw)
                .context("parse transaction intents JSON")?;
            let preview =
                crate::transaction::preview_transaction(&root, expected_revision, intents)?;
            emit_semantic_value(out, "preview-transaction", serde_json::to_value(preview)?)?;
        }
        Command::CommitTransaction {
            transaction_id,
            expected_revision,
            agent,
            session_id,
        } => {
            let report = crate::transaction::commit_transaction(
                &root,
                &transaction_id,
                expected_revision,
                agent,
                session_id,
            )?;
            emit_semantic_value(out, "commit-transaction", serde_json::to_value(report)?)?;
        }
        Command::Verify => {
            let progress =
                crate::presentation::TaskProgress::start("Verifying workspace", !out.is_json());
            let report = verify_workspace(&root)?;
            progress.finish();
            if out.is_json() {
                out.emit("verify", serde_json::to_value(&report)?, None);
            } else {
                let rows = [
                    ("Tracked files", report.checked.to_string()),
                    ("Failures", report.failures.len().to_string()),
                    ("Status", "healthy".to_string()),
                ];
                if !crate::presentation::summary("Verification complete", &rows) {
                    println!("verified tracked_files={}", report.checked);
                }
            }
        }
        Command::Doctor { agent } => {
            doctor(&root, agent, out)?;
        }
        Command::InstallHooks { agent, force } => {
            install_hooks(&root, agent, force, out)?;
        }
        Command::UninstallHooks { agent } => {
            uninstall_hooks(&root, agent, out)?;
        }
        Command::Completions { .. } => unreachable!("completions are handled before dispatch"),
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

fn semantic_read<T>(
    root: &std::path::Path,
    operation: impl FnOnce(&rusqlite::Connection) -> Result<T>,
) -> Result<T> {
    let layout = crate::config::Layout::new(root);
    layout.require_initialized()?;
    let _lock = crate::lock::WorkspaceLock::acquire_shared(&layout)?;
    let conn = crate::db::connect(&layout)?;
    crate::db::check_schema(&conn)?;
    operation(&conn)
}

fn emit_semantic_value(
    out: crate::output::Output,
    command: &'static str,
    value: serde_json::Value,
) -> Result<()> {
    if out.is_json() {
        out.emit(command, value, None);
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).context("serialize semantic CLI output")?
        );
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

/// Journal path for CLI stdout; `-` for the no-journal (dry-run) case, which
/// non-dry paths never produce.
fn display_journal(journal_path: &Option<PathBuf>) -> String {
    journal_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn doctor(root: &std::path::Path, agent: Option<Agent>, out: crate::output::Output) -> Result<()> {
    use serde_json::json;
    let layout = crate::config::Layout::new(root);
    let pretty = !out.is_json() && crate::presentation::stdout_is_tty();
    let path_status = |path: &std::path::Path| json!({ "path": path.display().to_string(), "exists": path.exists() });
    if !out.is_json() && !pretty {
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
    }
    let agent_status = match agent {
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
            if !out.is_json() && !pretty {
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
            Some(json!({
                "name": "codex",
                "installed": installed,
                "files": [path_status(&hooks), path_status(&pre), path_status(&post)],
            }))
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
            if !out.is_json() && !pretty {
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
            Some(json!({
                "name": "claude",
                "installed": installed,
                "files": [
                    path_status(&settings),
                    path_status(&pre),
                    path_status(&post),
                    path_status(&session),
                ],
            }))
        }
        Some(Agent::Opencode) => {
            let plugin = root.join(".opencode/plugins/code-sanity.ts");
            let pkg = root.join(".opencode/package.json");
            let plugin_ok = plugin.exists();
            let installed = plugin_ok
                && fs::read_to_string(&plugin)
                    .map(|body| body.contains("project-edit"))
                    .unwrap_or(false);
            if !out.is_json() && !pretty {
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
            Some(json!({
                "name": "opencode",
                "installed": installed,
                "files": [path_status(&plugin), path_status(&pkg)],
            }))
        }
        None => {
            if !out.is_json() && !pretty {
                println!("agents: codex, claude, opencode");
            }
            None
        }
    };
    if pretty {
        let mut rows = vec![
            ("Root", root.display().to_string()),
            (
                "State",
                if layout.state_dir.exists() {
                    "ready"
                } else {
                    "missing"
                }
                .to_string(),
            ),
            (
                "Config",
                if layout.config_path.exists() {
                    "present"
                } else {
                    "missing"
                }
                .to_string(),
            ),
            (
                "Database",
                if layout.db_path.exists() {
                    "present"
                } else {
                    "missing"
                }
                .to_string(),
            ),
            (
                "Mirror",
                if layout.mirror_dir.exists() {
                    "present"
                } else {
                    "missing"
                }
                .to_string(),
            ),
            (
                "Maps",
                if layout.maps_dir.exists() {
                    "present"
                } else {
                    "missing"
                }
                .to_string(),
            ),
        ];
        if let Some(status) = &agent_status {
            rows.push((
                "Agent",
                status["name"].as_str().unwrap_or("unknown").to_string(),
            ));
            rows.push((
                "Hooks",
                if status["installed"].as_bool().unwrap_or(false) {
                    "installed"
                } else {
                    "not installed"
                }
                .to_string(),
            ));
        } else {
            rows.push(("Agents", "codex, claude, opencode".to_string()));
        }
        crate::presentation::summary("Workspace doctor", &rows);
    }
    out.emit(
        "doctor",
        json!({
            "root": root.display().to_string(),
            "state_dir": path_status(&layout.state_dir),
            "config": path_status(&layout.config_path),
            "db": path_status(&layout.db_path),
            "mirror": path_status(&layout.mirror_dir),
            "maps": path_status(&layout.maps_dir),
            "agent": agent_status,
        }),
        None,
    );
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

fn install_hooks(
    root: &std::path::Path,
    agent: Agent,
    force: bool,
    out: crate::output::Output,
) -> Result<()> {
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
    if out.is_json() {
        out.emit(
            "install-hooks",
            serde_json::json!({ "agent": installed.to_lowercase() }),
            None,
        );
    } else {
        crate::presentation::success(&format!("installed hooks for {installed}"));
    }
    Ok(())
}

fn uninstall_hooks(root: &std::path::Path, agent: Agent, out: crate::output::Output) -> Result<()> {
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
    if out.is_json() {
        out.emit(
            "uninstall-hooks",
            serde_json::json!({ "agent": name.to_lowercase() }),
            None,
        );
    } else {
        crate::presentation::success(&format!("uninstalled hooks for {name}"));
    }
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
  const toMirror = async (p?: string) => {
    const rel = toRel(p)
    if (!rel) return p
    const projected = (await run(["project-path", rel])) || rel
    return join(mirrorRoot, projected)
  }
  const inMirror = (p?: string) => {
    if (!p) return false
    const abs = isAbsolute(p) ? p : join(root, p)
    return abs.startsWith(mirrorRoot)
  }
  const redirect = async (args: any) => {
    if (args?.filePath) args.filePath = await toMirror(args.filePath)
    if (args?.path) args.path = await toMirror(args.path)
  }

  return {
    "tool.execute.before": async (input: any, output: any) => {
      const tool = input?.tool
      const args = output?.args
      if (!tool || !args) return
      if (READ_TOOLS.has(tool)) {
        await redirect(args)
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
        await redirect(args)
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
        log_path = os.path.join(log_dir, "hooks.log")
        try:
            # Mirror the Rust logger's rotation: one .old generation at 5 MiB,
            # so per-edit hook syncs cannot grow the log without bound.
            if os.path.getsize(log_path) > 5 * 1024 * 1024:
                os.replace(log_path, log_path + ".old")
        except OSError:
            pass
        with open(log_path, "a", encoding="utf-8") as handle:
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
        log_path = os.path.join(log_dir, "hooks.log")
        try:
            # Mirror the Rust logger's rotation: one .old generation at 5 MiB,
            # so per-edit hook syncs cannot grow the log without bound.
            if os.path.getsize(log_path) > 5 * 1024 * 1024:
                os.replace(log_path, log_path + ".old")
        except OSError:
            pass
        with open(log_path, "a", encoding="utf-8") as handle:
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
        log_path = os.path.join(log_dir, "hooks.log")
        try:
            # Mirror the Rust logger's rotation: one .old generation at 5 MiB,
            # so per-edit hook syncs cannot grow the log without bound.
            if os.path.getsize(log_path) > 5 * 1024 * 1024:
                os.replace(log_path, log_path + ".old")
        except OSError:
            pass
        with open(log_path, "a", encoding="utf-8") as handle:
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
