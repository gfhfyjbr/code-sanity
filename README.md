# code-sanity

`code-sanity` builds a sanitized mirror of a real repository and applies agent edits from that mirror back to the real files. The real repository remains the source of truth; `.code-sanity/mirror` is the agent-facing view, and `.code-sanity/maps` plus `db.sqlite` hold span and hash state.

Sanitization is deterministic and local (dictionary + human-approved alias registry + denylist). A model can *propose* aliases through a provider interface, but it never writes the mirror.

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

## Sanitization

The engine is deterministic and local. One matching primitive is shared by the
sanitizer and the verify leak backstop: every configured term is matched
**case-insensitively and underscore-insensitively inside word runs**
(`[A-Za-z0-9_]+`), in comments, in **all** string literals, and in identifiers
alike. A registry entry for `AcmeClient` therefore also catches `ACME_CLIENT`
and `acmeClientFactory`.

- terms come from the static dictionary, the human-approved alias registry, and
  the denylist (denylist terms are removed immediately with a deterministic
  salted `sym_xxxxxxxx` alias, before any human picks a nicer name);
- replacements adapt to the casing of the matched slice (`ACME` → `CLIENT`,
  `Acme` → `Client`);
- the **repo-wide protected identifier set** (public declarations,
  import-position names, dunder names, collected from the real files) is the
  only sanctioned residue: one symbol gets one decision across the whole
  mirror, so a `pub fn` keeps its name at every call site in every file;
- `'` is not a string delimiter in Rust/Go, so lifetimes (`&'a str`) cannot
  open phantom strings that would suppress sanitization;
- zone detection (comment/string/identifier) only labels the replacement
  category — it can never suppress a replacement;
- line count is preserved, but replacement lengths may differ;
- the per-workspace salt is randomly generated at `init`.

Every tracked file gets a JSON span map with original and sanitized byte offsets, line starts, hashes, replacement spans, and rendered sizes.

### Incremental index

Every file is a component owning its mirror file, span map, and db rows. A file
is re-rendered only when its **input fingerprint** (content sha256, with an
mtime/size pre-check that avoids reading unchanged files) or the **logic
fingerprint** (dictionary, registry, allow/deny lists, salt, sanitizer behavior
version, and the repo-wide protected symbol set) changes. A file that
disappeared takes its targets with it. Each file commits in a single SQLite
transaction with idempotent upserts; the database runs in WAL mode with a busy
timeout and is fully derived state (`PRAGMA user_version` migrations recreate
it). An unchanged 5k-file repo re-indexes in well under a second; editing one
file re-renders exactly one file.

Mirror files holding a **pending agent edit** (mirror on disk differs from the
last indexed sanitized hash) are never clobbered by `sync`/`index`; only the
patch bridge resets them after projecting the edit. `sync --force` is the
recovery path: it re-renders everything and resets pending (or tampered)
mirror files back to `sanitize(real)`.

## Model-based sanitizer

The model never writes the mirror. It runs only in an offline *propose* step and its output is validated, queued, and applied deterministically:

1. `code-sanity propose-sanitize [--path <path>]` runs the configured proposal provider. The default is a deterministic `HeuristicProposalProvider` (proposes neutral aliases for denylisted terms). Set `provider.kind = "external"` with a `command` (and optional `timeout_secs`) to plug in a local model; it receives `{rel, content}` JSON on stdin and returns a `ProposalBatch`. Because the command comes from repo-local config, executing it requires explicit confirmation with `--allow-provider-command`; stdin/stdout are pumped concurrently (no pipe deadlock on large files) and the child is killed on timeout.
2. Each proposal is validated: the original must appear in the file, allowlisted terms are refused, identifier aliases must be valid identifiers, aliases may not introduce newlines or contain a denylisted term. Survivors are queued under `.code-sanity/review/`; anything touching a public API name or below `confidence_threshold` is flagged for review.
3. `code-sanity review [--all]` lists the queue. `review --approve <id>` records the alias in the deterministic registry (`sanitizer.alias_registry` in `config.toml`) and reindexes the file; `review --reject <id>` drops it. Approval re-validates so a stale queue can't apply an unsafe alias.
4. `index`/`verify` use only the deterministic engine (dictionary + alias registry), so they stay reproducible and the model stays out of the write path.

