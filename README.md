# code-sanity

`code-sanity` builds a sanitized mirror of a real repository and applies agent edits from that mirror back to the real files. The real repository remains the source of truth; `.code-sanity/mirror` is the agent-facing view, and `.code-sanity/maps` plus `db.sqlite` hold span and hash state.

Sanitization is deterministic and local (dictionary + human-approved alias registry + denylist). A model can *propose* aliases through a provider interface, but it never writes the mirror.

## Installation

Prebuilt binaries for Linux and macOS (x86_64 / aarch64) are attached to each
[GitHub Release](https://github.com/gfhfyjbr/code-sanity/releases):

```bash
# pick your platform: x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu,
#                     x86_64-apple-darwin, aarch64-apple-darwin
version=v0.3.0
target=aarch64-apple-darwin
curl -fsSLO "https://github.com/gfhfyjbr/code-sanity/releases/download/${version}/code-sanity-${version}-${target}.tar.gz"
curl -fsSLO "https://github.com/gfhfyjbr/code-sanity/releases/download/${version}/code-sanity-${version}-${target}.tar.gz.sha256"
shasum -a 256 -c "code-sanity-${version}-${target}.tar.gz.sha256"
tar xzf "code-sanity-${version}-${target}.tar.gz"
install -m 755 "code-sanity-${version}-${target}/code-sanity" ~/.local/bin/
```

Or build from source (Rust ≥ 1.85):

```bash
cargo install --git https://github.com/gfhfyjbr/code-sanity --locked
# or from a checkout:
cargo install --path . --locked
```

Linux and macOS only — workspace locking is `flock`-based and the build refuses
other platforms.

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
- terms must be single word-run tokens: entries containing `.`/`@`/spaces
  (emails, hostnames, `com.acme.Foo`) can never match and are **rejected at
  config load** with a fix-it message — split them into per-word entries;
- aliases must be **collision-free**: an alias that occurs naturally in the
  real repo (or is shared by two terms) makes the mirror ambiguous and is a
  hard error at index/verify/approve time. Default dictionary aliases carry a
  per-workspace salted suffix (`neutral_3fd1`-style) so natural collisions are
  practically impossible;
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
- the per-workspace salt is 128-bit, read from `/dev/urandom` at `init`.

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

1. `code-sanity propose-sanitize [--path <path>]` runs the configured proposal provider. The default is a deterministic `HeuristicProposalProvider` (proposes neutral aliases for denylisted terms). Set `provider.kind = "external"` with a `command` (and optional `timeout_secs`) to plug in a local model; it receives `{rel, content}` JSON on stdin and returns a `ProposalBatch`. Because the command comes from repo-local config, executing it requires explicit confirmation with `--allow-provider-command`; stdin/stdout are pumped concurrently (no pipe deadlock on large files) and the child is killed on timeout. Set `provider.kind = "llm"` to use any OpenAI-compatible chat endpoint instead — e.g. a local [kou-router](https://github.com/gfhfyjbr/kou-router) gateway that fans out to OpenAI/Anthropic/Ollama accounts:

   ```toml
   [sanitizer.provider]
   kind = "llm"
   base_url = "http://127.0.0.1:20128/v1"
   model = "claude-sonnet-5"          # any model the gateway routes
   api_key_env = "KOU_ROUTER_API_KEY" # key read from env, never from config
   timeout_secs = 120
   ```

   Two presets skip the boilerplate — `kind = "kou-router"` (defaults: `base_url = "http://127.0.0.1:20128/v1"`, `api_key_env = "KOU_ROUTER_API_KEY"`) and `kind = "openrouter"` (defaults: `base_url = "https://openrouter.ai/api/v1"`, `api_key_env = "OPENROUTER_API_KEY"`); each accepts the same optional `base_url`/`api_key_env`/`timeout_secs` overrides, and `kind = "llm"` remains for any other OpenAI-compatible endpoint:

   ```toml
   [sanitizer.provider]
   kind = "openrouter"                    # or "kou-router"
   model = "anthropic/claude-sonnet-4.5"  # export OPENROUTER_API_KEY=sk-or-...
   ```

   The model receives the real file plus the current policy (deny/allow lists, already-mapped terms) and must answer with a strict-JSON `ProposalBatch`. Because the endpoint comes from repo-local config **and receives real file content**, running it requires explicit confirmation with `--allow-provider-endpoint` — for all three kinds, including the loopback kou-router preset. Point `base_url` at a local gateway/Ollama to keep real code on the machine; a remote endpoint (OpenRouter included) sees exactly the content you are trying to sanitize. A remote endpoint with no API key in the environment fails fast with the variable name instead of an HTTP 401 mid-run.
2. Each proposal is validated: the original must appear in the file, allowlisted terms are refused, identifier aliases must be valid identifiers, aliases may not introduce newlines or contain a denylisted term. Survivors are queued under `.code-sanity/review/`; anything touching a public API name or below `confidence_threshold` is flagged for review.
3. `code-sanity review [--all]` lists the queue. `review --approve <id>` records the alias in the deterministic registry (`sanitizer.alias_registry` in `config.toml`) and reindexes the file; `review --reject <id>` drops it. Approval re-validates so a stale queue can't apply an unsafe alias.
4. `index`/`verify` use only the deterministic engine (dictionary + alias registry), so they stay reproducible and the model stays out of the write path.

`code-sanity review-sanitize [--path <path>]` prints an audit of every applied replacement (category, original → sanitized, policy source, confidence, line) read from the span maps.

## Semantic index (embeddings)

An optional vector index over the **sanitized mirror** gives agents semantic search next to the literal `search`/`grep`. It follows the same incremental component model as the file index: every mirror file owns its chunk/vector rows and is re-embedded only when its mirror content hash or the embed fingerprint (model + chunker version + chunk parameters) changes; a deleted file takes its vectors with it. Vectors and chunk texts live in the existing `db.sqlite`.

```toml
[embeddings]
enabled = true
base_url = "https://openrouter.ai/api/v1"     # any OpenAI-compatible /embeddings
model = "openai/text-embedding-3-small"
api_key_env = "OPENROUTER_API_KEY"            # key read from env, never from config
chunk_lines = 60
chunk_overlap = 10
batch_size = 32
timeout_secs = 120
```

```bash
export OPENROUTER_API_KEY=sk-or-...
code-sanity embed-index                  # incremental; unchanged files cost no HTTP
code-sanity semantic-search "where is retry logic for the parser" --k 10
```

Only sanitized mirror content is ever sent to the embedding endpoint — the same text agents already read — so enabling OpenRouter leaks no real names. The default endpoint is OpenRouter's OpenAI-compatible `/embeddings`; a local [kou-router](https://github.com/gfhfyjbr/kou-router) gateway or any other OpenAI-compatible endpoint works via `base_url`. Mirror files are snapshotted under short-lived shared locks and embedding requests run unlocked, so a slow endpoint never starves writers; each file's chunk rows then commit in one SQLite transaction under a brief exclusive workspace lock that re-verifies the mirror still matches the embedded snapshot (files that changed mid-run are reported as `stale` and reconciled by the next run). Run `embed-index` after `index`/`sync` to pick up re-rendered files (stale vectors are self-healing on the next run). The MCP server exposes the same search as a `semantic_search` tool.

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
- `64` — command-line usage error (unknown flags/subcommand);
- `1` — any other error.

**Exception:** `sh` and `strict-run` propagate the wrapped command's exit code
verbatim, so a child that exits 2/3/64 produces those codes with none of the
meanings above. The contract applies only to code-sanity's own commands.

### Machine-readable output (`--json`)

Every command except `sh`/`strict-run`/`serve` accepts a global `--json` flag
and then writes **exactly one compact JSON document to stdout** — success or
failure — while stderr stays free-form human diagnostics outside the contract.
Exit codes are unchanged.

```json
{"ok":true,"command":"index","data":{"indexed":42,"unchanged":0,"...":"..."},"elapsed_ms":840}
{"ok":false,"command":"apply-patch","error":{"kind":"conflict","message":"...","journal_path":"..."}}
```

- `error.kind` is `conflict` (exit 2, with `journal_path`), `verify_failed`
  (exit 3, with `checked` and `failures`), or `error` (exit 1).
- Consumers must ignore unknown fields: fields are added over time, never
  renamed or retyped.
- A clap usage error (exit 64) is reported before `--json` is parsed and is
  never JSON.
- `read --json` wraps the file as `{"path","content"}` (byte-faithful inside
  the JSON string); `sh`/`strict-run`/`serve` refuse the flag (exit 64)
  because their stdout (and for the wrappers, the exit code) belongs to the
  wrapped command or the MCP protocol stream.

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
- `apply-patch [--patch <file>] [--dry-run] [--agent <name>] [--session-id <id>]` (`--dry-run` plans and validates without writing; conflicts still exit 2)
- `write --path <path> [--sanitized-content <file>]`
- `rename --path <path> --from <alias> --to <name> [--agent <name>] [--session-id <id>]`
- `project-edit --path <path> [--agent <name>] [--session-id <id>]`
- `recover [--rollback] [--force]` (`--force` overwrites files whose content changed after the crash)
- `mode`
- `propose-sanitize [--path <path>] [--allow-provider-command] [--allow-provider-endpoint]`
- `review [--approve <id>] [--reject <id>] [--all]`
- `review-sanitize [--path <path>]`
- `sh -- <cmd> [args...]`
- `strict-run -- <cmd> [args...]`
- `sync [--path <rel>] [--force]`
- `embed-index`
- `semantic-search <query> [--k <n>]`
- `verify`
- `doctor [--agent codex|claude|opencode]`
- `install-hooks --agent codex|claude|opencode [--force]`
- `uninstall-hooks --agent codex|claude|opencode`
- `serve [--once]`

Search results are capped (default 200, hard max 1000) with an explicit truncation notice.

`--glob` uses gitignore-style dispatch: a pattern without `/` matches file **names** at any depth (`*.rs` = every Rust file); a pattern with `/` matches the repo-relative **path**, with `*` stopping at separators and `**` crossing them (`src/*.rs`, `src/**`, `**/*.rs`). Invalid patterns are an error.

## MCP Server

`code-sanity serve` runs a Model Context Protocol server over stdio with tools `read_file`, `search`, `list_files`, `semantic_search`, `apply_patch`, and `verify`. Reads and search return sanitized content only; `apply_patch` projects a sanitized diff back onto the real repo through the bridge. Inspect the manifest with `code-sanity serve --once`. See [docs/MCP.md](docs/MCP.md) for Codex, Claude Code, and opencode connection config.

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
- [docs/ROADMAP.md](docs/ROADMAP.md) — prioritized tasks to production readiness (release engineering, hardening, performance backlog).

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
- The model-based sanitizer is proposal-only: an external provider (a `command` confirmed with `--allow-provider-command`, or an OpenAI-compatible endpoint confirmed with `--allow-provider-endpoint`) must be supplied; there is no bundled LLM. The deterministic engine (dictionary + alias registry + denylist) always does the actual sanitization.
- The `llm`/`openrouter`/`kou-router` proposal providers post real file content to the configured endpoint; keep it local (kou-router/Ollama) unless you accept that exposure. Embedding requests carry sanitized mirror content only.
- Semantic search is brute-force cosine over all stored vectors (no ANN index); fine for tens of thousands of chunks, not millions. Vectors go stale between `index` and the next `embed-index` run (self-healing, hash-keyed).
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
