# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Human `review` output once again shows every proposal's warning flag,
  rationale, category, confidence, and file without truncating the actionable
  proposal ID. Flagged TUI rows now carry a visible `!`, and the detail panel
  labels validation warnings and provider reasons explicitly.

## [0.4.0] - 2026-07-11

### Added

- Running `code-sanity` without a subcommand now opens a full-screen,
  mouse-enabled workspace with review, source-context, activity, and workspace
  views.
- The interactive command palette supports completion, history, filtering,
  review decisions, scoped proposal scans, and background index/verify jobs.
- Human CLI output attached to a terminal now uses live spinners, color,
  summaries, and width-aware result tables while redirected and JSON output
  keep their existing machine contracts.

### Changed

- Proposal scans and review decisions in the TUI require explicit confirmation;
  the provider dialog identifies the configured endpoint and makes clear that
  real source may leave the process.
- Long TUI operations run on background workers and expose live progress and an
  event log without blocking keyboard or mouse input.

## [0.3.2] - 2026-07-11

### Added

- The file index now records external framework/package/SDK evidence separately
  from repository-owned protected names. LLM proposal requests receive the
  relevant scoped evidence, and local validation rejects external API/vendor
  fragments even when a model labels them private.
- `propose-sanitize --path` accepts indexed directories as well as individual
  files, so scoped runs no longer require a shell loop.

### Changed

- Proposal prompts now distinguish non-public repository-owned names from
  public third-party brands, products, integrations, OS components, protocols,
  and library APIs, which are explicitly out of scope.
- Pending originals and findings are deduplicated repo-wide, including
  normalized containing variants, because one approved alias applies globally.
- Proposal aliases are checked against every indexed real-file word before an
  item reaches review; approval retains the same repo-wide check as a backstop.
- External API evidence is scoped to the selected path, preventing generated or
  vendored trees outside a targeted source directory from suppressing findings.
- SQLite schema v3 stores indexed external API evidence; derived lexical and
  embedding state is rebuilt while patch journal history is preserved.

## [0.3.1] - 2026-07-11

Follow-up hardening from the v0.3.0 production-readiness audit. The headline
is a **soundness fix in the no-leak guarantee**: prose in a README could grant
a denylisted term repo-wide immunity, and `verify` blessed the leak because it
recomputes the same protected set. Also: file modes survive the patch bridge,
`recover` survives a torn file, `sync` no longer destroys un-projected agent
edits, and a lost `config.toml` no longer silently voids the policy.

### Added

- `install.sh` now detects the host target and downloads the matching checksummed
  binary from the latest or a pinned GitHub Release, requiring neither `sudo`
  nor Rust. It verifies archive layout, checksum, release/binary version parity,
  and the installed binary; source-build, custom destination, idempotent `PATH`,
  and uninstall modes remain available.
- CI now validates the installer contract, and tagged CD validates version/tag
  parity and main-branch ancestry before building all four supported targets.
  Native binaries are smoke-tested before packaging, then the published release
  is installed through `install.sh` on fresh Linux and macOS runners.
- `propose-sanitize` now runs provider calls with bounded concurrency (four
  workers by default, configurable with `sanitizer.propose_concurrency` or
  `--jobs`) and reports live per-file progress, latency, queue counts, skips,
  and errors on stderr. `--no-progress` disables the renderer, and `--json`
  stdout remains a single machine-readable document.
- Known dictionary/registry terms are now redacted before an LLM proposal
  request. The task contract also travels in the structured user message, so
  OpenAI-compatible gateways that replace custom system prompts still return
  the required proposal JSON instead of prose or a refusal.
- Repeated proposal scans no longer create duplicate pending review items for
  the same file and normalized original term; progress and final reports expose
  the number suppressed as `duplicates`.
- Proposal JSON is normalized through an untyped JSON value before schema
  decoding, tolerating duplicate object keys produced by some compatible
  gateways while retaining the full local content/policy validation step.
- LLM proposal discovery now explicitly covers security- and abuse-adjacent
  vocabulary used in benign code, in addition to private naming. The prompt
  carries the sanitizer's exact single-word-run constraints so model output is
  directly compatible with the deterministic matcher.
- The proposal task now requires byte-for-byte, case-sensitive source evidence
  and a final membership preflight, explicitly forbidding invented concepts,
  spelling/case changes, and words synthesized across source punctuation.
