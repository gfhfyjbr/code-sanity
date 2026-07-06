# Hooks and adapter capability matrix

This documents what each agent adapter can and cannot do, and why hooks are a
**guardrail, not a full enforcement boundary**. It reflects the documented
behavior of each host's hook/plugin API (see `PLAN.md` §2 for sources).

Legend: ✅ supported · 🟡 best-effort / partial · ❌ not intercepted / n/a

| Capability | opencode plugin | Codex hooks | Claude Code hooks | MCP server |
| --- | --- | --- | --- | --- |
| Redirect file `read` to mirror | ✅ rewrites `filePath`/`path` | 🟡 shell `cat`/`nl`/`head`/`tail` → `code-sanity read` | 🟡 denies raw `Read` in strict; steers to MCP | ✅ `read_file` returns sanitized |
| Redirect `search`/`grep` to mirror | ✅ rewrites tool path | 🟡 not rewritten (deny/allow only) | 🟡 steer via context | ✅ `search`/`list_files` |
| Bridge `edit`/`write` to real repo | ✅ mirror edit → `project-edit` | ➡️ via MCP `apply_patch` | ➡️ via MCP `apply_patch` | ✅ `apply_patch` |
| Block raw real-repo edit (strict) | ✅ throws | ✅ `permissionDecision: deny` | ✅ `permissionDecision: deny` | n/a (only sanctioned ops) |
| Nudge to sanctioned tools (guided) | ✅ | ✅ message | ✅ `SessionStart` context | n/a |
| Sanitize shell/build output | 🟡 use `code-sanity sh` | 🟡 use `code-sanity sh` | 🟡 use `code-sanity sh` | n/a |
| Sync after edit | ✅ `file.edited` → `sync` | ✅ `PostToolUse` → `sync` | ✅ `PostToolUse` → `sync` | ✅ apply reindexes |
| Reads via arbitrary shell/IDE/LSP | ❌ not intercepted | ❌ not fully intercepted | ❌ not intercepted | ❌ out of band |

`➡️` means the capability is provided through the MCP `apply_patch` tool rather
than the hook itself.

## Per-adapter notes

### opencode (`.opencode/plugins/code-sanity.ts`)
The plugin API exposes tool-argument mutation (`tool.execute.before`) and
post-execution hooks (`tool.execute.after`), so opencode gets the most complete
adapter: reads/search/list are physically redirected to the mirror, edits land on
the mirror and are back-projected with `code-sanity project-edit`, and strict mode
throws on real-repo edits. It cannot intercept reads issued through `bash` or
other non-file tools.

### Codex (`.codex/hooks.json`, `hooks/{pre,post}_tool_use.py`)
`PreToolUse` can deny or rewrite supported tool calls (`apply_patch`, `Bash`, MCP
tools) with `permissionDecision` and `updatedInput`. code-sanity denies raw
real-repo edits in strict, nudges in guided, and best-effort rewrites obvious
shell reads. Codex documentation is explicit that `PreToolUse` is a guardrail:
it does not intercept every shell path (`unified_exec`, some tools), and
`PostToolUse` cannot undo side effects.

### Claude Code (`.claude/settings.json`, `hooks/*.py`)
`PreToolUse` can block a tool via `permissionDecision: deny`; code-sanity blocks
raw real-repo `Read`/`Edit`/`Write` in strict (edits in guided) and steers to the
MCP server. `SessionStart` injects guidance to use the code-sanity tools;
`PostToolUse` syncs. Claude's API does not offer a general `updatedInput` rewrite
contract, so the adapter is "guard + MCP", not a transparent read rewrite.

### MCP server (`code-sanity serve`)
The sanctioned tool surface: `read_file`, `search`, `list_files` return sanitized
content only; `apply_patch` and `verify` go through the bridge. It is the cleanest
path but cannot prevent the agent from reading the real repo through some other
tool — that is what the hooks and strict mode are for.

## Why hooks are not a boundary

- No hook API guarantees that **every** byte reaching the model passed through the
  sanitizer; shell paths, IDE context loaders, LSP, file upload, and compiler
  output are not uniformly interceptable.
- `PreToolUse`-style hooks cover the tools named in their matchers; a new or
  unmatched tool is not guarded until the matcher is updated.
- Output-side hooks (`PostToolUse`) see results but cannot reliably rewrite them.

For hard guarantees, combine strict mode with an OS-level sandbox/overlay whose
only visible tree is the sanitized mirror. See [THREAT_MODEL.md](THREAT_MODEL.md).

## Enforcement mode reference

The mode lives in `.code-sanity/config.toml` (`mode = "soft" | "guided" |
"strict"`) and is read by the hooks directly. `code-sanity mode` prints it. Modes
are summarized in [THREAT_MODEL.md](THREAT_MODEL.md#trust-boundaries-and-enforcement-tiers).
