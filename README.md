# code-sanity

`code-sanity` builds a sanitized mirror of a real repository and applies agent edits from that mirror back to the real files. The real repository remains the source of truth; `.code-sanity/mirror` is the agent-facing view, and `.code-sanity/maps` plus `db.sqlite` hold span and hash state.

This is a Rust-first MVP from `PLAN.md`. LLM sanitization is intentionally stubbed behind a provider interface.

## Quick Start

```bash
cargo run -- init
cargo run -- index
cargo run -- read src/lib.rs
cargo run -- search neutral_parser
cargo run -- grep neutral_parser
cargo run -- verify
```

Apply a patch written against the sanitized mirror:

```bash
cargo run -- apply-patch --patch /path/to/sanitized.diff
```

Replace a sanitized file and back-project it to the real repository:

```bash
cargo run -- write --path src/lib.rs --sanitized-content /path/to/new-sanitized-src.txt
```

Rescan after external edits:

```bash
cargo run -- sync
```

Check the fixture flow:

```bash
cargo run -- --root fixtures/basic-rust index
cargo run -- --root fixtures/basic-rust read src/lib.rs
cargo run -- --root fixtures/basic-rust grep neutral_parser
cargo run -- --root fixtures/basic-rust verify
```

## Layout

`init` creates:

```text
.code-sanity/
  config.toml
  db.sqlite
  mirror/
  maps/
  journal/
  logs/
  tmp/
```

`.code-sanity/` is ignored by git.

## Sanitization MVP

The default provider is deterministic and local:

- static dictionary in `.code-sanity/config.toml`;
- comments and doc-like text are sanitized;
- string literals are sanitized only in fixture/test contexts;
- private-looking identifiers are sanitized where the regex tokenizer can do it safely;
- public/import/export-like identifiers are skipped conservatively;
- line count is preserved, but replacement lengths may differ.

Every tracked file gets a JSON span map with original and sanitized byte offsets, line starts, hashes, replacement spans, and rendered sizes.

## Patch Bridge

`apply-patch` accepts unified diffs against sanitized paths such as `a/src/lib.rs`, `b/src/lib.rs`, or `.code-sanity/mirror/src/lib.rs`. Modify, create (`--- /dev/null`), and delete (`+++ /dev/null`) patches are all supported.

Before writing real files it:

- checks real and mirror hashes against SQLite state;
- applies the patch to the current mirror in memory;
- rejects edits whose changed byte range intersects a replacement span;
- reverses aliases only at known replacement spans from the span map;
- applies the translated patch in memory;
- verifies `sanitize(patched real) == patched mirror`;
- writes a durable `applying` journal entry (with per-file before/after snapshots) **before** touching any real file, then writes through temporary files plus rename, reindexes changed files, and finalizes the journal entry to `success`.

For create patches the added lines become the real file directly (the new file must already be neutral: `sanitize(real) == real`). For delete patches the entire mirror file must be removed, and the real file, mirror, map, and db row are all dropped.

Conflicts write `.code-sanity/journal/*.patch.json` and leave the real file unchanged. If a write or reindex step fails after real-file writes start, the changed real files are restored from the before-snapshots and the entry is marked `rolled-back`.

### Crash safety and recover

Because the intent is journaled before any real write, a process killed mid-apply leaves an `applying` entry on disk. `code-sanity recover` replays it to the recorded `after` state (roll-forward); `code-sanity recover --rollback` restores every touched file to its `before` state. Recover clears a stale apply lock left by the crash and assumes no live apply is running concurrently.

### Rename

Editing inside a replacement span via a normal patch is refused on purpose. `code-sanity rename --path <rel> --from <alias> --to <name>` is the sanctioned path: the sanitized alias is reversed through the span map to the real identifier, which is renamed across the real file and reindexed. The rename lands on the real symbol (real repo is the source of truth), goes through the same crash-safe journal, and is scoped to a single file.

## Current Commands

- `init`
- `index`
- `read <path>`
- `search <query> [--glob <glob>]`
- `grep <query> [--glob <glob>]`
- `apply-patch [--patch <file>] [--agent <name>] [--session-id <id>]`
- `write --path <path> [--sanitized-content <file>]`
- `rename --path <path> --from <alias> --to <name> [--agent <name>] [--session-id <id>]`
- `project-edit --path <path> [--agent <name>] [--session-id <id>]`
- `recover [--rollback]`
- `mode`
- `sync`
- `verify`
- `doctor [--agent codex|claude|opencode]`
- `install-hooks --agent codex|claude|opencode`
- `serve [--once]`

The Codex/Claude hooks are still scaffolds. The opencode plugin and the MCP server are working adapters.