- Large LLM proposal inputs are now split into configurable line-aligned,
  overlapping chunks (`propose_chunk_bytes`, `propose_chunk_overlap_lines`).
  Chunks run in bounded parallel waves, report request-level progress, and pass
  validated findings plus existing pending-review originals to the next wave so
  the model does not spend output tokens proposing them again. Overlap is sent
  separately as read-only context; only the non-overlap region is eligible for
  proposals, with local ownership and deduplication enforced before review.
  External-command and heuristic provider contracts are unchanged.

### Breaking

- **Protected identifiers are collected only from code contexts.** The
  declaration/import heuristics ran over raw content, so `Data from shadowfax
  is loaded` in a README, `// migrated from acme_v1` in a comment, and
  markdown bold `__shadowfax__` each added the term to the **repo-wide**
  protected set — it then survived verbatim in every mirror file, with
  `verify` exiting 0. Collection now classifies the language and ignores
  prose formats (`.md`, `.txt`/`.rst`/`.adoc`, `.json`, `.toml`) entirely;
  within code, runs inside comments or string literals never protect, and the
  token-rule lookback no longer crosses a newline. Import-position string
  specifiers (`import x from "acme_sdk"`, Go `import "acme/pkg"`) stay
  protected. Unknown extensions remain code, so `.java`/`.rb` imports and
  `export FOO` in shell scripts keep their protection.
- **Dunder-shaped runs are no longer blanket-sanctioned.** `__init__` in
  python code is still protected (it now reaches the set through collection),
  but `__term__` markdown bold sanitizes to `__sym_…__`.
- **A denylisted term protected as a public name is a hard error.** `pub fn
  shadowfax_client()` with `shadowfax` denylisted previously kept the term in
  the mirror silently. `index` now refuses and `verify` reports it (exit 3),
  naming the declaring file and the three ways out. Dictionary terms in public
  names remain sanctioned residues (the default dictionary must not brick
  ordinary repos).
- **A missing `config.toml` on an initialized workspace is a hard error.**
  It used to be silently replaced with defaults — new salt, empty denylist and
  alias registry — and the whole mirror re-rendered without the user's policy,
  exit 0. The message points at the `config.toml.bak` sibling `Config::save`
  always keeps. `Config::write_if_missing` is removed.
- **The db-corruption remedy is now `sync --force`, not `index`** (see the
  pending-edit fix below). The schema-migration message still says `index`.
- `.sh`/`.yml`/`.txt` gain comment and string zones, so aliases inside them in
  **newly added** mirror lines are no longer reverse-mapped into real terms —
  consistent with how `.rs`/`.py`/`.go` have always behaved.
- `SANITIZER_BEHAVIOR_VERSION` 3 → 4: the first `sync`/`index` after upgrading
  re-renders every file and sweeps out any legacy prose leak.
- **Hyphenated sanitizer terms are rejected at validation.** A term like
  `acme-corp` normalized to a clean-looking needle but spans two word runs,
  so it never matched anywhere — the mirror, the MCP redactor, and `verify`'s
  leak backstop all silently missed it while `sh` output redaction caught it.
  Terms whose raw form is not exactly one `[A-Za-z0-9_]+` word run now fail
  load/save/verify with the existing split-it fix-it message
  (underscore-joined terms are unaffected).
- **Writers refuse to run on a detected network filesystem** (NFS/SMB/CIFS/
  AFP/WebDAV), where flock may be host-local and two hosts could silently
  corrupt the workspace; previously this only logged a warning. Readers still
  only warn. Opt back in with `durability.allow_network_fs = true`. Unknown
  filesystems are treated as local — detection is an allowlist, never a guess.
- `serve` now refuses `--json` with exit 64 like `sh`/`strict-run` (its
  stdout is the MCP protocol stream / `--once` manifest), matching the
  documented contract.

### Fixed

- Workspaces initialized by v0.2 transparently upgrade the exact legacy default
  dictionary to salted aliases in memory. This prevents common words such as
  `client` from blocking the first v0.3 index, while any user-customized
  dictionary remains untouched and the config file is never silently rewritten.
- **File permission bits survive the patch bridge.** Every real-file write went
  through a fresh 0644 temp file renamed over the target, so back-projecting an
  agent edit onto `deploy.sh` silently stripped its executable bit. The atomic
  write now carries the existing target's mode across the rename, and the
  journal records `before_mode`/`after_mode` so a rollback or `recover` that
  re-creates a deleted file restores the right mode too.
