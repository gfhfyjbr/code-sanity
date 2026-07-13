# AST/semantic v2 architecture

The v2 path keeps the real repository as the source of truth but no longer
uses a global text substitution as the agent edit contract:

```text
real source -> Tree-sitter index -> symbol-scoped projection -> MCP intent
            -> compiler/LSP WorkspaceEdit -> revision CAS -> durable journal
            -> real source + unified mirror + semantic reindex
```

Real repo-relative paths remain internal document identities. `find_code`,
`read_code`, references, previews, and commit reports return the same projected
directory/filename spelling as the lexical mirror, and `read_code` accepts that
spelling. Stable node/symbol/occurrence IDs continue to bind to the internal
real document identity, so changing an alias cannot redirect a transaction.

## Identity and persistence

`code-sanity index` refreshes the shared physical mirror and the v2 SQLite tables in the
same exclusive workspace-lock window. The v2 schema stores workspace
revisions, documents, every named AST node, declarations, occurrences,
symbol-scoped aliases, model proposals, and previewed/committed transactions.

- `symbol_id` is derived from file, language, declaration kind, qualified
  name, and either a normalized callable/type identity or same-name ordinal.
  Whitespace and overload reordering do not swap identities.
- `node_id` is derived from file and the named syntax path. It is stable across
  whitespace changes; structural edits may intentionally produce a new ID.
- the local resolver understands lexical scopes and shadowing, declarator
  shape (including pointers, function pointers and structured bindings),
  namespace qualifiers and aliases, class receivers, chained/smart-pointer member types,
  constructors, prototype/definition grouping, default/variadic arity,
  literal overload ranking, templates, declaration/definition parameter-name,
  default-value and comment differences, and Objective-C selector fragments;
- `external` occurrences have no visible owned candidate and do not poison an
  unrelated same-spelling symbol. Genuine ambiguity remains `unresolved` and
  fails closed;
- a named JavaScript/TypeScript, Python, or Rust function nested inside another
  callable is closed over that enclosing callable. Its own function body is
  never mistaken for proof of locality, so top-level functions, exported
  bindings, and class/member APIs still require a semantic provider;
- each document stores a resolver version. An ordinary index run re-resolves
  unchanged bytes after an upgrade while preserving old IDs, aliases, and
  compatibility anchors for pending review targets. Qualified names and local
  binding decisions always come from the new resolver; only separately
  persisted compiler bindings are reapplied.
- generated, vendor, dependency, and `_deps` paths are never mutable targets.

The semantic revision changes whenever documents or projected aliases change.
Every mutation requires `expected_revision`; preview and commit both compare
it. A mismatch is a conflict, never a fuzzy rebase.

## Projection

`read_code` returns the same content bytes as the physical mirror. The lexical
policy first sanitizes configured prose and code terms, then accepted semantic
aliases overlay only declaration/reference ranges bound to one `symbol_id`.
Comments, strings, imports, external APIs, and unrelated same-spelling tokens
do not receive the semantic overlay. Returned paths, symbol names, qualified
names, node/symbol/occurrence ranges, and byte offsets all use projected
coordinates; real spellings and coordinates do not escape through metadata.

Existing v1 identifier replacements are imported only when a span exactly
matches a bound occurrence and no same-name unresolved occurrences remain.
Overloads/cross-file references therefore fail closed instead of producing a
partly renamed projection. Lexical replacements in prose remain part of the
shared base projection but never become symbol bindings. New reviewed model
proposals write `proposal-v2` symbol aliases rather than global lexical policy;
the index still materializes them into the same physical mirror.

## Language backends

All languages use the `LanguageBackend` capability contract. Missing
capabilities fail closed.

| Language | Structure | References/rename | Mutation |
| --- | --- | --- | --- |
| Rust | Tree-sitter Rust | `rust-analyzer` LSP | AST edits + LSP rename |
| C/C++ | Tree-sitter C++ | `clangd` LSP | AST edits + LSP rename |
| Objective-C | Tree-sitter Objective-C | `clangd` LSP | AST edits + LSP rename |
| Objective-C++ | merged C++ + Objective-C trees and byte-stable C++ method-body projection | `clangd` LSP + persisted compiler bindings | AST edits + LSP rename |
| JS/TS, Python, Go | Tree-sitter grammar | unavailable | AST edits; semantic rename disabled |
| unknown | none | unavailable | read-only |

`rust-analyzer` or `clangd` must answer `--version` before the corresponding
semantic capability is advertised. `clangd` discovers compilation databases
in common nested build directories, runs a background index, and waits for a
stable result after indexing becomes quiescent. LSP work happens without the
workspace lock; the result is admitted only after a second revision/hash
check.

For every non-lexically-closed symbol, the language server is authoritative:
`rust-analyzer` for Rust and `clangd` for C/C++/Objective-C-family code. Exact
declaration and use locations are persisted as a compiler overlay; linked
declaration symbols receive the same alias and previously unresolved
references bind to the canonical symbol. A participating document hash change
marks the entire group stale. `index` re-runs the reference closure and restores
the already-reviewed decision only after the new result is complete. No stale
or partial compiler result is projected.

