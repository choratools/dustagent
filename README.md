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

`dust run` writes the model's final response to STDOUT and diagnostics to STDERR. The app prompt determines whether that response is text, JSON, or patch blocks. An `output_format` declaration guides use and reinforcement checks; it does not enforce a response schema during ordinary execution. Incomplete runs return a nonzero exit code and emit no raw result. Use `--json` or `--report` to retain partial responses and tool observations.

## Execution reports and budgets

```bash
# Structured result on stdout, including evidence when a run stops early.
dust run crawler --json --max-turns 15 --timeout-ms 120000 --tool-timeout-ms 15000 "https://example.com"

# Keep normal result output and write a private JSON report file.
git diff --cached | dust run commit_gen --report reports/commit.json
```

Reports distinguish final responses from tool observations. They contain `stop_reason`, `output`, `turns_used`, `elapsed_ms`, `turns`, `tool_calls`, `validation`, and `warnings`. On an incomplete run, `output` can contain the last partial assistant text; it is not a completed task result. Tool records retain actual arguments, outputs, failures, and durations. Coverage metadata supplied by a tool stays in its recorded output; the runtime does not infer that all links or documents were observed.

| Exit code | Stop reason | Meaning |
| :--- | :--- | :--- |
| `0` | `completed` | Nonempty final response; configured output checker passed, if present |
| `1` | CLI/setup error | Invalid input, configuration, or report-file I/O failure |
| `2` | `turn_limit` | Model turn budget exhausted |
| `3` | `time_limit` / `tool_timeout` | Overall or individual tool deadline exhausted |
| `4` | `empty_response` | Model ended without nonempty final content |
| `5` | `execution_error` | Execution failed, such as a provider request error |
| `6` | `validation_failed` | Configured checker rejected the output |

Without `--json`, only completed runs emit task output on STDOUT. With `--json`, failed runs also emit their report; callers must still inspect the exit code. `--report` saves the report atomically with owner-only permissions on Unix. Reports can include sensitive task and tool data.

Defaults are ten model turns, five minutes overall, and thirty seconds per tool call. App fields `max_turns`, `timeout_ms`, and `tool_timeout_ms` set defaults; CLI options override them. Timeout values support 1–86400000 milliseconds. The CLI's overall deadline includes MCP startup, automatic experience review, tool discovery, model requests, and tool calls. Bounded cancellation cleanup can add up to one second and shutdown has a separate five-second budget. Synchronous input/report file I/O is outside the async deadline.

Timed-out MCP clients are discarded to avoid consuming a late response as a later request's result. A timed-out remote operation can still have executed; the runtime does not automatically replay it. Recovered tool failures remain visible in the report and prevent admission as successful experience examples. Output-check failures also prevent admission.

Library callers use `execute_report()` or `execute_report_with_experience()` for structured results. Existing `execute()` methods return errors on incomplete execution. Library execution budgets cover discovery and execution; initialization is separately bounded. `completed` describes the execution contract, not exhaustive crawl coverage or factual correctness.

Reports are written at termination. Use a separate checkpoint to retain the conversation during execution:

```bash
dust run crawler --checkpoint state/crawl.json --max-turns 10 "https://example.com"
dust run crawler --resume state/crawl.json --max-turns 10 --json
```

Resume uses the original input and requires the same manifest and working directory. Each resumed invocation grants an additional turn/time budget. Confirmed tool results remain in the conversation; they are not automatically replayed. MCP processes restart, so browser sessions and remote state are not restored. The model can still request new operations.

Checkpoints are atomically saved with filesystem synchronization, owner-only Unix permissions, and a 32 MiB size limit. The CLI holds an exclusive sidecar lock released automatically when its process exits. A safe boundary after a tool batch or before a model request can resume; an interrupted tool call, unknown transport outcome, or interrupted output checker cannot. Completed runs also cannot resume. There is no force-resume option or automatic continuation. Keep `--report` and checkpoint paths distinct. See [checkpoint behavior](docs/13_체크포인트_및_재개.md) for details. Building from source requires Rust 1.89 or newer.

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

The checker also runs on the final response before execution is marked complete. The fixed command receives `{ "input": "...", "output": "..." }` as JSON on STDIN. It must exit `0` and return a JSON verdict on STDOUT:

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