- **`recover` survives a torn, non-UTF-8 pending file.** The freshness check
  read pending files as UTF-8 and aborted the entire run on `InvalidData` —
  before `--force` was even consulted — leaving every entry `applying` and the
  workspace blocked. That is exactly the power-loss case the `F_FULLFSYNC`
  barrier exists to make recoverable. Freshness now compares bytes, and an
  unreadable-but-present file is a per-entry conflict.
- **A plain `sync` no longer destroys un-projected agent mirror edits when the
  db row is missing.** The pending-edit guard required an existing row, so
  after `rm db.sqlite` (the documented corruption remedy) or a crash before the
  first commit, the edit was silently overwritten with `stashed=0`. A missing
  row now counts as pending; only `sync --force`, which stashes to
  `journal/discarded/`, may reset it.
- **Real-file writes no longer follow symlinked directories out of the repo.**
  A pre-planted `src -> /outside` symlink let a create patch (or a tampered
  journal replay) write outside the root despite lexical path validation;
  apply, rollback, and recover now resolve the target's existing ancestors
  and refuse anything that escapes the canonical root.
- **macOS power-loss durability:** the journal `applying` entry (and its
  in-flight marker) are now written through `F_FULLFSYNC` — plain `fsync(2)`
  does not flush the drive cache on macOS, so a power loss could reorder a
  real-file write ahead of its recovery record. One full flush per apply;
  `durability.full_fsync = true` extends it to every durable-tier write.
- A configured `timeout_secs = 0` for the external proposal provider became a
  zero-duration deadline that killed every child; it is now floored to 1s
  (like the LLM client) and flagged by config validation, along with
  `embeddings.timeout_secs = 0`.
- Post-clap errors that fired before dispatch (e.g. a bad explicit `--root`)
  escaped the `--json` envelope; they now emit the standard error envelope.

### Added

- Retention for the remaining unbounded history: `journal/discarded/` stashes
  and RESOLVED review-queue items are pruned to `journal.max_entries` (same
  knob as journal entries; pending items are never touched).
- `[durability]` config section: `full_fsync`, `allow_network_fs`.
- Seed corpus and stable-toolchain replay for the `fuzz_apply_patch` target
  (hand-writable `content %%% patch` byte format); apply-side fuzz findings
  now become permanent regression tests like parse-side ones.

## [0.3.0] - 2026-07-09

Production-hardening release: every finding from the v0.2.0 readiness audit
closed — the alias model, path safety, lock/schema discipline, journal
scalability, LLM-loop robustness, MCP output privacy, and the CLI/CI
contracts.

### Breaking

- **Sanitizer config is validated at load and save.** Terms containing
  anything outside `[A-Za-z0-9_-]` (emails, hostnames, `com.acme.Foo`) can
  never match the word-run engine and were silently inert — they are now
  rejected with a fix-it message (split into per-word entries). Non-injective
  alias sets (two terms → one alias) and aliases containing a sanitizable
  term are rejected too. `verify` lists the same violations as findings
  instead of dying.
- **Alias collisions are hard errors.** An alias occurring naturally in the
  real repo made the mirror ambiguous and silently reverse-mapped agent-typed
  words into real terms (`let client = 5;` became `let acme = 5;`). Collisions
  now fail index/verify/rename/approval with the offending word, file, and
  offset; patches that would introduce an alias word conflict (exit 2). New
  workspaces get collision-proof default aliases (`neutral_3fd1`-style,
  per-workspace salted suffix). Existing configs are untouched, but the
  sanitizer behavior-version bump forces a one-time full re-render that
  surfaces any legacy collision as an actionable error.
- **Usage errors exit 64** (EX_USAGE), not clap's default 2 — `2` is
  exclusively the patch-conflict contract.
- **Glob semantics are real now** (globset): a pattern without `/` matches
  file names at any depth (`*.rs`, as always documented); with `/` it matches
  the repo-relative path — `src/*.rs` used to silently match nothing. The
  substring fallback is gone; invalid patterns are errors.
- **Read commands require an initialized workspace** and no longer create
  `.code-sanity/` in arbitrary directories; on an outdated DB schema they say
  "run `code-sanity index`" instead of migrating under a shared lock.
