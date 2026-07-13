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

There is one materialized agent projection. The lexical privacy policy applies
first, including prose; accepted semantic aliases then overlay only identifier
occurrences bound to an accepted `symbol_id`. Legacy mirror readers and
semantic v2 `read_code` consume the same bytes. V2 adds projected AST metadata
and structured mutation, but never promotes comments or strings into symbol
references.

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
- provocative/toxic lexicon normalized to neutral wording;
- configured terms in agent-facing directory names and filename stems; the
  complete tracked path map must remain reversible.

Deliberately **not** hidden (behavior must remain legible):

- control flow, imports/exports, and behavior-bearing include/module-path text
  inside source;
- filename extensions, which remain stable for language and tool dispatch;
- public API names — the repo-wide protected identifier set (public
  declarations, import-position names, code dunders; collected from code
  positions only, never from prose/comments/strings) keeps one decision per
  symbol across the whole mirror;
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

### 5a. Workspace on a network filesystem
All writer serialization is a single advisory `flock(2)`. On NFS/SMB/CIFS,
`flock` may be host-local (or a no-op) depending on mount options and server
support, so two hosts editing the same workspace can both hold the "exclusive"
lock and silently corrupt the mirror/db.
- **Mitigation:** a best-effort per-process warning is logged when the lock
  file's filesystem looks networked (statfs); the db/mirror are derived state
  and rebuildable by `index`.
- **Residual risk:** the warning is advisory, not a gate. Keep the repo on a
  local filesystem; the real files themselves are not protected by a rebuild.

### 6. Crash mid-apply
The process dies after real files start changing.
- **Mitigation:** the full before/after intent (contents and permission bits)
  is journaled as `applying` and fsync'd (temp + rename + directory fsync)
  before any real write; `recover` replays (roll-forward) or `--rollback`
  undoes it, and sweeps temp files stranded by a kill mid-atomic-write. A file
  torn by power loss into non-UTF-8 bytes is a per-entry conflict resolvable
  with `--force`, never an abort of the whole recovery. All writers serialize
  on a blocking `flock` that the kernel releases when the process dies, so a
  crash never wedges the workspace.
- **Residual risk:** not a cross-crash transactional FS/DB; the sqlite state is
  derived and rebuilt by `index`.

### 7. Model proposal error
A model proposes an unsafe or wrong alias.
- **Mitigation:** the model never writes the mirror. Proposals are schema- and
  policy-validated (typed semantic/path target IDs, surface-specific allowlist,
  denylist-in-output, identifier validity, public-API guard, path-map
  reversibility, confidence threshold) and queued for human review. Approval
  re-validates and records either a symbol-scoped alias or a path-only alias;
  neither lets the model write. A non-local Rust or C-family identifier
  additionally requires a stable `rust-analyzer`/`clangd` reference result and
  persists exact compiler bindings across declarations and uses. Drift marks
  the whole alias group stale; `index` projects it again only after a complete
  fresh closure. Syntax ambiguity and incomplete compiler results fail closed.
  `index`/`verify` use only the deterministic engine.
  Executing a repo-supplied provider command requires an explicit
  `--allow-provider-command`, runs with concurrent pipe I/O, and is killed on
  timeout.
- **Residual risk:** a human can approve a bad alias; the audit (`review-sanitize`)
  makes every applied replacement inspectable, and the verify backstop rejects
  aliases that still contain a term.

### 7a. Repo-local config exfiltrates real content via the LLM provider
The `llm`/`openrouter`/`kou-router` proposal providers pre-redact terms already
covered by deterministic content policy, then POST the **remaining real file
content** and a separate projected path inventory to the endpoint named in
repo-local `config.toml`. Large files are sent in line-aligned chunks: the owned
analysis region and overlap context are separate payload fields, but both
contain real source. Path-only requests contain no source and batch unique
directory/filename-stem candidates from the selected scope. Files above
`propose_max_file_bytes` send no source body or semantic candidates, but their
paths remain in that inventory. A malicious
or tampered config could point that at an attacker's server.
- **Mitigation:** running any endpoint provider requires the explicit
  `--allow-provider-endpoint` confirmation naming the URL (for every kind,
  including loopback presets); API keys are read only from the environment,
  never from config; a remote endpoint with no key fails preflight before any
  content is sent. The embedding path (`embed-index`, `semantic-search`) sends
  **sanitized mirror content only** — the same text any agent already reads —
  so a redirected embeddings endpoint gains nothing beyond agent-visible text.
