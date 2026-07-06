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
    /// Run the configured proposal provider and queue proposals for review.
    ProposeSanitize {
        #[arg(long)]
        path: Option<PathBuf>,
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
        Command::ProposeSanitize { path } => {
            let report = crate::proposal::propose_sanitize(&root, path.as_deref())?;
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
            println!("codex hooks.json={} exists={}", hooks.display(), hooks.exists());
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

fn install_hooks(root: &std::path::Path, agent: Agent) -> Result<()> {
    let installed = format!("{agent:?}");
    match agent {
        Agent::Codex => {
            let dir = root.join(".codex/hooks");
            fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
            fs::write(root.join(".codex/hooks.json"), CODEX_HOOKS_JSON)
                .context("write codex hooks.json")?;
            fs::write(dir.join("pre_tool_use.py"), CODEX_PRE_TOOL_USE_PY)
                .context("write codex pre_tool_use.py")?;
            fs::write(dir.join("post_tool_use.py"), CODEX_POST_TOOL_USE_PY)
                .context("write codex post_tool_use.py")?;
        }
        Agent::Claude => {
            let dir = root.join(".claude/hooks");
            fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
            fs::write(root.join(".claude/settings.json"), CLAUDE_SETTINGS_JSON)
                .context("write claude settings.json")?;
            fs::write(dir.join("pre_tool_use.py"), CLAUDE_PRE_TOOL_USE_PY)
                .context("write claude pre_tool_use.py")?;
            fs::write(dir.join("post_tool_use.py"), CLAUDE_POST_TOOL_USE_PY)
                .context("write claude post_tool_use.py")?;
            fs::write(dir.join("session_start.py"), CLAUDE_SESSION_START_PY)
                .context("write claude session_start.py")?;
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

const CODEX_HOOKS_JSON: &str = r##"{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "*",
        "hooks": [
          { "type": "command", "command": "python3 .codex/hooks/pre_tool_use.py" }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "*",
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


def main():
    try:
        payload = json.load(sys.stdin)
    except Exception:
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

const CODEX_POST_TOOL_USE_PY: &str = r##"#!/usr/bin/env python3
# code-sanity Codex PostToolUse: keep the mirror in sync after edits (best-effort).
import json, os, subprocess, sys


def main():
    try:
        payload = json.load(sys.stdin)
    except Exception:
        payload = {}
    binary = os.environ.get("CODE_SANITY_BIN", "code-sanity")
    try:
        subprocess.run(
            [binary, "sync"],
            cwd=payload.get("cwd") or os.getcwd(),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=60,
        )
    except Exception:
        pass


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


def main():
    try:
        payload = json.load(sys.stdin)
    except Exception:
        return
    cwd = payload.get("cwd") or os.getcwd()
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

const CLAUDE_POST_TOOL_USE_PY: &str = r##"#!/usr/bin/env python3
# code-sanity Claude Code PostToolUse: keep the mirror in sync after edits (best-effort).
import json, os, subprocess, sys


def main():
    try:
        payload = json.load(sys.stdin)
    except Exception:
        payload = {}
    binary = os.environ.get("CODE_SANITY_BIN", "code-sanity")
    try:
        subprocess.run(
            [binary, "sync"],
            cwd=payload.get("cwd") or os.getcwd(),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=60,
        )
    except Exception:
        pass


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