- `ApplyReport.journal_path` is now `Option<PathBuf>` (`None` for dry runs);
  `list_journal_entries` returns a `JournalListing` with corrupt entries
  surfaced (library API).

### Added

- **`--json`: a machine-readable output contract.** Every command except
  `sh`/`strict-run`/`serve` prints exactly one compact JSON envelope on
  stdout (`{ok, command, data, elapsed_ms?}` / `{ok, command, error: {kind,
  message, …}}`) with `error.kind` conflict/verify_failed/error mapping onto
  the unchanged exit codes. stderr stays human diagnostics; consumers must
  ignore unknown fields. `sh`/`strict-run` refuse the flag (exit 64) — their
  stdout and exit code belong to the wrapped command.
- **MCP session robustness**: a non-UTF-8 or oversized (>16 MiB) request line
  is answered with `-32700` and the session continues instead of the server
  exiting; non-object messages and missing `method` get `-32600`;
  `initialize` negotiates the protocol version against the supported list
  instead of echoing arbitrary client strings. The stdio server now logs
  file-only (hosts capture server stderr, and warn lines could carry real
  terms).
- **Journal retention**: `journal.max_entries` (default 500, `0` =
  unlimited) prunes the oldest terminal journal entries and `patch_journal`
  rows after each successful apply. In-flight entries and unparseable files
  are never pruned.
- Config numeric validation: `confidence_threshold` outside `[0, 1]`,
  zero `propose_max_file_bytes`/`ignore.max_file_bytes`, and degenerate
  embedding chunk parameters are policy violations with fix-it messages.
- A corrupt `db.sqlite` now names the remedy (delete the derived database,
  re-run `index`) instead of a raw "database disk image is malformed".
- Network-filesystem warning: the workspace flock is advisory-only across
  hosts on NFS/SMB/CIFS; a best-effort statfs check logs a warning when the
  lock sits on one (see the new THREAT_MODEL entry).
- `json_mode` (opt-in, per LLM provider config): sends
  `response_format: {"type": "json_object"}` with proposal chat requests;
  fence-stripping remains the fallback.
- The generated agent hooks rotate `.code-sanity/logs/hooks.log` at 5 MiB
  (one `.old` generation), mirroring the Rust logger.
- `apply-patch --dry-run` (CLI) and `dry_run` (MCP `apply_patch`): the full
  parse/translate/conflict pipeline with zero writes; conflicts still exit 2.
- `sanitizer.propose_max_file_bytes` (default 192 KiB): files larger than the
  cap are skipped with a note instead of overflowing the model context; one
  file's provider error no longer aborts a multi-file `propose-sanitize` run.
- Semantic-search fingerprint gate: changing the embeddings model/chunking
  without re-running `embed-index` is now a clear error before any HTTP call
  (it used to silently score the query against a different vector space).
- Index reports `errors=`/`symlinks=`: unreadable files (e.g. invalid UTF-8
  past the binary probe) are skipped per-file with their mirror preserved,
  instead of aborting the whole pass; symlinks are counted, never followed.
- Release workflow gates binaries on fmt/clippy/tests (both platforms) and
  cargo-deny; CI checks the declared MSRV (1.85) and runs tests `--locked`.
- Log rotation for `.code-sanity/logs/code-sanity.log` at 5 MiB (one `.old`
  generation).

### Fixed

- **Path traversal in `sync --path`**: `../…` paths (routinely produced by
  the editor hooks) read files outside the repo, wrote sanitized copies
  outside the mirror, and poisoned the stale sweep into deleting outside
  `.code-sanity/`. Out-of-repo paths are now a clean no-op skip (hooks) or a
  hard error (`--force`); the sweep validates stored paths before touching
  the filesystem; `recover` refuses journal entries with escaping paths;
  `propose-sanitize --path` validates too.
- **MCP success output leaked the absolute workspace path** (journal
  reference): now workspace-relative, and tool errors are root-scrubbed
  before term redaction.
- **Schema migration escaped the lock discipline**: init side effects (salt,
  `.gitignore`, migration) now run under one exclusive lock; the
  drop-and-recreate migration is transactional; concurrent first-init races
  are gone.
