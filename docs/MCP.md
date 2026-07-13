# Connecting agents to the code-sanity MCP server

`code-sanity serve` speaks the [Model Context Protocol](https://modelcontextprotocol.io) over stdio (JSON-RPC 2.0, one message per line). V2 tools return both text content and `structuredContent`; the v1 tools remain for compatibility.

| Tool | Input | Returns |
| --- | --- | --- |
| `workspace_snapshot` | `{}` | semantic revision and index counts |
| `find_code` | `{ "query": "...", "limit"? }` | owned symbols with stable IDs and ranges |
| `read_code` | `{ "path": "src/lib.rs" }` | symbol-scoped projection, AST nodes, occurrences, capabilities, revision |
| `references` | `{ "symbol_id": "sym_..." }` | compiler/LSP reference locations |
| `edit_node` | `{ "node_id", "replacement", "expected_revision" }` | persisted single-intent preview |
| `rename_symbol` | `{ "symbol_id", "new_name", "expected_revision" }` | compiler/LSP rename preview |
| `preview_transaction` | `{ "expected_revision", "intents": [...] }` | validated multi-intent preview and transaction ID |
| `commit_transaction` | `{ "transaction_id", "expected_revision", "agent"?, "session_id"? }` | committed revision, files, journal |
| `read_file` | `{ "path": "src/lib.rs" }` | sanitized file content |
| `search` | `{ "query": "...", "glob": "*.rs"?, "max_results"? }` | `path:line:column:text` lines (sanitized, capped) |
| `list_files` | `{ "glob": "src/**"? }` | projected repo-relative mirror paths |
| `semantic_search` | `{ "query": "...", "k"? }` | `path:start-end score preview` lines (sanitized); requires embeddings enabled + `embed-index` |
| `apply_patch` | `{ "patch": "<unified diff>", "agent"?, "session_id"?, "dry_run"? }` | applied files + workspace-relative journal path (`dry_run: true` plans/validates only) |
| `verify` | `{}` | tracked-file consistency check |

`read_code` is the preferred read path. Its content is the same combined
lexical + semantic projection stored in the mirror; semantic aliases apply only
at AST occurrences bound to one `symbol_id`. Paths, names, qualified names,
ranges, and byte offsets are all projected as one coordinate system. Mutation
is a two-step preview/commit protocol with revision CAS. `edit_node` cannot
touch a declaration-containing range and back-projects accepted alias
references in replacement syntax, while `rename_symbol` accepts only a
compiler/LSP `WorkspaceEdit` contained by the repository.

The compatibility `read_file`, `search`, and `list_files` read the same
`.code-sanity/mirror`, including accepted semantic aliases. Every directory component and filename stem uses the
deterministic sanitized projection; the final extension is preserved. Inputs
may use a current projected path (preferred), while tracked real spellings are
accepted as a host-compatibility fallback without being echoed back. Glob
parameters use gitignore-style dispatch over projected paths: without `/` they
match file names at any depth (`*.rs`); with `/` they match the repo-relative
path (`src/**/*.rs`). Tool output never carries host-absolute or real tracked
paths. `apply_patch` uses the combined span/symbol back-projection bridge; new integrations should use v2
structured transactions. See [SEMANTIC_V2.md](SEMANTIC_V2.md) for invariants
and capability boundaries.

Inspect the manifest without starting a session:

```bash
code-sanity serve --once
```

Run `code-sanity index` once before serving so the mirror exists.

## Codex

Add an MCP server to `~/.codex/config.toml` (global) or `<repo>/.codex/config.toml`:

```toml
[mcp_servers.code-sanity]
command = "code-sanity"
args = ["--root", ".", "serve"]
```

Codex then offers the AST/semantic v2 tools plus the legacy mirror tools. In the agent system prompt, require `workspace_snapshot` before reads or edits, `read_code`/`find_code` for context, and `preview_transaction` followed by `commit_transaction` for mutation. Pair this with strict-mode filesystem isolation when raw filesystem tools must be unavailable.

## Claude Code

Register the server with `.mcp.json` at the repo root:

```json
{
  "mcpServers": {
    "code-sanity": {
      "command": "code-sanity",
      "args": ["--root", ".", "serve"]
    }
  }
}
```

Or from the CLI:

```bash
claude mcp add code-sanity -- code-sanity --root . serve
```

Tools appear as `mcp__code-sanity__read_file`, `mcp__code-sanity__apply_patch`, etc. Pair with `code-sanity install-hooks --agent claude` to guard raw `Read`/`Edit`/`Write` in strict mode.

## opencode

The opencode plugin (`install-hooks --agent opencode`) already bridges read/edit tools. If you also want the MCP tools available, add to `opencode.json`:

```json
{
  "mcp": {
    "code-sanity": {
      "type": "local",
      "command": ["code-sanity", "--root", ".", "serve"],
      "enabled": true
    }
  }
}
```

## Guardrail, not a boundary

MCP tools are the sanctioned path, but they do not guarantee every byte reaches the model through the sanitizer: a shell `cat`, an IDE context loader, or a filesystem MCP can still read the real repo. For hard isolation, run the agent in strict mode against a sanitized worktree (`code-sanity strict-run`) where the real root is not present. See [THREAT_MODEL.md](THREAT_MODEL.md).
