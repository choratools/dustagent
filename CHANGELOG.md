# Changelog

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
