# AST/semantic v2 architecture

The v2 path keeps the real repository as the source of truth but no longer
uses a global text substitution as the agent edit contract:

```text
real source -> Tree-sitter index -> symbol-scoped projection -> MCP intent
            -> compiler/LSP WorkspaceEdit -> revision CAS -> durable journal
            -> real source + v1 mirror + semantic reindex
```

## Identity and persistence

`code-sanity index` refreshes the v1 mirror and the v2 SQLite tables in the
same exclusive workspace-lock window. The v2 schema stores workspace
revisions, documents, every named AST node, declarations, occurrences,
symbol-scoped aliases, model proposals, and previewed/committed transactions.

- `symbol_id` is derived from file, language, declaration kind, qualified
  name, and same-name ordinal. Whitespace-only reparses preserve it.
- `node_id` is derived from file and the named syntax path. It is stable across
  whitespace changes; structural edits may intentionally produce a new ID.
- an occurrence is bound locally only when the syntax index can prove one
  declaration. Shadowed/overloaded names stay unresolved until an LSP request.
- generated, vendor, dependency, and `_deps` paths are never mutable targets.

The semantic revision changes whenever documents or projected aliases change.
Every mutation requires `expected_revision`; preview and commit both compare
it. A mismatch is a conflict, never a fuzzy rebase.

## Projection

`read_code` renders real source by replacing only declaration/reference ranges
bound to an accepted `symbol_id`. Comments, strings, imports, external APIs,
and unrelated same-spelling tokens stay byte-for-byte unchanged. It returns
the projected content together with original/projected ranges, IDs,
capabilities, and the revision.

Existing v1 identifier replacements are imported only when a span exactly
matches a bound occurrence and no same-name unresolved occurrences remain.
Overloads/cross-file references therefore fail closed instead of producing a
partly renamed projection. Lexical replacements in prose are deliberately not
imported into v2. New reviewed model proposals write `proposal-v2` symbol
aliases and do not enter the global v1 alias registry.

## Language backends

All languages use the `LanguageBackend` capability contract. Missing
capabilities fail closed.

| Language | Structure | References/rename | Mutation |
| --- | --- | --- | --- |
| Rust | Tree-sitter Rust | `rust-analyzer` LSP | AST edits + LSP rename |
| C/C++ | Tree-sitter C++ | `clangd` LSP | AST edits + LSP rename |
| Objective-C | Tree-sitter Objective-C | `clangd` LSP | AST edits + LSP rename |
| Objective-C++ | Tree-sitter C++ | `clangd` LSP | AST edits + LSP rename |
| JS/TS, Python, Go | Tree-sitter grammar | unavailable | AST edits; semantic rename disabled |
| unknown | none | unavailable | read-only |

`rust-analyzer` or `clangd` must answer `--version` before the corresponding
semantic capability is advertised. `clangd` uses the repository compilation
database or `compile_flags.txt`. LSP work happens without the workspace lock;
the result is admitted only after a second revision/hash check.

## Proposals

The LLM receives owned `semantic_candidates`, each with existing
`symbol_id`/declaration `occurrence_id`, kind, qualified name, reference count,
occurrence/call lines, signature, enclosing code, origin/API-boundary evidence,
and accepted alias. Output is typed JSON and must copy existing IDs.
The engine rejects invented IDs, partial identifier substrings, ambiguous
same-name symbols, external origins, existing aliases, invalid identifiers,
collisions, and model text that is absent from the owned chunk.

Chunk decisions are carried by `symbol_id`, so overlap is deduplicated without
collapsing distinct same-spelling symbols. The model never writes source or
the projection; approval inserts a symbol-scoped alias.

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
permission preservation, rollback, recovery, and reindex path. New agent code
is never reverse-mapped merely because its spelling resembles an old alias.
Commit is idempotent: a retry returns the stored committed revision, and an
exact after-hash left by a crash is reconciled after journal recovery.

## Verification

`code-sanity verify` now checks both generations: v1 mirror/map invariants and
v2 document hashes, fresh AST counts, missing/stale documents, orphan aliases,
and proposal targets. Live integration tests exercise Rust and Objective-C++
LSP renames plus the mixed rename/edit regression described in the v2 goal.

## Compatibility boundary

The original `read`, `search`, `apply-patch`, `write`, `rename`, and
`project-edit` commands remain available for existing integrations. They are
the lexical v1 bridge. New agents should use `read-code`, `find-code`,
`edit-node`, `rename-symbol`, transaction commands, and the corresponding MCP
tools. V1 can be removed only after its TUI/hook workflows have v2 equivalents.