`code-sanity review-sanitize [--path <path>]` prints an audit of every applied replacement (category, original → sanitized, policy source, confidence, line) read from the span maps.

## Patch Bridge

`apply-patch` accepts unified diffs against sanitized paths such as `a/src/lib.rs`, `b/src/lib.rs`, or `.code-sanity/mirror/src/lib.rs`. Modify, create (`--- /dev/null`), and delete (`+++ /dev/null`) patches are all supported.

Before writing real files it:

- checks real and mirror hashes against SQLite state;
- applies the patch to the current mirror in memory;
- rejects edits whose changed byte range intersects a replacement span;
- reverses aliases at known replacement spans from the span map;
- **reverse-maps aliases in newly added lines** (whole words and inside
  identifiers) using this file's span map plus the global alias registry, so a
  line calling `neutral_parser()` lands in the real file as
  `dangerous_parser()`. A reversal is kept only if re-sanitizing it reproduces
  the exact text the agent wrote (run-level roundtrip filter), so innocent
  identifiers that merely contain an alias-looking substring are left alone.
  An alias observed with two different originals is ambiguous and conflicts;
- applies the translated patch in memory;
- verifies the invariant in **both directions**: `sanitize(patched real) ==
  patched mirror`, and reverse-projecting the patched mirror through the fresh
  span map reproduces the patched real file byte-for-byte;
- writes a durable `applying` journal entry (temp file + fsync + rename +
  directory fsync, with per-file before/after snapshots) **before** touching
  any real file, then writes through temporary files plus rename, reindexes
  changed files, and finalizes the journal entry to `success`.

All writers (apply, sync, index, project-edit, rename, recover, verify)
serialize on a blocking `flock` at `.code-sanity/tmp/apply.lock`; the kernel
releases it automatically if a process dies, so a crash never wedges the
workspace and parallel `apply-patch`/`sync` runs stay consistent.

For create patches the added lines become the real file directly (the new file must already be neutral: `sanitize(real) == real`). For delete patches the entire mirror file must be removed, and the real file, mirror, map, and db row are all dropped.

Conflicts write `.code-sanity/journal/*.patch.json` and leave the real file unchanged. If a write or reindex step fails after real-file writes start, the changed real files are restored from the before-snapshots and the entry is marked `rolled-back`.

### Exit codes

- `0` — success;
- `2` — patch conflict (real files untouched, conflict journal written);
- `3` — `verify` found the workspace broken (every failure is printed);
- `1` — any other error.

### Crash safety and recover

