use crate::index::{index_workspace, init_workspace};
use crate::patch::{
    ApplyOptions, apply_patch_text_with_options, project_mirror_edit, recover_workspace,
    rename_alias, write_sanitized_content,
};
use crate::search::{read_sanitized_file, search_mirror};
use crate::verify::verify_workspace;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "code-sanity")]
#[command(about = "Sanitized mirror and patch bridge for agent code workflows")]
pub struct Cli {
    #[arg(long, global = true, default_value = ".")]
    root: PathBuf,

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
    },
    /// Print the configured enforcement mode (soft|guided|strict).
    Mode,
    Sync,
    Verify,
    Doctor {
        #[arg(long)]
        agent: Option<Agent>,
    },
    InstallHooks {
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

    match cli.command {
        Command::Init => {
            let layout = init_workspace(&root)?;
            println!("initialized {}", layout.state_dir.display());
        }
        Command::Index => {
            let report = index_workspace(&root)?;
            println!(
                "indexed={} unchanged={} skipped={} removed={}",
                report.indexed, report.unchanged, report.skipped, report.removed
            );
        }
        Command::Read { path } => {
            print!("{}", read_sanitized_file(&root, &path)?);
        }
        Command::Search { query, glob } => {
            for hit in search_mirror(&root, &query, glob.as_deref())? {
                println!(
                    "{}:{}:{}:{}",
                    hit.rel_path, hit.line, hit.column, hit.line_text
                );
            }
        }
        Command::ApplyPatch {
            patch,
            agent,
            session_id,
        } => {
            let patch_text = read_optional_file_or_stdin(patch.as_ref())?;
            let report = apply_patch_text_with_options(
                &root,
                &patch_text,
                ApplyOptions { session_id, agent },
            )?;
            println!(
                "applied files={} journal={}",
                report.files.join(","),
                report.journal_path.display()
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
            let report = rename_alias(
                &root,
                &path,
                &from,
                &to,
                ApplyOptions { session_id, agent },
            )?;
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
        Command::Recover { rollback } => {
            let report = recover_workspace(&root, rollback)?;
            println!(
                "recovered entries={} rolled_back={}",
                report.recovered.len(),
                report.rolled_back
            );
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
        Command::Sync => {
            let report = index_workspace(&root)?;
            println!(
                "synced indexed={} unchanged={} skipped={} removed={}",
                report.indexed, report.unchanged, report.skipped, report.removed
            );
        }
        Command::Verify => {
            let report = verify_workspace(&root)?;
            println!("verified tracked_files={}", report.checked);
        }
        Command::Doctor { agent } => {
            doctor(&root, agent)?;
        }
        Command::InstallHooks { agent } => {
            install_hooks(&root, agent)?;
        }
        Command::Serve { once } => {
            if once {
                println!(
                    "serve --once: daemon scaffold is reachable; MCP/HTTP API is not implemented in MVP"
                );
            } else {
                println!(
                    "daemon scaffold only; use CLI read/search/apply-patch/write/sync/verify for MVP"
                );
            }
        }
    }

    Ok(())
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
            println!(
                "codex hooks scaffold target: {}",
                root.join(".codex/hooks.json").display()
            );
            println!(
                "strict enforcement requires running agents in mirror/overlay; hooks are best-effort"
            );
        }
        Some(Agent::Claude) => {
            println!(
                "claude hooks scaffold target: {}",
                root.join(".claude/settings.json").display()
            );
            println!(
                "recommended path is MCP/guard config; transparent read rewrite is not assumed"
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

fn install_hooks(root: &std::path::Path, agent: Agent) -> Result<()> {
    let installed = format!("{agent:?}");
    match agent {
        Agent::Codex => {
            let dir = root.join(".codex/hooks");
            fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
            let hooks = root.join(".codex/hooks.json");
            fs::write(
                &hooks,
                r#"{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "python3 .codex/hooks/pre_tool_use.py"
          }
        ]
      }
    ]
  }
}
"#,
            )
            .with_context(|| format!("write {}", hooks.display()))?;
            fs::write(
                dir.join("pre_tool_use.py"),
                "import json, sys\npayload = json.load(sys.stdin)\nprint(json.dumps({\"permissionDecision\":\"allow\",\"message\":\"code-sanity guided mode: prefer code-sanity read/search/apply-patch\"}))\n",
            )
            .context("write codex pre_tool_use.py")?;
        }
        Agent::Claude => {
            let dir = root.join(".claude/hooks");
            fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
            fs::write(
                root.join(".claude/settings.json"),
                "{\n  \"hooks\": {}\n}\n",
            )
            .context("write claude settings scaffold")?;
        }
        Agent::Opencode => {
            let dir = root.join(".opencode/plugins");
            fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
            fs::write(dir.join("code-sanity.ts"), OPENCODE_PLUGIN_TS)
                .context("write opencode plugin")?;
            fs::write(
                root.join(".opencode/package.json"),
                "{\n  \"name\": \"code-sanity-opencode-plugin\",\n  \"version\": \"0.1.0\",\n  \"private\": true,\n  \"type\": \"module\"\n}\n",
            )
            .context("write opencode package.json")?;
        }
    }
    println!("installed hooks for {installed}");
    Ok(())
}

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

const BIN = process.env.CODE_SANITY_BIN || "code-sanity"
const MIRROR_REL = ".code-sanity/mirror"
const READ_TOOLS = new Set(["read", "grep", "glob", "list"])
const EDIT_TOOLS = new Set(["edit", "write", "patch"])

export const CodeSanityPlugin = async ({ directory, $ }: any) => {
  const root = directory
  const mirrorRoot = join(root, MIRROR_REL)

  const run = async (args: string[]) => {
    try {
      const out = await $`${BIN} --root ${root} ${args}`.quiet()
      return out.stdout.toString().trim()
    } catch (_e) {
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
      await run(["project-edit", "--path", rel, "--agent", "opencode"])
    },
    "file.edited": async () => {
      await run(["sync"])
    },
  }
}

export default CodeSanityPlugin
"#;