The syntax index is not used as a numeric minimum for compiler references: it
can conservatively over-bind equal member spellings before receiver types are
known. A stable, quiescent compiler result may detach one such historical
same-file binding only when a fresh resolver pass independently assigns it to
another owned declaration (or proves the receiver external), another owned
same-name declaration exists, and the occurrence is not inside a preprocessor
branch. Every other omission still rejects the closure. Conversely, a
`static` function declared in a C/C++/Objective-C implementation file has
translation-unit-local linkage, so its exact indexed occurrences form a safe
syntax closure even if the active compilation database disables their
`#ifdef` branch. Headers are deliberately excluded because their static
entities are instantiated separately by each including translation unit.

## Proposals

The LLM receives two independent target sets. `semantic_candidates` contains
owned symbols with existing `symbol_id`/declaration `occurrence_id`, kind,
qualified name, reference count, occurrence/call lines, signature, enclosing
code, origin/API-boundary evidence, `references_complete`,
`compiler_resolvable`, and accepted alias. A candidate is eligible when syntax
is locally complete or an indexed compiler provider can close it during
approval; the latter never bypasses the fail-closed approval check. `path_candidates`
contains stable `path_id` values for each current projected directory component
and filename stem. Typed output must use `category: "identifier"` and copy the
semantic IDs, or use `category: "file_path"` and copy one `path_id`.

The engine rejects invented or stale IDs, partial source identifiers,
ambiguous same-name symbols, external origins, existing mappings, invalid
aliases, model text absent from the corresponding source/path candidate, and
path aliases that collapse tracked files or directories. Extensions are not
path candidates. Path metadata is deduplicated across the selected scope and
sent in bounded path-only batches, independently of source chunks. An
oversized source sends no source content or semantic targets, but its path
still participates in that inventory.

Chunk decisions are carried only by exact `symbol_id`; equal spellings in
different symbols are independent, and invalid alternatives do not suppress a
later valid alias. Local unresolved evidence is bounded to the enclosing
function instead of poisoning same-spelling symbols elsewhere in the file,
while non-local Rust and C-family approval upgrades it to compiler-wide
completeness and ambiguity stays fail-closed. Header/implementation anchors
are one alias owner; implementation-local static linkage has its own exact
syntax closure, and `index` reconverges split aliases written by older versions
onto the header contract. Path decisions are deduplicated separately by
normalized term. Bulk approval performs deterministic collision checks first,
refreshes stale source/policy/map/mirror or resolver state before resolving a
target, quarantines unsafe decisions written by older releases, retires invalid
or conflicting selected alternatives without aborting the rest,
detects all translation-unit-local closures in one indexed pass, multiplexes
the remaining compiler references through one language-server session in
bounded validation windows, atomically admits all surviving compiler closures,
and persists the selection in one apply pass. Pending agent mirror edits stop
approval before decisions are written. The
model never writes source or
the projection. Identifier approval inserts a symbol-scoped alias; file-path
approval inserts a global path-only registry entry, revalidates the complete
projection, and migrates only the agent-facing mirror path. The real file and
its source content are unchanged.

## Transactions

The only v2 mutation intents are:

- `edit_node`: replace one current AST node. Any target range containing a
  declaration is rejected.
- `rename_symbol`: ask the compiler/LSP for a `WorkspaceEdit` rooted at the
  declaration. Edits outside the workspace or overlapping edits are rejected.

`preview_transaction` validates UTF-8 ranges, ownership, capabilities,
non-overlap, source hashes, and reparses all candidate files without writing.
It stores the exact preview at a base revision. `commit_transaction` verifies
the revision and hashes again, then reuses the durable v1 applying journal,
permission preservation, rollback, recovery, and reindex path. Structured edits
and patch-created files parse the agent-facing syntax to distinguish references
from declarations: references to an accepted alias are back-projected to the
real symbol; declaring a new symbol with an existing alias is rejected. Commit
also requires the refreshed mirror to reproduce the exact projected preview.
An explicit compiler rename replaces the old symbol identity and its reviewed
alias; after reindex the requested new name is the real and projected spelling.
Commit is idempotent: a retry returns the stored committed revision, and an
exact after-hash left by a crash is reconciled after journal recovery.

## Verification

`code-sanity verify` checks the unified mirror/map invariants and semantic
document hashes, fresh AST counts, declaration coverage (including
resolver-upgrade compatibility anchors), missing/stale documents, orphan or
incomplete aliases, alias injectivity, unresolved alias collisions, projected
syntax, and proposal targets. Live integration tests exercise Rust and
Objective-C++ LSP renames plus the mixed rename/edit regression described in
the v2 goal.

## Compatibility boundary

The original `read`, `search`, `apply-patch`, `write`, `rename`, and
`project-edit` commands remain available for existing integrations. They are
the compatibility bridge over the same unified mirror. New agents should use `read-code`, `find-code`,
`edit-node`, `rename-symbol`, transaction commands, and the corresponding MCP
tools. V1 can be removed only after its TUI/hook workflows have v2 equivalents.
