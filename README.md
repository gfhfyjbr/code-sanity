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
- `recover [--rollback]`
- `sync`
- `verify`
- `doctor [--agent codex|claude|opencode]`
- `install-hooks --agent codex|claude|opencode`
- `serve [--once]`

`serve` and generated agent hooks are scaffolds. The working MVP surface is the CLI core.

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
- LLM provider, MCP server, generated hooks, and production daemon remain scaffolds for later phases.

## Tests

```bash
cargo test
```

The test suite covers indexing, sanitized read/search, path traversal rejection, empty grep/search rejection, span map offsets, patch roundtrip, alias-collision back-projection, public API consistency, conflicts inside replacements, rollback on simulated multi-file apply failure, sync after external edit, UTF-8 offsets, ignore rules, mixed language fixtures, and CLI smoke flows.
