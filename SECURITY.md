# Security Policy

code-sanity is a privacy tool: it exists to keep private context out of AI
agents' views of a repository. Leaks across that boundary (real terms surviving
into the mirror, MCP output, `sh`/`strict-run` output, or the embedding index)
are security bugs, not cosmetic ones — as are patch-bridge issues that let a
crafted diff write outside the workspace or corrupt the real repository.

## Reporting a vulnerability

Please **do not open a public issue** for suspected vulnerabilities.

- Preferred: [GitHub private vulnerability reporting](https://github.com/gfhfyjbr/code-sanity/security/advisories/new)
  ("Report a vulnerability" on the Security tab).
- Include: version (`code-sanity --version`), platform, a minimal repro
  (config + input file + command), and what leaked or got corrupted.

You should get a first response within a week. Fixes ship as a patch release;
credit is given unless you ask otherwise.

## Supported versions

Only the latest release line receives security fixes.

| Version | Supported |
| --- | --- |
| latest 0.x release | yes |
| older | no — upgrade |

## Scope notes

The enforcement tiers and known bypasses are documented in
[docs/THREAT_MODEL.md](docs/THREAT_MODEL.md). Hooks and adapters are guardrails,
not a kernel sandbox — "an agent read the real repo through a tool the hooks
don't cover" is a documented residual risk, not a vulnerability; "a term the
policy covers survived into sanitized output" is.
