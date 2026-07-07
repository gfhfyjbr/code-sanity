# code-sanity threat model

## What this is for

code-sanity is a **lexical normalization and privatization layer**. It reduces
leakage of private context (internal names, client/domain terms, provocative or
toxic identifiers, private comments and test fixtures) into an AI agent's view of
a repository, and it reduces false semantic framing where a name looks more
dangerous than the code's actual behavior.

It is **not** a mechanism to hide what code does. The sanitizer must not rewrite
control flow, imports, public API names, syscalls, protocol strings, SQL, shell
commands, auth/crypto semantics, or any other behavior-bearing text. If an
operation is destructive, networked, or security-sensitive, the agent should
still see that behavior even when identifiers are normalized. Hiding malicious
semantics from a reviewer or scanner is an explicit non-goal.

The real repository is always the source of truth. The mirror is a derived,
regenerable view.

## Assets

Protected (kept out of the agent-facing mirror where policy allows):

- private identifiers (local/private function, variable, type names);
- comments and doc comments;
- configured terms in **all** string literals (the string's structure and
  semantics are untouched; only the term's lexical spelling is replaced);
- private domain terms, client names, internal aliases — matched case- and
  underscore-insensitively, including inside larger identifiers (`AcmeClient`
  also catches `ACME_CLIENT` and `acmeClientFactory`);
- denylisted terms (replaced immediately with deterministic salted aliases,
  even before a human approves a nicer name);
- provocative/toxic lexicon normalized to neutral wording.

Deliberately **not** hidden (behavior must remain legible):

- control flow, imports/exports, module paths, filenames;
- public API names — the repo-wide protected identifier set (public
  declarations, import-position names) keeps one decision per symbol across
  the whole mirror;
- SQL statements, shell commands, env var names, feature flags (a configured
  term inside them is still lexically replaced, but structure and semantics
  never change);
- syscall/protocol/crypto/auth semantics.

## Trust boundaries and enforcement tiers

| Mode | Reads | Edits | Guarantee |
| --- | --- | --- | --- |
| `soft` | agent may read the real repo; adapters warn/redirect best-effort | allowed, nudged to the bridge | Convenience; assume the agent can see raw text. |
| `guided` (default) | reads steered to the mirror; direct real edits denied by hooks | via bridge/MCP | Good default; raw reads via non-file tools are not intercepted. |
| `strict` | raw real reads/edits denied by hooks; `strict-run` serves a sanitized worktree | via bridge/MCP only | Strongest available; still a guardrail, not a kernel-enforced sandbox. |

Hooks and adapters are **guardrails, not a hard enforcement boundary.** No hook
API for Codex, Claude Code, or opencode guarantees that every byte placed before
the model passes through the sanitizer. See [HOOKS_MATRIX.md](HOOKS_MATRIX.md).

## Known bypasses

Each is listed with the mitigation code-sanity provides and the residual risk.

### 1. Raw read via shell / IDE / LSP / compiler
The agent (or the harness) reads the real file with `cat`, `sed`, `rg`, an IDE
context loader, LSP diagnostics, or raw compiler output.
- **Mitigation:** guided/strict hooks redirect obvious shell reads (`cat`/`nl`/
  `head`/`tail`) to `code-sanity read`; MCP `read_file`/`search` serve sanitized
  content; `strict-run` runs in a sanitized worktree; `sh` reverse-maps command
  output.
- **Residual risk:** hooks do not intercept every shell path (Codex docs say so
  explicitly), IDE/LSP context loaders and file-upload paths are out of scope,
  and `sh` output sanitization is substring-based and best-effort. Hard isolation
  needs an overlay/FUSE/container.

### 2. Worktree escape / absolute paths (strict-run)
`strict-run` sets the cwd to a sanitized worktree, but a command can still
`cat /abs/path/to/real/repo/...` or `cd` elsewhere.
- **Mitigation:** the worktree is materialized outside the repo tree so the real
  root is not a parent of cwd.
- **Residual risk:** absolute paths, symlinks, and network access still reach the
  real repo. Only an OS sandbox closes this.

### 3. Raw filesystem MCP or a second tool server
A separate `mcp__filesystem__read_file` (or any tool that reads the real root)
bypasses code-sanity entirely.
- **Mitigation:** guided/strict hooks deny raw `Read`/`Edit`/`Write` on the real
  root and steer to the code-sanity server.
- **Residual risk:** a tool not covered by the hook matcher is not blocked;
  matchers must be maintained per agent.

### 4. Edits that touch a replacement span
An edit that changes the alias text itself cannot be back-projected
unambiguously.
- **Mitigation:** the bridge refuses such edits (conflict), leaves the real file
  untouched, and writes a journal entry; the sanctioned path is `rename`.
- **Residual risk:** none to correctness; it is a usability limitation.

### 5. Drift between real and mirror
The real file changes outside the bridge, so the mirror is stale.
- **Mitigation:** every apply checks real/mirror hashes against the db and
  conflicts on drift; `sync`/`index` re-derive the mirror; `verify` detects it.
- **Residual risk:** a read between an external edit and the next `sync` serves
  stale (but still sanitized) content.

### 6. Crash mid-apply
The process dies after real files start changing.
- **Mitigation:** the full before/after intent is journaled as `applying` and
  fsync'd (temp + rename + directory fsync) before any real write; `recover`
  replays (roll-forward) or `--rollback` undoes it, and sweeps temp files
  stranded by a kill mid-atomic-write. All writers serialize on a blocking
  `flock` that the kernel releases when the process dies, so a crash never
  wedges the workspace.
- **Residual risk:** not a cross-crash transactional FS/DB; the sqlite state is
  derived and rebuilt by `index`.

### 7. Model proposal error
A model proposes an unsafe or wrong alias.
- **Mitigation:** the model never writes the mirror. Proposals are schema- and
  policy-validated (allowlist, denylist-in-output, identifier validity, public-API
  guard, confidence threshold) and queued for human review; approval re-validates
  and records a deterministic alias. `index`/`verify` use only the deterministic
  engine. Executing a repo-supplied provider command requires an explicit
  `--allow-provider-command`, runs with concurrent pipe I/O, and is killed on
  timeout.
- **Residual risk:** a human can approve a bad alias; the audit (`review-sanitize`)
  makes every applied replacement inspectable, and the verify backstop rejects
  aliases that still contain a term.

### 7a. Repo-local config exfiltrates real content via the LLM provider
The `llm`/`openrouter`/`kou-router` proposal providers POST **real file
content** to the endpoint named in repo-local `config.toml` — a malicious or
tampered config could point that at an attacker's server.
- **Mitigation:** running any endpoint provider requires the explicit
  `--allow-provider-endpoint` confirmation naming the URL (for every kind,
  including loopback presets); API keys are read only from the environment,
  never from config; a remote endpoint with no key fails preflight before any
  content is sent. The embedding path (`embed-index`, `semantic-search`) sends
  **sanitized mirror content only** — the same text any agent already reads —
  so a redirected embeddings endpoint gains nothing beyond agent-visible text.
- **Residual risk:** a user who confirms without reading the URL sends real
  content wherever the config points; embedding chunk texts stored in
  `db.sqlite` (local only) can lag the mirror until the next `embed-index`
  run after a policy change.

### 8. Compiler/test output leaks real names
`cargo`/`rustc`/`pytest` print real identifiers and paths.
- **Mitigation:** `code-sanity sh -- <cmd>` streams stdout/stderr through a
  leftmost-longest Aho-Corasick rewrite built from the span maps, dictionary,
  registry, and denylist; `strict-run` additionally executes in a unique
  owner-only (0700) sanitized worktree. If the output sanitizer cannot be
  built, output is withheld (fail closed), not passed through raw.
- **Residual risk:** covers only known terms; novel real names in output are
  not hidden.

### 8a. Error text leaks real names through the MCP server
Tool successes serve sanitized mirror content, but tool **errors** interpolate
whatever the failure touched — real paths, io error text, hunk context.
- **Mitigation:** every MCP tool error passes through the workspace redactor
  before leaving the server; if the redactor itself cannot be built, a generic
  message is returned instead (fail closed).
- **Residual risk:** the redactor covers known terms only, like `sh` output
  sanitization (bypass 8).

### 9. Sanitization breaks the code
A replacement produces invalid code or renames a public symbol.
- **Mitigation:** the repo-wide protected identifier set (public declarations,
  import-position names) is skipped everywhere — one symbol, one decision — and
  identifier aliases are validated as identifiers. Terms and their case
  variants map to one deterministic alias, so cross-file renames stay
  consistent.
- **Residual risk:** regex/byte-scanner tokenization is not AST-aware; a
  sanitized worktree may still not compile in edge cases (use `sh` against the
  real repo for builds).

### 10. A term leaks into the mirror despite everything
A sanitizer bug, a bad replacement value, or a planted mirror file leaves a
term visible.
- **Mitigation:** `verify` runs an independent leak backstop: it rescans the
  mirror and every span-map replacement output with the same matching primitive
  and recomputes the protected set from the real files; any unsanctioned term
  occurrence, and any untracked file inside the mirror, fails verification
  (exit code 3, each failure printed).
- **Residual risk:** residues inside protected identifiers are sanctioned by
  policy (public names stay real); the backstop shares the matching primitive
  with the sanitizer, so a bug in that primitive itself would blind both.

## Guarantees vs non-guarantees

**Guarantees**
- `sanitize(real)` is deterministic and equals the mirror after `index`/`sync`
  (checked by `verify`).
- The patch bridge preserves the invariant in both directions for edits outside
  replacement spans: `sanitize(apply_original(patch)) == apply_sanitized(patch)`
  and reverse-projecting the patched mirror reproduces the patched real file
  byte-for-byte — or it conflicts (exit code 2) and leaves the real file
  untouched. Aliases in newly added lines are reverse-mapped to their real
  originals; an ambiguous alias is a conflict.
- One symbol, one decision: a protected name stays real everywhere; a term maps
  to one alias everywhere (including case/underscore variants).
- No dictionary/denylist/registry term survives into the mirror outside a
  protected identifier — enforced independently by the `verify` leak backstop.
- The model never writes the mirror; only the deterministic engine does.
- Apply intent is journaled (fsync'd) before any real write, and every writer
  holds the workspace flock.
- Sync never overwrites a mirror file holding a pending, not-yet-projected
  agent edit.

**Non-guarantees**
- That every byte the model sees passed through the sanitizer.
- That real behavior/semantics are hidden (explicitly not a goal).
- That strict mode is a kernel-enforced sandbox.

## Recommendations

- Use `guided` as the default; use `strict` + `strict-run`/`sh` when you must
  minimize raw-name exposure.
- For hard isolation, run the agent in an OS sandbox/container/overlay whose only
  visible tree is the mirror.
- Keep the allowlist/denylist current; review the audit (`review-sanitize`)
  periodically.
- Never rely on code-sanity to hide dangerous behavior from a human reviewer or a
  security scanner.
