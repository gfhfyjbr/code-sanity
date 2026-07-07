# Roadmap to production readiness

Status as of 2026-07-07 (end of day): **P0 #1–6 and all of P1 are done.**
v0.2.0 is released with binaries for linux/macos x86_64+aarch64; CI is green
on both platforms with a cargo-deny gate; the patch parser is fuzzed (first
run found and fixed a UTF-8 slice panic); ureq is on v3; the schema refuses
newer-versioned databases; embed-index has an A3-style storm suite; recover
sweeps crash-stranded temp files. The suite is 139 tests, clippy-clean.

What remains is P0 #7 — dogfooding (see the checklist below) — and the
on-demand P2 backlog.

Effort: **S** — up to half a day, **M** — 0.5–2 days, **L** — more than 2 days.

## P0 — no prod status without these (~1 week)

| # | Task | Why | Effort |
|---|------|-----|--------|
| 1 | Commit the current working tree (A0–A3 follow-ups, prod-readiness fixes, OpenRouter presets) as logical commits | ~1,400 uncommitted lines are one disk failure away from being lost | S |
| 2 | Create a GitHub remote, push, add `repository` to Cargo.toml | Without a remote there is no CI, no releases, no issues; also activates the `HTTP-Referer` attribution header for OpenRouter | S |
| 3 | Get CI green on both ubuntu **and** macos | The macOS job has never run; the platform guard in `src/lib.rs` claims macOS support — prove it | S–M |
| 4 | Release workflow: tag → binaries (linux x86_64/aarch64, macos x86_64/aarch64) → GitHub Release | The only install path today is `cargo run` from source | M |
| 5 | `CHANGELOG.md`, bump to 0.2.0, Installation section in README | Users must know what they are installing and what changed | S |
| 6 | `cargo-deny` in CI (advisories, licenses, bans) + Dependabot | The tool sells privacy; a vulnerable dependency attacks the product's core value | S |
| 7 | Dogfooding: one week on 2–3 real repositories against live OpenRouter and kou-router | The LLM integrations have never met real traffic: malformed model replies, nonstandard error codes, rate limits | L (calendar, not effort) |

## P1 — towards a confident 1.0 (~1 week)

| # | Task | Why | Effort |
|---|------|-----|--------|
| 8 | `rust-toolchain.toml` (pin stable) | Edition 2024 needs Rust ≥ 1.85; reproducible builds for contributors | S |
| 9 | Upgrade `ureq` 2 → 3 | One major behind; v3 reworked timeouts and the error API — migrate while the HTTP layer is small | S–M |
| 10 | Fuzz the patch parser (`cargo-fuzz`: unified diff, CRLF, counted hunks) | `patch.rs` is 2,300 lines of the most complex parsing and consumes input from LLM agents; proptest exists, fuzzing digs deeper | M |
| 11 | Stress test: `embed-index` against a slow mock concurrently with `sync`/`apply` | The embed lock scheme is unit-tested; it needs an integration storm like the A3 suite | S–M |
| 12 | DB schema migration test (open a database with old `user_version`) | The mechanism exists (`SCHEMA_VERSION = 2`, `src/db.rs`), but the upgrade path is untested | S |
| 13 | Timings in command reports (`indexed=... elapsed=1.2s`) | Cheap observability: otherwise users' only diagnostic is `-vv` | S |
| 14 | `SECURITY.md` (how to report) + re-read THREAT_MODEL.md against current code | The Redactor boundary changed; doc claims must match the code | S |

## P2 — backlog, on demand (do not build ahead of need)

| # | Task | Trigger | Effort |
|---|------|---------|--------|
| 15 | Parallel scan + i8 quantization in `semantic_search` | Search latency complaints at 50k+ chunks | M |
| 16 | `sqlite-vec` (same db.sqlite, same transactions) | Repositories with hundreds of thousands of chunks; **not** an external vector DB — see the decision note below | M–L |
| 17 | Cache query embeddings | A noticeable share of repeated `semantic-search` queries | S |
| 18 | Homebrew tap / install.sh | A stream of external users appears | M |
| 19 | Criterion benchmarks for index / patch bridge | Performance regressions while refactoring `patch.rs` | M |
| 20 | Publish to crates.io | Decision to distribute via `cargo install code-sanity` | S |

## Decision note: vector search stays in SQLite

External vector databases (LanceDB, SatoriDB, server-based engines) were
evaluated and rejected: they solve "corpus larger than RAM / high QPS", which
this tool does not have. The per-repo corpus is thousands to tens of thousands
of vectors, the dominant `semantic-search` latency is the HTTP embedding call
for the query, and a second store next to `db.sqlite` would break the
single-transaction atomicity and workspace-lock discipline the crash-safety
work established. The upgrade ladder is: brute-force scan (current) →
parallel + quantized scan (#15) → `sqlite-vec` (#16). Each step keeps one
store, one lock, both platforms.

The priority logic: P0 turns "production-quality code" into "a product you can
install and trust"; P1 closes the reliability tails the first external users
would hit; P2 items answer problems that do not exist yet — building them
early is waste.

## Dogfooding checklist (P0 #7, one week, started 2026-07-07)

Setup per repository (2–3 real repos, at least one against live OpenRouter and
one against a local kou-router):

```bash
cargo install --git https://github.com/gfhfyjbr/code-sanity --locked  # or a release binary
cd <repo> && code-sanity init && code-sanity index
# config.toml: [sanitizer.provider] kind = "openrouter" | "kou-router"
# [embeddings] enabled = true; export the API key env var
```

Record every occurrence of:

- [ ] malformed model replies (`propose-sanitize`): unfenced/prose JSON the
      fence-stripper missed, schema violations, empty batches;
- [ ] nonstandard HTTP error codes / bodies from either gateway (anything the
      429/502/503/504 retry set misses — e.g. 520s, HTML error pages);
- [ ] rate-limit behavior: does 3×exponential backoff survive real OpenRouter
      limits during `embed-index` on a large repo?
- [ ] `embed-index` wall time and `elapsed=` numbers on the biggest repo
      (P2 #15 trigger data);
- [ ] any `verify` failure, `recover` invocation, or stale-vector report in
      normal use — each one is a bug report;
- [ ] agent-adapter friction (hooks, MCP) worth an issue.

Exit criterion: one week of daily use with zero unexplained failures; every
recorded item either fixed or filed as an issue.