- **Residual risk:** a user who confirms without reading the URL sends the
  remaining real content wherever the config points; embedding chunk texts stored in
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
  before leaving the server, with the absolute workspace-root prefix scrubbed
  first (the root's own directory names are not dictionary terms); if the
  redactor itself cannot be built, a generic message is returned instead
  (fail closed).
- **Residual risk:** the redactor covers known terms only, like `sh` output
  sanitization (bypass 8).

### 8b. Success output leaks the host path through the MCP server
`apply_patch` success used to embed the absolute journal path — including the
workspace root's directory names (e.g. a company-named repo folder), which the
dictionary redactor cannot know about.
- **Mitigation:** journal references in MCP tool output are workspace-relative;
  the fallback is the bare file name, never an absolute prefix.
- **Residual risk:** none known for tool output; CLI stdout (host-side) keeps
  absolute paths by design.

### 8c. An alias collides with a natural word (ambiguous mirror)
If a configured alias also occurs naturally in the real repo, the mirror
becomes non-injective: the natural word is indistinguishable from the alias,
reads mislead, and an agent-typed word reverse-maps into the real term
(silent corruption of agent intent).
- **Mitigation:** lexical and semantic alias collisions are hard errors at index, verify, patch
  (conflict, exit 2), rename, and proposal-approval time; config load/save
  rejects non-injective or self-sanitizable alias sets and unmatchable
  multi-token terms; default dictionary aliases carry a per-workspace salted
  suffix so natural collisions are practically impossible.
- **Residual risk:** a legacy workspace keeps its human-chosen aliases until
  the first post-upgrade sync surfaces any collision as an actionable error.

### 8d. A real filename biases the model or two paths collapse after sanitization
A private or provocative filename can cause false semantic framing even when
the file content is benign. Independently, two different real paths could map
to the same sanitized spelling and make edits ambiguous.
- **Mitigation:** every directory component and filename stem in the physical
  mirror and all agent-facing path fields is deterministically projected with
  the configured term table. The proposal provider receives that projected
  `rel` plus independently typed `path_candidates`; a `file_path` result must
  copy a current `path_id` and survive local policy and collision validation.
  Approval writes only `sanitizer.path_alias_registry`, never source content or
  the real filesystem name. The provider cannot apply its own answer.
  Before any mirror write, index proves a bidirectional mapping for every
  tracked file and directory prefix and rejects file/directory or ASCII
  case-insensitive collisions. Reads, searches, strict worktrees, reviews,
  semantic results, and patch reports require membership in the current map;
  stale or planted physical files are not exposed.
- **Residual risk:** before the first mapping is approved, an unknown raw term
  remains visible in the projected path and in its path-proposal candidate. The
  extension and behavior-bearing include/module strings inside source are
  preserved; raw filenames also remain in the real repo and internal state
  identities. A policy change can leave a pending edit at its old physical
  mirror path until `sync --force` stashes and migrates it, but that stale path
  is withheld from agent-facing reads in the meantime.

### 9. Sanitization breaks the code
A replacement produces invalid code or renames a public symbol.
- **Mitigation:** the repo-wide protected identifier set (public declarations,
  import-position names, code dunders) is skipped everywhere — one symbol, one
  decision — and identifier aliases are validated as identifiers. Terms and
  their case variants map to one deterministic alias, so cross-file renames
  stay consistent. A denylisted term that would be protected this way is a hard
  error, so the two policies can never quietly contradict each other.
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
  (exit code 3, each failure printed). The protected set is collected only from
  **code contexts** — prose formats contribute nothing, and comments and string
  literals never protect — so an English sentence cannot grant a term repo-wide
  immunity. A denylisted term that a protected identifier would keep alive is a
  hard error at index and a verify finding, never a silent residue.
- **Residual risk:** residues of *dictionary* terms inside protected
  identifiers are sanctioned by policy (public names stay real); the backstop
  shares the matching primitive AND the protected-set collector with the
  sanitizer, so a bug in either would blind both. Import-position string
  specifiers stay real by design.

## Guarantees vs non-guarantees

**Guarantees**
- The deterministic combined lexical + accepted-semantic projection of real
  source equals the mirror after `index`/`sync` (checked by `verify`).
- The patch bridge preserves the invariant in both directions for edits outside
  replacement spans: `sanitize(apply_original(patch)) == apply_sanitized(patch)`
  and reverse-projecting the patched mirror reproduces the patched real file
  byte-for-byte — or it conflicts (exit code 2) and leaves the real file
  untouched. Aliases in newly added lines are reverse-mapped to their real
  originals; an ambiguous alias is a conflict.
- Semantic aliases are scoped to one compiler-linked symbol closure;
  same-spelling independent symbols can have independent decisions,
  comments/strings never become references, and all agent readers see the
  resulting unified mirror.
- No dictionary/denylist/registry term survives into the mirror outside a
  protected identifier — enforced independently by the `verify` leak backstop.
  Protected identifiers come only from code positions, so no amount of prose,
  comments, or string literals can create one; a **denylisted** term can never
  be a sanctioned residue at all (it is a hard error instead).
- The model never writes the mirror; only the deterministic engine does.
- V2 proposal output must reference either existing owned symbol/occurrence IDs
  or a current directory/filename-stem `path_id`; invented, ambiguous, stale,
  external, generated, dependency, API-boundary, and colliding path targets are
  rejected locally.
- Apply intent is journaled (fsync'd) before any real write, and every writer
  holds the workspace flock. A back-projected write preserves the real file's
  permission bits.
- Sync never overwrites a mirror file holding a pending, not-yet-projected
  agent edit — including when the workspace database row for that file is
  missing. Only `sync --force`, which stashes the edit first, may reset it.
- The sanitization policy is never regenerated silently: losing `config.toml`
  on an initialized workspace is a hard error, not a reset to defaults.

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