- **Journal**: the interrupted-apply check is O(1) via in-flight markers
  instead of parsing all history on every command; corrupt `applying`
  entries BLOCK with instructions instead of being silently quarantined
  (which unblocked torn workspaces); rolled-back entries drop their full
  file snapshots; durable writes fsync newly created parent directories.
- **Incremental index**: an unknown mtime (mtime-less filesystems) never
  rides the fast path (same-size edits were invisible forever); a source
  FILE named `build`/`dist`/`target` is indexed again (the name skip now
  applies to directories only).
- **Patch bridge**: appending after a final line without a trailing newline
  no longer merges lines; corrupt span maps yield conflicts instead of slice
  panics; header paths split on TAB (spaces are refused, not silently
  truncated to the wrong file); CRLF/LF tie prefers LF.
- `is_loopback` parses the host as an IP: DNS names like `127.evil.com` no
  longer count as keyless-eligible loopback.
- Truncated LLM replies (`finish_reason="length"`) are a clear error with
  remediation instead of a JSON-parse failure; retries honor `Retry-After`
  (capped) with jitter.
- The per-workspace salt comes from `/dev/urandom` (128-bit); the previous
  time+PID hash was guessable, contradicting its own documentation.
- Explicit `--root` pointing at a nonexistent path errors immediately;
  half of `--help` no longer renders blank.

## [0.2.0] - 2026-07-07

First installable release: prebuilt binaries for Linux and macOS (x86_64 and
aarch64) attached to the GitHub Release.

### Added

- LLM proposal providers: `provider.kind = "llm"` for any OpenAI-compatible
  chat endpoint, plus `openrouter` and `kou-router` presets. The endpoint
  receives real file content, so it is gated behind an explicit
  `--allow-provider-endpoint` confirmation; remote endpoints fail fast when the
  API key env var is missing.
- Semantic index over the sanitized mirror: `embed-index` (incremental,
  hash-keyed, batch embeddings) and `semantic-search`, backed by the existing
  `db.sqlite` under the same workspace-lock discipline. Exposed to agents as
  the `semantic_search` MCP tool. Disabled by default (`[embeddings]` config).
- Retrying HTTP client with connection reuse, exponential backoff on
  429/502/503/504 and transport errors, response-size cap, and OpenRouter
  attribution headers.
- `--version` flag.
- Supply-chain gates: `cargo-deny` (advisories, licenses, bans, sources) in CI
  and Dependabot updates for Cargo and GitHub Actions.
- CI runs on macOS as well as Linux; release workflow builds binaries for
  `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
  `x86_64-apple-darwin`, and `aarch64-apple-darwin`.

### Changed

- Strict-mode output sanitization fails closed: if the output sanitizer cannot
  be built, command output is withheld instead of passed through unredacted.
- MCP tool errors are redacted through the workspace redactor before leaving
  the server (fail-closed with a generic message when the redactor is
  unavailable).
- Review queue writes are atomic; a crash mid-write can no longer corrupt the
  proposal queue.
- Patched `crossbeam-epoch` past RUSTSEC-2026-0204.

## [0.1.0] - 2026-07-06

Source-only baseline; never released as a binary.

### Added

- Sanitized mirror with deterministic span-mapped sanitization (dictionary +
  alias registry + denylist) and incremental indexing.
- Patch bridge: unified diffs against the mirror are projected back onto the
  real repository (create/delete/rename, CRLF, counted hunks, per-line conflict
  granularity), with crash-safe journaled applies and `recover`.
- Workspace locking (`flock`) covering every DB/mirror writer; shared locks for
  readers; TOCTOU-safe reads.
- MCP server over stdio (`read_file`, `search`, `list_files`, `apply_patch`,
  `verify`) and agent adapters for opencode, Codex, and Claude Code.
- Strict mode (`sh` / `strict-run`) with sanitized command output.
- Proposal review queue with human approval (`propose-sanitize`, `review`).

[0.4.0]: https://github.com/gfhfyjbr/code-sanity/releases/tag/v0.4.0
[0.3.2]: https://github.com/gfhfyjbr/code-sanity/releases/tag/v0.3.2
[0.3.1]: https://github.com/gfhfyjbr/code-sanity/releases/tag/v0.3.1
[0.2.0]: https://github.com/gfhfyjbr/code-sanity/releases/tag/v0.2.0
[0.1.0]: https://github.com/gfhfyjbr/code-sanity/commit/8d1a159
