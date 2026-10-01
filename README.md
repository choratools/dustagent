# DustAgent

**Small, focused agents your agent can call.**

[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-edition%202024-orange.svg)](https://www.rust-lang.org)

> 직접 `dust`를 쓰려고요? Agent한테 시키세요.
> Let your agent create, run, and improve the agents it needs.

DustAgent is a Rust runtime for **Agent as an Application (AaaA)**. Each application is a JSON manifest with a focused prompt and its own MCP servers. It accepts a task, calls an OpenAI-compatible model, runs tool calls when needed, and returns the result.

Your coding agent or automation owns the larger task. DustAgent supplies focused workers: generate a commit message, extract data, diagnose an error, or produce a patch. With experience enabled, it also records runs, investigates relevant documentation, and selects examples to reuse.

## Ask your agent

Give your existing coding agent a task like this:

> Use DustAgent to create an agent that summarizes text as JSON. Run it on my input, inspect the result, and enable experience so later runs can reuse useful examples. Add a checker for the output contract and let `dust learn` investigate the recorded results.

The commands below are the interface that agent uses. There are no interactive confirmation prompts in the CLI.

## Install and connect a model

```bash
git clone https://github.com/choratools/dustagent.git
cd dustagent
cargo install --path . --locked

export OPENAI_API_KEY="your-api-key"
# For another OpenAI-compatible server:
# export OPENAI_BASE_URL="http://localhost:30000/v1"
```

`OPENAI_BASE_URL` defaults to `https://api.openai.com/v1`. The adapter requires `OPENAI_API_KEY`, including for local servers; use a placeholder only if your local server accepts one. Use `--model` to select the model served by your endpoint. `run` and `learn` otherwise use the app's `default_model`, falling back to `gpt-4o-mini`; `new` defaults to `gpt-4o-mini`.

Run examples from the repository root so the bundled `apps/` manifests and checker paths resolve. External MCP tools require their declared commands, such as `uvx` or `npx`, to be installed separately.

## Create, run, compose

```bash
# Generate apps/summarizer.json from a task description.
dust new summarizer "Summarize input as JSON with one summary field"

# Inspect the generated manifest on stdout instead of saving it.
dust new reviewer "Review a diff and return JSON comments" --stdout

# Run an app with an argument or piped input.
dust run summarizer "The text to summarize"
git diff --cached | dust run commit_gen

# Compose workers using ordinary pipes.
git diff --cached | dust run commit_gen | dust run summarizer
```

Place options before the input text, for example `dust run summarizer --model MODEL "input"`. You can also pass a manifest path instead of an app name.

`dust new` generates a manifest using the scaffold prompt; the calling agent should inspect the generated prompt and tool declarations before running it. App names resolve against the current directory and its `apps/` directory.

`dust run` writes the model's final response to STDOUT and diagnostics to STDERR. The app prompt determines whether that response is text, JSON, or patch blocks. An `output_format` declaration guides use and reinforcement checks; it does not enforce a response schema during ordinary execution. Reaching the turn limit can return an empty response, so callers should check the result as well as the exit code.

## Improve from experience

```bash
# Record this run and review earlier pending examples before executing.
dust run summarizer --experience "The text to summarize"

# Investigate pending records and persist reuse decisions.
dust learn summarizer

# Inspect results, checker evidence, sources, and decision reasons.
dust learn summarizer --list
```

No run IDs or individual approval commands are needed. The reinforcement flow is:

```text
Recorded input/output
  → output-format check
  → configured local checker, if present
  → relevant documentation lookup
  → model review with the collected evidence
  → persist selection and reason
  → reuse related examples in later runs
```

The CLI proposes up to two official documentation URLs per reviewed example and fetches their content. Set `research.urls` in the manifest to use fixed sources instead. This is URL discovery and document retrieval, rather than search-engine integration. Failed retrievals are recorded as failures, not source evidence.

Review processes at most ten pending completed examples per invocation. Selected examples are matched by word overlap; at most three examples and 8 KiB of their combined input/output enter the next task's context. The app prompt and tool declarations remain in place. Model weights are not changed.

Recording is opt-in. Both `run` and `learn` accept `--experience-dir PATH`; on `run`, that option also enables experience. The default store is `~/.dustagent/experiences/`. Records are scoped by the full manifest: changing the prompt, tools, checker, or research settings starts a separate set of examples and preserves the old files.

Records include task inputs and outputs. Review sends those records and collected evidence to the configured model API and can make additional HTTP requests. A selected example is useful reference material, not proof of correctness. Checker success establishes only what the checker tested; document excerpts establish only what was actually fetched.

### Add an actual checker

An app can include these fields:

```json
{
  "validation": {
    "command": "python3",
    "args": ["examples/validators/check_summary.py"],
    "timeout_ms": 5000
  },
  "research": {
    "urls": ["https://www.rfc-editor.org/rfc/rfc8259.txt"]
  }
}
```

The fixed command receives `{ "input": "...", "output": "..." }` as JSON on STDIN. It must exit `0` and return a JSON verdict on STDOUT:

```json
{"passed": true, "reason": "Checked the summary key and 200-character bound"}
```

Failed checks prevent selection. Commands run in the current directory with the current environment; the model does not choose or modify them. A test command such as `cargo test` needs a wrapper that converts its actual result into this verdict format. Timeout defaults to five seconds and supports up to sixty seconds; stdout and stderr are limited to 8 KiB each.

Try the bundled example:

```bash
dust run checked_summary --experience "DustAgent runs focused agents from JSON manifests."
dust learn checked_summary
dust learn checked_summary --list
```

Its checker validates the JSON shape and summary length, not factual accuracy. See [the experience guide](docs/11_경험_기반_자기강화.md) for storage limits, source retrieval, and the library interfaces. [reinforcer](apps/reinforcer.json) exposes the review prompt as a separate app; the CLI connects the actual checks, retrieval, and persistence.

## App manifests and scoped tools

```json
{
  "$schema": "dustagent/app-v1",
  "name": "crawler",
  "description": "Extract structured data from a URL",
  "system_prompt": "Fetch the supplied URL and return extracted data as JSON only.",
  "default_model": "gpt-4o-mini",
  "mcp_servers": {
    "fetch": {
      "command": "uvx",
      "args": ["mcp-server-fetch"]
    }
  },
  "max_turns": 10,
  "output_format": "raw_json"
}
```

`name`, `description`, and `system_prompt` describe the app; `$schema` is a convention marker. The runtime accepts these as optional fields. `mcp_servers` defaults to an empty map, and `max_turns` defaults to ten. Common output formats are `text`, `raw_json`, and `search_replace_patch`.

Only the app's declared external MCP servers are started. Their tools are named `{server}__{tool}`. Native tools are also available on every app, with no separate MCP process:

| Tool | Arguments | Purpose |
| :--- | :--- | :--- |
| `dustagent__sleep` | `ms` | Async delay in milliseconds |
| `dustagent__timestamp` | Optional `format`: `unix_ms`, `iso8601`, or `both` | Current UTC time |
| `dustagent__uuid` | None | Generate a UUID v4 |
| `dustagent__env_get` | `name` | Read an environment variable |
| `dustagent__hash` | `input` | SHA-256 hex digest |

Scoped declarations keep unrelated external tools out of a task. They are not a sandbox: declared tools and checker commands retain their own capabilities, and native environment access remains available.

## Patch a file

```bash
# Generate SEARCH/REPLACE blocks for inspection.
dust patch --file src/lib.rs --dry-run "Simplify this function without changing behavior"

# Apply the generated blocks to the file.
dust patch --file src/lib.rs "Simplify this function without changing behavior"
```

The patcher reads the target file, asks the `patcher` app for SEARCH/REPLACE blocks, and applies them using the Rust fuzzy patch engine. `--range 15:30` focuses the supplied code context. `patch` takes its instruction as an argument; it does not consume instructions from STDIN.

## Philosophy and implementation

- **One task per app.** A manifest defines a focused worker that a calling agent can compose with others.
- **No interactive workflow.** The caller supplies input and handles the output, validation, and larger task.
- **Explicit capabilities.** Apps declare their external MCP servers and local checkers.
- **Ordinary composition.** Use files, pipes, and existing automation to connect workers.

The Rust runtime provides manifest loading, non-streaming OpenAI-compatible chat completion calls, scoped stdio MCP clients, native utility tools, fuzzy patching, and bounded experience review. No resident service is required; external MCP servers and configured checkers run as subprocesses when invoked.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The current implementation has been checked with unit and integration tests, a mock-LLM CLI flow, actual checker execution, and a public HTTPS documentation fetch. These checks establish the execution flow; improvement in real-model task quality has not been measured.

See [CONTRIBUTING.md](CONTRIBUTING.md), [the documentation index](docs/index.md), and [the scaffolding guide](docs/10_dust_new_스캐폴딩.md).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
