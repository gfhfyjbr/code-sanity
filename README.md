# code-sanity

[![CI](https://github.com/gfhfyjbr/code-sanity/actions/workflows/ci.yml/badge.svg)](https://github.com/gfhfyjbr/code-sanity/actions/workflows/ci.yml)

`code-sanity` builds a sanitized mirror of a real repository and applies agent edits from that mirror back to the real files. The real repository remains the source of truth; `.code-sanity/mirror` is the agent-facing view, and `.code-sanity/maps` plus `db.sqlite` hold span, path-projection, and hash state. Agent-facing directory names and filename stems use deterministic shared and path-only aliases, so a known or reviewed provocative filename does not keep framing otherwise benign work.

Sanitization is deterministic and local (dictionary + human-approved content/path alias registries + denylist). A model can *propose* aliases through a provider interface, but it never writes the mirror.

## Installation

The installer detects macOS/Linux and x86_64/aarch64, downloads the matching
binary from the latest GitHub Release, verifies its SHA-256 checksum, and places
it in `${CARGO_HOME:-$HOME/.cargo}/bin` without `sudo` or a Rust toolchain. If
zsh is available, it also generates `_code-sanity` from the installed binary,
places it in `${ZDOTDIR:-$HOME}/.zfunc`, and idempotently adds that directory to
`fpath` in `.zshrc`:

```bash
curl -fsSL https://raw.githubusercontent.com/gfhfyjbr/code-sanity/main/install.sh | bash

# Pin a release or customize installation:
curl -fsSL https://raw.githubusercontent.com/gfhfyjbr/code-sanity/main/install.sh | \
  bash -s -- --version v0.5.0 --bin-dir "$HOME/.local/bin" --add-to-path
```

Download and inspect `install.sh` first if piping a remote script is outside
your trust policy. From a checkout, `./install.sh --from-source` performs the
old optimized Cargo build; `--no-build` installs an existing
`target/release/code-sanity`. Use `--zsh-completions-dir <dir>` to select a
completion directory or `--no-zsh-completions` to leave shell configuration
untouched. Uninstall removes both the generated completion and the installer's
managed `.zshrc` block. Run `./install.sh --help` for all environment overrides.
Installation is atomic and verifies both the release archive and installed
binary before returning success.

The same completion can be generated without the installer:

```bash
mkdir -p ~/.zfunc
code-sanity completions zsh > ~/.zfunc/_code-sanity
```

For a manual setup, ensure `~/.zfunc` is added to `fpath` before `compinit` is
called. The installer manages that ordering and registration automatically.

Prebuilt binaries for Linux and macOS (x86_64 / aarch64) are attached to each
[GitHub Release](https://github.com/gfhfyjbr/code-sanity/releases):

```bash
# pick your platform: x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu,
#                     x86_64-apple-darwin, aarch64-apple-darwin
version=v0.5.0
target=aarch64-apple-darwin
curl -fsSLO "https://github.com/gfhfyjbr/code-sanity/releases/download/${version}/code-sanity-${version}-${target}.tar.gz"
curl -fsSLO "https://github.com/gfhfyjbr/code-sanity/releases/download/${version}/code-sanity-${version}-${target}.tar.gz.sha256"
shasum -a 256 -c "code-sanity-${version}-${target}.tar.gz.sha256"
tar xzf "code-sanity-${version}-${target}.tar.gz"
install -m 755 "code-sanity-${version}-${target}/code-sanity" ~/.local/bin/
```

The equivalent manual source installation (Rust ≥ 1.85) is:

```bash
cargo install --git https://github.com/gfhfyjbr/code-sanity --locked
# or from a checkout:
cargo install --path . --locked
```

Provider keys can be exported normally or placed in `<workspace>/.env`, for
example `OPENROUTER_API_KEY=...`. Existing process environment variables take
precedence over the file. The exact `.env` file is added to `.gitignore` by
`code-sanity init` and is always excluded from indexing and provider payloads.

Linux and macOS only — workspace locking is `flock`-based and the build refuses
other platforms.

Release tags are built by GitHub Actions for all four supported targets. The
pipeline verifies the tag against `Cargo.toml`, runs tests and dependency policy
checks, publishes checksummed archives, then installs the published release on
fresh Linux and macOS runners. See [docs/RELEASING.md](docs/RELEASING.md).

## Quick Start

```bash
cargo run -- init
cargo run -- index
cargo run --                 # open the interactive workspace
cargo run -- read src/lib.rs
cargo run -- search neutral_parser
cargo run -- grep neutral_parser
cargo run -- verify
```

### Interactive workspace

Running `code-sanity` without a subcommand opens the full-screen terminal UI.
It combines the pending review queue, proposal details, real-source context,
workspace status, live operation progress, and an activity log. Tabs, review
rows, and action buttons support mouse clicks and scrolling.

Press `:` to open the command palette. It provides completion with `Tab` and
history with `Up`/`Down`; available commands include `index`, `verify`,
`propose [path] -j N`, `review [all]`, `approve [id]`, `reject [id]`,
`filter <text>`, `tab review|activity|workspace`, `refresh`, and `quit`.
Keyboard accelerators (`i`, `v`, `p`, `a`, `r`, `/`) run the same
command engine. Provider scans and review decisions use confirmation dialogs,
while long operations run in the background so input and mouse handling stay
live. Pending review rows have `[ ]`/`[X]` approval checkboxes: press `Space`
or click the checkbox to toggle one proposal, and use the button below the
queue to `Select All` or `Deselect All` proposals in the current filtered view.
`Approve` processes every checked proposal; with no checkboxes selected it
keeps the focused-row behavior. A checked batch shares one deterministic
preflight, one language-server session, and one atomic apply pass; the activity
panel shows the current validation stage and completed compiler closures.

The `Propose` toolbar button and `p` shortcut open a setup modal before any
provider call. Its directory dropdown is built from indexable repository files
(`Entire workspace` is always available), including before the first manual
index. A first proposal scan populates the derived index before resolving that
scope. Endpoint-backed providers also require an explicit `Allow provider
endpoint` checkbox before `Run` is enabled; the modal shows the configured URL
and model receiving the selected scope's real source.

Human CLI output uses colors, spinners, summaries, and result tables when
attached to a terminal. Redirected output retains the stable plain-text format;
`--json` remains undecorated machine-readable output. `read`, `sh`,
`strict-run`, and `serve` keep their raw stream contracts.

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
- every component of the agent-facing repo-relative path is projected with the
  effective path term table: reviewed path-only aliases take precedence, then
  the ordinary dictionary/content registry/denylist mappings apply. Directory
  names and filename stems are sanitized while the final extension is
  preserved for language/tool dispatch
  (`dangerous/client_dangerous.mm` may become
  `neutral_x1/consumer_y2_neutral_x1.mm`);
- path projection is reversible over the complete tracked workspace. Indexing
  fails before writing if two real files/directories would collapse to one
  projected path, including ASCII case-insensitive collisions;
- the **repo-wide protected identifier set** (public declarations,
  import-position names, and code dunders like `__init__`, collected from the
  real files' code contexts — never from prose, comments, or string literals)
  is the only sanctioned residue: one symbol gets one decision across the
  whole mirror, so a `pub fn` keeps its name at every call site in every file;
- `'` is not a string delimiter in Rust/Go, so lifetimes (`&'a str`) cannot
  open phantom strings that would suppress sanitization;
- zone detection (comment/string/identifier) only labels the replacement
  category — it can never suppress a replacement;
- line count is preserved, but replacement lengths may differ;
- the per-workspace salt is 128-bit, read from `/dev/urandom` at `init`.

Every tracked file gets a JSON span map with its internal real identity, current
agent-facing projected path, original and sanitized byte offsets, line starts,
hashes, replacement spans, and rendered sizes. `code-sanity project-path
<real-or-projected-path>` prints the current agent-facing spelling for adapters
and diagnostics.

### Incremental index

Every file is a component owning its mirror file, span map, and db rows. A file
is re-rendered only when its **input fingerprint** (content sha256, with an
mtime/size pre-check that avoids reading unchanged files) or the **logic
fingerprint** (dictionary, registry, allow/deny lists, salt, sanitizer/path
behavior versions, and the repo-wide protected symbol set) changes. A file that
disappeared takes its targets with it. Each file commits in a single SQLite
transaction with idempotent upserts; the database runs in WAL mode with a busy
timeout and is fully derived state (`PRAGMA user_version` migrations recreate
it). An unchanged 5k-file repo re-indexes in well under a second; editing one
file re-renders exactly one file.

Mirror files holding a **pending agent edit** (mirror on disk differs from the
last indexed sanitized hash) are never clobbered by `sync`/`index`; only the
patch bridge resets them after projecting the edit. A mirror file with **no
database row at all** (a deleted `db.sqlite`, or a crash before its first
commit) counts as pending too — the row's absence cannot prove the on-disk
mirror is our render. `sync --force` is the recovery path: it re-renders
everything and resets pending (or tampered) mirror files back to
`sanitize(real)`, stashing each discarded edit under
`.code-sanity/journal/discarded/` first.

## Model-based sanitizer

The model never writes the mirror. It runs only in an offline *propose* step;
its output is validated, queued, and applied deterministically. Every provider
receives the current projected repo-relative path rather than the internal real
path. A still-unknown term can therefore remain visible until its path proposal
is reviewed.

1. `code-sanity propose-sanitize [--path <file-or-directory>]` runs the
   configured provider over two independent candidate surfaces:
   `context.semantic_candidates` contains owned symbols, while
   `context.path_candidates` contains each current projected directory
   component and filename stem. Semantic output uses
   `category: "identifier"` with `target: {symbol_id, occurrence_id}`; path
   output uses `category: "file_path"` with `target: {path_id}`. The same
   spelling may receive one, both, or neither decision. Public third-party
   brands, products, OS components, protocols, frameworks, and library APIs
   remain out of scope.

   LLM source is split on complete lines at
   `sanitizer.propose_chunk_bytes`, with
   `propose_chunk_overlap_lines` of read-only `context_before`. Path metadata is
   scanned in a dedicated `request_mode: "path-only"` pass: directory and
   filename-stem candidates are deduplicated across the selected scope and
   batched by `sanitizer.propose_path_batch_size`. If a source exceeds
   `sanitizer.propose_max_file_bytes`, its body and semantic candidates are not
   sent, but its path remains in that shared inventory. Semantic decisions use
   exact `symbol_id` identity; same-spelling symbols remain independent, and a
   locally rejected alias does not suppress a later alternative. Unresolved
   occurrences suppress a local candidate only inside its enclosing function;
   a non-local C++/ObjC++ candidate remains eligible when clangd can perform the
   mandatory approval-time closure. A malformed
   proposal object cannot discard valid siblings, and one wholly invalid JSON
   response is retried once. Full scans use four concurrent
   requests by default (`sanitizer.propose_concurrency`, range 1–32), or
   `--jobs <n>` for one run. Progress goes to stderr; `--json` keeps stdout as
   one JSON document and `--no-progress` disables live updates.

   The default provider is the deterministic `HeuristicProposalProvider` for
   denylisted source terms. Set `provider.kind = "external"` with a `command`
   and optional `timeout_secs` to plug in a local model. During a proposal run
   it receives `{request_mode, rel, content, chunk, context}` JSON on stdin and returns a
   `ProposalBatch`; executing a repo-local command requires
   `--allow-provider-command`, uses concurrent pipe I/O, and is killed on
   timeout. Set `provider.kind = "llm"` to use any OpenAI-compatible chat
   endpoint — for example, a local
   [kou-router](https://github.com/gfhfyjbr/kou-router) gateway:

   ```toml
   [sanitizer]
   propose_concurrency = 4
   propose_chunk_bytes = 16384
   propose_chunk_overlap_lines = 12
   propose_path_batch_size = 64

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

   Before the provider boundary, known deterministic content terms are
   pre-redacted. Source requests receive the remaining real source and
   file-local external-ownership evidence; path-only requests receive only the
   projected path inventory. `allowlist` controls deterministic content,
   `proposal_allowlist` controls semantic candidates, and `path_allowlist`
   controls path candidates. The model must return strict JSON. Running an
   endpoint provider requires
   `--allow-provider-endpoint`, including the loopback kou-router preset. A
   missing remote API key fails before any request.
2. Identifier `original_text` must equal its owned symbol name and occur in the
   current source chunk. File-path `original_text` must instead be an exact,
   case-sensitive substring of the selected path candidate; extensions are
   never candidates. Local validation rejects invented/stale IDs, allowlisted
   or already-mapped terms, public/external identifiers, unsafe aliases, and
   path aliases that would make any tracked file or directory projection
   collide. Low confidence is flagged for review, never auto-applied.
3. `code-sanity review [--all]` lists the queue. Approving an identifier writes
   a symbol-scoped semantic alias. A function-local symbol can be proven closed
   by the lexical resolver; every non-local Rust or C/C++/Objective-C-family
   symbol first requires a stable compiler/LSP reference set. The accepted
   closure links declarations and uses across files and refuses a partial
   result. Source drift marks the group stale and `index` must successfully
   refresh the same decision through the language server before projecting it
   again. Header declarations and their implementation definitions are one
   alias owner even when clangd returns only the opened declaration. `index`
   deterministically reconverges aliases accepted by older versions onto the
   header's authoritative spelling and reports the repaired anchor count.
   Bulk review first refreshes stale source/policy/mirror or resolver state,
   quarantines unsafe legacy aliases and broken compiler components, then
   preflights deterministic ownership/collisions before starting compiler work.
   It retires only invalid, conflicting, obsolete, or incomplete selections,
   batches reference requests through one language-server session, proves
   implementation-local `static` closures from the semantic index, atomically
   admits the surviving closure batch, and writes aliases, mirrors, review
   files, and the ledger once. Pending mirror edits stop the operation before
   any review decision is written.
   Approving `file_path` writes a global
   path-only entry under `sanitizer.path_alias_registry`, revalidates the
   complete path map, and migrates the projected mirror path; it does not
   rewrite source content or rename the real repository file. Rejecting changes
   no policy. Approval always revalidates stale queue items.
4. `index` and `verify` use only the deterministic dictionary, registries, and
   denylist. The model remains outside the write path.

`code-sanity review-sanitize [--path <path>]` prints the span-map audit for
applied content replacements (category, original → sanitized, policy source,
confidence, line). Path-only decisions remain visible in review history and
`sanitizer.path_alias_registry`; `project-path` shows their current result.

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

Only sanitized mirror content is ever sent to the embedding endpoint — the same text agents already read — so enabling OpenRouter leaks no real names. The default endpoint is OpenRouter's OpenAI-compatible `/embeddings`; a local [kou-router](https://github.com/gfhfyjbr/kou-router) gateway or any other OpenAI-compatible endpoint works via `base_url`. Mirror files are snapshotted under short-lived shared locks and embedding requests run unlocked, so a slow endpoint never starves writers; each file's chunk rows then commit in one SQLite transaction under a brief exclusive workspace lock that re-verifies the mirror still matches the embedded snapshot (files that changed mid-run are reported as `stale` and reconciled by the next run). Run `embed-index` after `index`/`sync` to pick up re-rendered files. Search fails closed before making an HTTP request if any tracked mirror fingerprint is stale. The MCP server exposes the same search as a `semantic_search` tool.

## Patch Bridge

`apply-patch` accepts unified diffs against projected sanitized paths such as
`a/src/neutral_worker.rs`, `b/src/neutral_worker.rs`, or
`.code-sanity/mirror/src/neutral_worker.rs`. Modify, create (`--- /dev/null`),
and delete (`+++ /dev/null`) patches are all supported; existing paths are
reverse-mapped to their real identities before any write.

Before writing real files it:

- checks real and mirror hashes against SQLite state;
- applies the patch to the current mirror in memory;
- rejects edits whose changed byte range intersects a replacement span;
- reverses aliases at known replacement spans from the span map;
- **reverse-maps aliases in newly added lines** (whole words and inside
  identifiers) using the file span map, global lexical registry, and accepted
  workspace semantic aliases, so a
  line calling `neutral_parser()` lands in the real file as
  `dangerous_parser()`. A reversal is kept only if re-sanitizing it reproduces
  the exact text the agent wrote (run-level roundtrip filter), so innocent
  identifiers that merely contain an alias-looking substring are left alone.
  An alias observed with two different originals is ambiguous and conflicts.
  Reusing an existing alias as a new declaration also conflicts, while a
  reference to that alias is back-projected to the real symbol;
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

Create patches use the same syntax-aware back-projection as added lines in an
existing file: references to accepted aliases land as real symbol names, while
new declarations that reuse an existing alias are rejected as ambiguous. The
newly indexed mirror must then reproduce the agent's file byte-for-byte or the
whole journaled apply is rolled back. For delete patches the entire mirror file
must be removed, and the real file, mirror, map, and db row are all dropped.

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

- `code-sanity` (no subcommand: interactive workspace)
- `init`
- `index`
- `read <path>`
- `project-path <path>`
- `search <query> [--glob <glob>] [--max-results <n>]`
- `grep <query> [--glob <glob>] [--max-results <n>]`
- `apply-patch [--patch <file>] [--dry-run] [--agent <name>] [--session-id <id>]` (`--dry-run` plans and validates without writing; conflicts still exit 2)
- `write --path <path> [--sanitized-content <file>]`
- `rename --path <path> --from <alias> --to <name> [--agent <name>] [--session-id <id>]`
- `project-edit --path <path> [--agent <name>] [--session-id <id>]`
- `recover [--rollback] [--force]` (`--force` overwrites files whose content changed after the crash)
- `mode`
- `propose-sanitize [--path <path>] [--jobs <1..32>] [--no-progress] [--allow-provider-command] [--allow-provider-endpoint]`
- `review [--approve <id>] [--reject <id>] [--all]`
- `review-sanitize [--path <path>]`
- `sh -- <cmd> [args...]`
- `strict-run -- <cmd> [args...]`
- `sync [--path <rel>] [--force]`
- `embed-index`
- `semantic-search <query> [--k <n>]`
- `workspace-snapshot`
- `find-code <query> [--limit <n>]`
- `read-code <path>`
- `edit-node --node-id <id> --replacement <text> --expected-revision <n>`
- `rename-symbol --symbol-id <id> --new-name <name> --expected-revision <n>`
- `preview-transaction --expected-revision <n> [--intents <json-file>]`
- `commit-transaction <id> --expected-revision <n> [--agent <name>] [--session-id <id>]`
- `verify`
- `doctor [--agent codex|claude|opencode]`
- `install-hooks --agent codex|claude|opencode [--force]`
- `uninstall-hooks --agent codex|claude|opencode`
- `completions zsh`
- `serve [--once]`

Search results are capped (default 200, hard max 1000) with an explicit truncation notice.

`--glob` uses gitignore-style dispatch over projected paths: a pattern without
`/` matches file **names** at any depth (`*.rs` = every Rust file); a pattern
with `/` matches the projected repo-relative **path**, with `*` stopping at
separators and `**` crossing them (`src/*.rs`, `src/**`, `**/*.rs`). Invalid
patterns are an error.

## MCP Server

`code-sanity serve` runs a Model Context Protocol server over stdio. The preferred v2 surface is `workspace_snapshot`, `find_code`, `read_code`, `references`, `edit_node`, `rename_symbol`, `preview_transaction`, `commit_transaction`, and `verify`. These tools use stable AST/symbol IDs, compiler/LSP rename, and revision-checked transactions. Legacy mirror tools remain available for compatibility. Inspect the manifest with `code-sanity serve --once`; see [docs/MCP.md](docs/MCP.md) and [docs/SEMANTIC_V2.md](docs/SEMANTIC_V2.md).

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
- [docs/SEMANTIC_V2.md](docs/SEMANTIC_V2.md) — AST identities, language capabilities, proposal schema, and transaction protocol.
- [docs/HOOKS_MATRIX.md](docs/HOOKS_MATRIX.md) — per-adapter capability matrix and why hooks are guardrails.
- [docs/THREAT_MODEL.md](docs/THREAT_MODEL.md) — assets, enforcement tiers, known bypasses, and guarantees.
- [docs/ROADMAP.md](docs/ROADMAP.md) — prioritized tasks to production readiness (release engineering, hardening, performance backlog).

## Safety Notes

This tool is for lexical normalization and privacy reduction, not for hiding real behavior. The sanitizer should not rewrite control flow, imports, public APIs, auth semantics, dangerous APIs, protocol strings, SQL, shell commands, or other behavior-bearing text.

Known bypasses and residual risks are catalogued in [docs/THREAT_MODEL.md](docs/THREAT_MODEL.md).

Hooks are not a complete enforcement boundary. Strict protection requires running agents inside the sanitized mirror or an overlay/sandbox where raw repository reads are physically unavailable.

## Verify

`code-sanity verify` checks every tracked file (the combined lexical + semantic
projection equals the mirror, hashes and replacement counts agree) and
additionally runs an **independent leak
backstop**: the mirror and every span-map replacement output are scanned with
the same matching primitive the sanitizer uses; any dictionary / denylist /
registry term whose enclosing identifier is not in the repo-wide protected set
is a failure. It also rejects stale/incomplete/non-injective semantic aliases,
unresolved real spellings that collide with an alias, projection parse
regressions, and files in the mirror that nothing tracks. Failures are printed
one per line and the process exits with code `3`.

## Known Limitations

- Legacy mirror commands and v2 `read-code` now consume one physical combined
  projection: lexical policy still covers prose, while reviewed semantic
  aliases are overlaid only on exact bound occurrences. V2 additionally
  returns projected AST/symbol metadata and supports structured edits.
- Repo-relative path components are projected, but behavior-bearing import,
  include, and module-path text inside source remains governed by the content
  sanitizer/public-API rules. The final filename extension is deliberately
  preserved. Internal maps, journals, and SQLite keep real path identities;
  they are state, not an agent-facing tree.
- Multi-file apply is journaled (fsync'd) before writes, serialized by `flock`, and recoverable via `recover`, but it is not a substitute for transactional filesystem commits.
- Patch back-projection is span-aware for known replacement spans and reverse-maps aliases in added lines, but hunk coordinate remapping is line-oriented and edits *inside* an alias still conflict; use `rename-symbol` to replace the real compiler identity and its old reviewed alias with the requested new name.
- Legacy `rename` is single-file scoped. V2 `rename-symbol` uses `rust-analyzer` or `clangd` and rejects edits outside the workspace.
- Protected-identifier detection (public API, imports) is conservative lexical heuristics, not a language-aware symbol graph; matching is ASCII-oriented (non-ASCII terms are not matched).
- Rust and C/C++/Objective-C family have Tree-sitter structure plus compiler/LSP references and rename when the server is installed. Objective-C++ merges C++ and Objective-C trees and reparses Objective-C method bodies through a byte-stable C++ projection, rather than choosing one incomplete grammar for the whole `.mm` file. JS/TS, Python, and Go have Tree-sitter AST edits but no semantic rename; unknown languages are read-only. No text-edit fallback is attempted.
- Term matching is deliberately aggressive (case- and underscore-insensitive substrings inside word runs), so a term embedded in an unrelated word is also replaced; keep the allowlist current.
- `.gitignore` support is delegated to the `ignore` crate (full gitignore language, `require_git(false)`); the walker does not follow parent-directory or global gitignores, for determinism.
- The opencode plugin, MCP server, and Codex/Claude hooks are working guardrail adapters, not hard boundaries; they do not intercept reads via `bash` or other non-file tools.
- Codex/Claude hooks require `python3` on the host.
- The model-based sanitizer is proposal-only: an external provider (a `command` confirmed with `--allow-provider-command`, or an OpenAI-compatible endpoint confirmed with `--allow-provider-endpoint`) must be supplied; there is no bundled LLM. The deterministic engine (dictionary + content/path alias registries + denylist) always does the actual sanitization.
- The `llm`/`openrouter`/`kou-router` proposal providers pre-redact known
  content terms, then post the remaining real file content in line-aligned
  source chunks and the projected path inventory in separate source-free
  batches; keep the endpoint local (kou-router/Ollama) unless you accept that
  exposure. Oversized files send no source body but their path metadata remains
  eligible. Embedding requests carry sanitized mirror content only.
- Semantic search is brute-force cosine over all stored vectors (no ANN index); fine for tens of thousands of chunks, not millions. Vectors go stale between `index` and the next `embed-index` run; search detects this and refuses results until `embed-index` converges them.
- Strict mode (`sh`/`strict-run`) is a guardrail, not a hard sandbox; FUSE/overlay isolation is not implemented. Output sanitization covers terms present in the span maps/dictionary/registry/denylist; novel real names in output are not hidden.

## Development

```bash
cargo test          # full suite, including the 5k-file incremental index
cargo fmt --check   # CI-enforced
cargo clippy --all-targets -- -D warnings
```

CI (GitHub Actions) runs fmt + clippy + tests on every push and pull request.

The test suite covers indexing (including incremental fingerprints and a
parallel apply/sync stress run), reversible filename/directory projection,
collision refusal, pending-edit migration, projected provider/review/MCP/strict
surfaces, sanitized read/search with result caps, path traversal rejection,
span map offsets, bidirectional patch roundtrips (plus a property test over
random files and patches), alias reverse-mapping in added lines, public API
consistency, conflicts inside replacements, rollback on simulated multi-file
apply failure, verify's leak backstop and exit codes, hook
generation/merging/uninstall, strict-mode streaming and sanitization, the review
pipeline, and CLI smoke flows.

## License

MIT — see [LICENSE](LICENSE).