## MCP Server

`code-sanity serve` runs a Model Context Protocol server over stdio with tools `read_file`, `search`, `list_files`, `apply_patch`, and `verify`. Reads and search return sanitized content only; `apply_patch` projects a sanitized diff back onto the real repo through the bridge. Inspect the manifest with `code-sanity serve --once`. See [docs/MCP.md](docs/MCP.md) for Codex, Claude Code, and opencode connection config.

## Agent Adapters

### opencode

`code-sanity install-hooks --agent opencode` generates a working plugin at `.opencode/plugins/code-sanity.ts` (plus `.opencode/package.json`). It:

- redirects `read`/`grep`/`glob`/`list` tools to the sanitized mirror;
- lets `edit`/`write` land on the mirror file, then back-projects the change to the real repo with `code-sanity project-edit` (span-aware, conflict-safe);
- in `strict` mode, blocks edits that target the real repo instead of the mirror;
- keeps the mirror synced at session start and on external `file.edited` events.

The plugin resolves the CLI as `code-sanity` on `PATH` or `$CODE_SANITY_BIN`. `project-edit` is the bridge primitive: after the agent edits a mirror file in place, it refreshes the baseline from `sanitize(real)` and drives the difference through the patch bridge. Hooks are a guardrail, not a hard boundary — reads via `bash`/other tools are not intercepted; hard isolation requires strict-mode worktree isolation.

### Codex

`code-sanity install-hooks --agent codex` writes `.codex/hooks.json` plus `.codex/hooks/{pre,post}_tool_use.py`. The `PreToolUse` guard:

- denies raw real-repo edits (`apply_patch`/`edit`/`write`) in strict mode and steers to the code_sanity MCP `apply_patch` tool;
- nudges toward `apply_patch` in guided mode;
- best-effort redirects obvious shell reads (`cat`/`nl`/`head`/`tail <file>`) to `code-sanity read` via `updatedInput`;
- always allows `code_sanity` MCP tools and mirror-targeted edits.

`PostToolUse` runs `code-sanity sync` after edits. Pair with the [MCP server](docs/MCP.md). Codex `PreToolUse` is explicitly a guardrail, not a full enforcement boundary — it does not intercept every shell path.

### Claude Code

`code-sanity install-hooks --agent claude` writes `.claude/settings.json` plus `.claude/hooks/{pre_tool_use,post_tool_use,session_start}.py`. The `PreToolUse` guard denies raw real-repo `Read`/`Edit`/`Write` in strict mode (guided denies edits) and steers to the code-sanity MCP server; `SessionStart` injects guidance to use the code-sanity tools; `PostToolUse` runs `code-sanity sync`. Register the MCP server as in [docs/MCP.md](docs/MCP.md). Hooks read the enforcement mode from `.code-sanity/config.toml` and are a guardrail, not a transparent read rewrite.

## Safety Notes

This tool is for lexical normalization and privacy reduction, not for hiding real behavior. The sanitizer should not rewrite control flow, imports, public APIs, auth semantics, dangerous APIs, protocol strings, SQL, shell commands, or other behavior-bearing text.

Hooks are not a complete enforcement boundary. Strict protection requires running agents inside the sanitized mirror or an overlay/sandbox where raw repository reads are physically unavailable.

## Known Limitations

- Tokenization is regex/byte-scanner based, not AST-aware.
- Multi-file apply is journaled before writes and recoverable via `recover`, but it is still a single-process MVP and not a substitute for transactional filesystem/database commits.
- Patch back-projection is span-aware for known replacement spans, but hunk coordinate remapping is still line-oriented and rejects edits inside aliases; use `rename` to change a symbol behind an alias.
- `rename` is single-file scoped; it does not chase references across files.
- Public API detection is conservative heuristic protection, not a full language-aware symbol graph.
- `.gitignore` support is delegated to the `ignore` crate (full gitignore language, `require_git(false)`); the walker does not follow parent-directory or global gitignores, for determinism.
- The opencode plugin, MCP server, and Codex/Claude hooks are working guardrail adapters, not hard boundaries; they do not intercept reads via `bash` or other non-file tools.
- Codex/Claude hooks require `python3` on the host.
- The LLM sanitizer provider remains a scaffold for a later phase.

## Tests

```bash
cargo test
```

The test suite covers indexing, sanitized read/search, path traversal rejection, empty grep/search rejection, span map offsets, patch roundtrip, alias-collision back-projection, public API consistency, conflicts inside replacements, rollback on simulated multi-file apply failure, sync after external edit, UTF-8 offsets, ignore rules, mixed language fixtures, and CLI smoke flows.
