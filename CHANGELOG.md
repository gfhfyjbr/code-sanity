# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.2.0]: https://github.com/gfhfyjbr/code-sanity/releases/tag/v0.2.0
[0.1.0]: https://github.com/gfhfyjbr/code-sanity/commit/8d1a159