Because the intent is journaled (fsync'd) before any real write, a process killed mid-apply leaves an `applying` entry on disk. `code-sanity recover` replays it to the recorded `after` state (roll-forward); `code-sanity recover --rollback` restores every touched file to its `before` state. The crashed process's lock is released by the kernel, so recover just takes the lock normally.

### Rename

Editing inside a replacement span via a normal patch is refused on purpose. `code-sanity rename --path <rel> --from <alias> --to <name>` is the sanctioned path: the sanitized alias is reversed through the span map to the real identifier, which is renamed across the real file and reindexed. The rename lands on the real symbol (real repo is the source of truth), goes through the same crash-safe journal, and is scoped to a single file.

## Current Commands

- `init`
- `index`
- `read <path>`
- `search <query> [--glob <glob>] [--max-results <n>]`
- `grep <query> [--glob <glob>] [--max-results <n>]`
- `apply-patch [--patch <file>] [--agent <name>] [--session-id <id>]`
- `write --path <path> [--sanitized-content <file>]`
- `rename --path <path> --from <alias> --to <name> [--agent <name>] [--session-id <id>]`
- `project-edit --path <path> [--agent <name>] [--session-id <id>]`
- `recover [--rollback]`
- `mode`
- `propose-sanitize [--path <path>] [--allow-provider-command]`
- `review [--approve <id>] [--reject <id>] [--all]`
- `review-sanitize [--path <path>]`
- `sh -- <cmd> [args...]`
- `strict-run -- <cmd> [args...]`
- `sync [--path <rel>] [--force]`
- `verify`
- `doctor [--agent codex|claude|opencode]`
- `install-hooks --agent codex|claude|opencode [--force]`
- `uninstall-hooks --agent codex|claude|opencode`
- `serve [--once]`

Search results are capped (default 200, hard max 1000) with an explicit truncation notice.

## MCP Server

`code-sanity serve` runs a Model Context Protocol server over stdio with tools `read_file`, `search`, `list_files`, `apply_patch`, and `verify`. Reads and search return sanitized content only; `apply_patch` projects a sanitized diff back onto the real repo through the bridge. Inspect the manifest with `code-sanity serve --once`. See [docs/MCP.md](docs/MCP.md) for Codex, Claude Code, and opencode connection config.

## Agent Adapters

### opencode

`code-sanity install-hooks --agent opencode` generates a working plugin at `.opencode/plugins/code-sanity.ts` (plus `.opencode/package.json`). It:

- redirects `read`/`grep`/`glob`/`list` tools to the sanitized mirror;
- lets `edit`/`write` land on the mirror file, then back-projects the change to the real repo with `code-sanity project-edit` followed by `sync --path` of just that file (span-aware, conflict-safe);
- in `strict` mode, blocks edits that target the real repo instead of the mirror;
- indexes at session start; `file.edited` events sync only the touched path;
- logs failures to `.code-sanity/logs/hooks.log` instead of swallowing them.

The plugin resolves the CLI as `code-sanity` on `PATH` or `$CODE_SANITY_BIN`. `project-edit` is the bridge primitive: after the agent edits a mirror file in place, it refreshes the baseline from `sanitize(real)` and drives the difference through the patch bridge. Hooks are a guardrail, not a hard boundary — reads via `bash`/other tools are not intercepted; hard isolation requires strict-mode worktree isolation.

### Codex

`code-sanity install-hooks --agent codex` writes `.codex/hooks.json` plus `.codex/hooks/{pre,post}_tool_use.py`. The `PreToolUse` guard:

- denies raw real-repo edits (`apply_patch`/`edit`/`write`) in strict mode and steers to the code_sanity MCP `apply_patch` tool;
- nudges toward `apply_patch` in guided mode;
- best-effort redirects obvious shell reads (`cat`/`nl`/`head`/`tail <file>`) to `code-sanity read` via `updatedInput`;
- always allows `code_sanity` MCP tools and mirror-targeted edits.

`PostToolUse` matches edit tools only (not `*`): mirror edits are back-projected with `project-edit` first, then only the touched path is synced (`sync --path`); errors are logged to `.code-sanity/logs/hooks.log`. Pair with the [MCP server](docs/MCP.md). Codex `PreToolUse` is explicitly a guardrail, not a full enforcement boundary — it does not intercept every shell path.

`install-hooks` merges into an existing `hooks.json`/`settings.json` (foreign keys and hooks are preserved, a `.bak` backup is written, reinstalling is idempotent); `--force` replaces an unparseable config. `uninstall-hooks` removes only code-sanity's entries and scripts.

### Claude Code

`code-sanity install-hooks --agent claude` merges hook entries into `.claude/settings.json` (preserving existing settings, with a `.bak` backup) and writes `.claude/hooks/{pre_tool_use,post_tool_use,session_start}.py`. The `PreToolUse` guard denies raw real-repo `Read`/`Edit`/`Write` in strict mode (guided denies edits) and steers to the code-sanity MCP server; `SessionStart` injects guidance to use the code-sanity tools; `PostToolUse` back-projects mirror edits (`project-edit`) and syncs only the touched path. Register the MCP server as in [docs/MCP.md](docs/MCP.md). Hooks read the enforcement mode from `.code-sanity/config.toml`, log failures to `.code-sanity/logs/hooks.log`, and are a guardrail, not a transparent read rewrite.

## Strict mode

Two runners hide real names from a command, including in build/test output, via a reverse map (alias → shown, so real originals are replaced with their sanitized aliases in stdout/stderr):

```bash
# Run in the real repo (so the build actually compiles/passes), sanitize output:
code-sanity sh -- cargo test

# Run inside a fresh sanitized worktree (the process reads only sanitized files):
code-sanity strict-run -- cat src/lib.rs
```

`sh` runs the command in the real repo and reverse-maps its stdout/stderr, so a real compiler/test error shows sanitized identifiers. `strict-run` first copies the mirror into a fresh worktree outside the repo tree — a unique per-run directory with `0700` permissions, removed afterwards — and runs there, so even a raw `cat`/`grep` only sees sanitized content. Both propagate the child exit code.

Output is **streamed** line by line as the child produces it (a long build shows progress immediately) and rewritten with a leftmost-longest Aho-Corasick automaton built from the span maps, dictionary, registry, and denylist.

These are guardrails, not a hard sandbox: absolute paths, network, or escaping the worktree can still reach the real repo. True isolation needs an overlay/FUSE/container (optional; not implemented). Known bypasses are catalogued in [docs/THREAT_MODEL.md](docs/THREAT_MODEL.md).

## Docs

- [docs/MCP.md](docs/MCP.md) — connect Codex, Claude Code, and opencode to the MCP server.
- [docs/HOOKS_MATRIX.md](docs/HOOKS_MATRIX.md) — per-adapter capability matrix and why hooks are guardrails.
- [docs/THREAT_MODEL.md](docs/THREAT_MODEL.md) — assets, enforcement tiers, known bypasses, and guarantees.

## Safety Notes

This tool is for lexical normalization and privacy reduction, not for hiding real behavior. The sanitizer should not rewrite control flow, imports, public APIs, auth semantics, dangerous APIs, protocol strings, SQL, shell commands, or other behavior-bearing text.

Known bypasses and residual risks are catalogued in [docs/THREAT_MODEL.md](docs/THREAT_MODEL.md).

Hooks are not a complete enforcement boundary. Strict protection requires running agents inside the sanitized mirror or an overlay/sandbox where raw repository reads are physically unavailable.

## Verify

`code-sanity verify` checks every tracked file (`sanitize(real) == mirror`,
hashes, replacement counts) and additionally runs an **independent leak
backstop**: the mirror and every span-map replacement output are scanned with
the same matching primitive the sanitizer uses; any dictionary / denylist /
registry term whose enclosing identifier is not in the repo-wide protected set
is a failure, as is any file in the mirror that nothing tracks. Failures are
printed one per line and the process exits with code `3`.

## Known Limitations

- Tokenization is regex/byte-scanner based, not AST-aware.
- Multi-file apply is journaled (fsync'd) before writes, serialized by `flock`, and recoverable via `recover`, but it is not a substitute for transactional filesystem commits.
- Patch back-projection is span-aware for known replacement spans and reverse-maps aliases in added lines, but hunk coordinate remapping is line-oriented and edits *inside* an alias still conflict; use `rename` to change a symbol behind an alias.
- `rename` is single-file scoped; it does not chase references across files.
- Protected-identifier detection (public API, imports) is conservative lexical heuristics, not a language-aware symbol graph; matching is ASCII-oriented (non-ASCII terms are not matched).
- Term matching is deliberately aggressive (case- and underscore-insensitive substrings inside word runs), so a term embedded in an unrelated word is also replaced; keep the allowlist current.
- `.gitignore` support is delegated to the `ignore` crate (full gitignore language, `require_git(false)`); the walker does not follow parent-directory or global gitignores, for determinism.
- The opencode plugin, MCP server, and Codex/Claude hooks are working guardrail adapters, not hard boundaries; they do not intercept reads via `bash` or other non-file tools.
- Codex/Claude hooks require `python3` on the host.
- The model-based sanitizer is proposal-only: an external provider must be supplied as a `command` and confirmed with `--allow-provider-command`; there is no bundled LLM. The deterministic engine (dictionary + alias registry + denylist) always does the actual sanitization.
- Strict mode (`sh`/`strict-run`) is a guardrail, not a hard sandbox; FUSE/overlay isolation is not implemented. Output sanitization covers terms present in the span maps/dictionary/registry/denylist; novel real names in output are not hidden.

## Development

```bash
cargo test          # full suite, including the 5k-file incremental index
cargo fmt --check   # CI-enforced
cargo clippy --all-targets -- -D warnings
```

CI (GitHub Actions) runs fmt + clippy + tests on every push and pull request.

The test suite covers indexing (including incremental fingerprints and a parallel apply/sync stress run), sanitized read/search with result caps, path traversal rejection, span map offsets, bidirectional patch roundtrips (plus a property test over random files and patches), alias reverse-mapping in added lines, public API consistency, conflicts inside replacements, rollback on simulated multi-file apply failure, verify's leak backstop and exit codes, hook generation/merging/uninstall, strict-mode streaming and sanitization, the review pipeline, and CLI smoke flows.

## License

MIT — see [LICENSE](LICENSE).
