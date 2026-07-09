# Connecting agents to the code-sanity MCP server

`code-sanity serve` speaks the [Model Context Protocol](https://modelcontextprotocol.io) over stdio (JSON-RPC 2.0, one message per line). It exposes six tools, all backed by the same bridge the CLI uses:

| Tool | Input | Returns |
| --- | --- | --- |
| `read_file` | `{ "path": "src/lib.rs" }` | sanitized file content |
| `search` | `{ "query": "...", "glob": "*.rs"?, "max_results"? }` | `path:line:column:text` lines (sanitized, capped) |
| `list_files` | `{ "glob": "src/**"? }` | repo-relative mirror paths |
| `semantic_search` | `{ "query": "...", "k"? }` | `path:start-end score preview` lines (sanitized); requires embeddings enabled + `embed-index` |
| `apply_patch` | `{ "patch": "<unified diff>", "agent"?, "session_id"?, "dry_run"? }` | applied files + workspace-relative journal path (`dry_run: true` plans/validates only) |
| `verify` | `{}` | tracked-file consistency check |

`read_file`, `search`, and `list_files` only ever read `.code-sanity/mirror`, so the model never sees raw identifiers/comments. Glob parameters use gitignore-style dispatch: without `/` they match file names at any depth (`*.rs`); with `/` they match the repo-relative path (`src/**/*.rs`). Tool output never carries host-absolute paths (journal references are workspace-relative; errors are redacted and root-scrubbed). `apply_patch` accepts a diff written against sanitized mirror paths (`a/src/lib.rs`, `b/src/lib.rs`, or `.code-sanity/mirror/src/lib.rs`) and projects it back onto the real repo through the span-aware, conflict-safe bridge.

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

Codex then offers `read_file`, `search`, `list_files`, `semantic_search`, `apply_patch`, and `verify`. Pair this with `code-sanity install-hooks --agent codex` to deny raw real-repo edits in strict mode and steer reads toward these tools.

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
