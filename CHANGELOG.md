# Changelog

## 2.0.0 — 2026-10-18

- Bundle app-local callable tools under `tools/` alongside private skills. Models call declared `app__<name>` tools with schema-validated JSON input and receive one JSON result; no MCP server is required. Source directories, installed apps, plain archives, and encrypted packages use the same runtime.
- Validate final responses against an optional native JSON output schema. Invalid candidates receive bounded repair feedback within the execution budget.
- Enable `wrap_up_on_limit` or `--wrap-up-on-limit` to grant three finalization turns after the normal turn limit. CLI overrides are available for both `dust run` and `dust acp`; the default is disabled and overall deadlines remain unchanged.
- Default the configured context window to 256K. Preserve initial output instructions through compaction and retain wrap-up notices across staged summaries.
- Configure retained and model-facing tool-output limits per app while preserving separately bounded original-history access.
- Create private transcript directories without unnecessary permission changes when already private, supporting restricted execution environments.

App-local tools require their declared interpreter and dependencies on the host. Tools run with the host user's permissions; existing MCP-specific chroot settings do not apply to app-local tools. Initial prompts are preserved verbatim, but compaction summaries remain unverified historical references.

## 1.0.0 — 2026-10-11

First stable release of DustAgent, a Rust runtime for focused Agent-as-an-Application workers.

- Run JSON-manifest agents through the `dust` CLI, with OpenAI-compatible providers or the existing Codex CLI login.
- Connect external MCP tools over stdio, use native `dustagent__` tools, and serve agents to ACP clients.
- Create, validate, package, install, and run app-scoped agents with private skills; `.dustpkg` archives can be encrypted with a passphrase.
- Resume safe checkpoints, preserve searchable original transcripts through context compaction, and reconcile uncertain external tool outcomes without replaying them.
- Inspect bounded provider diagnostics and token usage; retry eligible provider failures while respecting execution budgets.
- Keep MCP execution serial by default. Parallel batches require explicit read-only or idempotent declarations and distinct servers.
- Optionally record and reuse execution examples, research relevant documentation, and validate outputs with app-defined checkers.

Build from source with Rust 1.89 or newer. See the [README](README.md) for installation and usage.
