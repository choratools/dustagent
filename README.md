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

# Default: reuse your existing file-backed Codex login.
codex login

# Optional: use an API key instead.
# export OPENAI_API_KEY="your-api-key"
# For another OpenAI-compatible server:
# export OPENAI_BASE_URL="http://localhost:30000/v1"
```

CLI commands automatically select the model connection. Explicit `OPENAI_API_KEY` uses the OpenAI-compatible API; `OPENAI_BASE_URL` overrides its address and requires a key, including a placeholder accepted by a local server. With neither variable set, Dust reads `$CODEX_HOME/auth.json` (default `~/.codex/auth.json`): a cached API key uses the OpenAI API, while a ChatGPT login uses the Codex backend directly. No Codex subprocess or proxy is started. Keyring-only login is not supported.

`--model` overrides the app's `default_model`; without either, the API default is `gpt-4o-mini` and the Codex default is `gpt-6.1-sol`. Existing manifests naming API/local-only models need a Codex-compatible `--model` or a manifest update. Account access and usage limits still apply. See [Codex authentication](docs/17_Codex_인증_및_모델_연결.md) for token refresh and compatibility limits.

Run examples from the repository root so the bundled `apps/` manifests and checker paths resolve. External MCP tools require their declared commands, such as `uvx` or `npx`, to be installed separately.

## Create, run, compose

```bash
# Generate apps/summarizer/app.json and an empty skills/ directory.
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

`dust new` generates a manifest using the scaffold prompt; the calling agent should inspect the generated prompt and tool declarations before running it. Existing JSON apps remain supported. App names resolve against the current directory, its `apps/` directory, then the installed package store.

`dust run` writes the model's final response to STDOUT and diagnostics to STDERR. The app prompt determines whether that response is text, JSON, or patch blocks. An `output_format` declaration guides use and reinforcement checks; it does not enforce a response schema during ordinary execution. Incomplete runs return a nonzero exit code and emit no raw result. Use `--json` or `--report` to retain partial responses and tool observations.

## App development lifecycle

A calling agent owns the cycle: create an app, write its skills, run and validate it, improve from experience, package a version, then distribute it. Operational results feed back into the next revision.

1. **Create the app.** Generate a focused manifest and an empty skill directory. Inspect the generated prompt and MCP declarations.

   ```bash
   dust new crawler "Extract structured information from web documents"
   ```

2. **Write its skills.** Put instructions in `skills/<name>/SKILL.md`, detailed guidance in `references/`, templates in `assets/`, and optional helper code in `scripts/`. Declare each permitted skill in `app.json`. The native skill reader exposes only those declared skills; it does not execute scripts.

3. **Run and validate locally.** Exercise both normal and failure cases. Inspect stop reasons, tool observations, and the actual output. Configure an output checker when a deterministic check is available. Runtime completion alone does not establish task quality or complete coverage.

   ```bash
   dust run ./apps/crawler --json "test input"
   ```

4. **Improve from experience.** Record runs and investigate which examples are useful for future tasks.

   ```bash
   dust run ./apps/crawler --experience "input"
   dust learn ./apps/crawler
   ```

   Current reinforcement reviews recorded examples for few-shot reuse. It does not automatically rewrite skill files. The calling agent edits skills or prompts and validates the revised app again.

5. **Version and package.** Set `package.version` in `app.json`, then build and test the actual distribution artifact. For version `0.1.0`:

   ```bash
   dust pack ./apps/crawler
   dust run ./crawler-0.1.0.dustpkg --json "test input"
   ```

6. **Distribute and operate.** Another agent can run the archive directly, or optionally install it and use its package name. Installation does not replace an existing package. Feed operational results into the next improvement cycle. Use checkpoints for longer runs; changing package or declared skill contents prevents resuming an old checkpoint.

DustAgent provides these individual commands. A single command that automatically manages the entire development and release lifecycle is not implemented.

## CLI reference

| Command | Purpose |
| :--- | :--- |
| `dust run <APP> [INPUT...]` | Run an app by name, JSON manifest path, package directory, or `.dustpkg` path |
| `dust acp <APP>` | Serve an app through ACP v1 over stdio for compatible clients |
| `dust new <NAME> <DESCRIPTION>` | Generate `apps/<NAME>/app.json` and an empty `skills/` directory using an LLM |
| `dust pack <SOURCE>` | Pack an application directory into a `.dustpkg` archive |
| `dust install <SOURCE>` | Install a local directory or `.dustpkg` package |
| `dust learn <APP>` | Investigate and review recorded examples for future reuse |
| `dust patch --file <FILE> <INSTRUCTION>` | Apply generated SEARCH/REPLACE blocks to a file |
| `dust help [COMMAND]` | Show command help |
| `dust --version` | Show the installed version; short form: `-V` |

Every subcommand supports `-h` / `--help`.

### Run

```bash
dust run crawler --model MODEL --max-turns 15 "input"
dust run ./apps/coverage-reader "input"
dust run ./coverage-reader-0.1.0.dustpkg "input"
git diff --cached | dust run commit_gen
```

| Option | Purpose |
| :--- | :--- |
| `-m, --model MODEL` | Override the model |
| `--max-turns N` | Set this invocation's model turn budget |
| `--timeout-ms MS` | Set the overall time budget, including startup and automatic experience review |
| `--tool-timeout-ms MS` | Set the individual tool timeout |
| `--json` | Emit a structured report, including incomplete results |
| `--report PATH` | Save the termination report to a file |
| `--checkpoint PATH` | Override the automatic temporary checkpoint destination; requires a new file |
| `--resume PATH` | Resume saved conversation; optional input appends a follow-up user prompt |
| `--experience` | Record runs and review/reuse useful past examples |
| `--experience-dir PATH` | Override the experience directory; implies `--experience` |

Place options before input text. Without input arguments, `run` reads STDIN. `--resume` accepts optional positional follow-up text and cannot be combined with `--checkpoint`. Without follow-up text it continues the saved task; it does not read STDIN. Inspect the exit code even when using `--json`.

### ACP

```bash
dust acp ./apps/coverage-reader
dust acp ./coverage-reader-0.1.0.dustpkg --model MODEL --timeout-ms 120000
```

Configure an ACP client to launch `dust` with arguments `["acp", "/absolute/path/to/app"]` and the same API environment or file-backed Codex login used by `run`. `APP` accepts an app name, manifest, directory, or archive; installation is optional. Options are `--model`, `--max-turns`, `--timeout-ms`, and `--tool-timeout-ms`.

STDIN/STDOUT carry newline-delimited JSON-RPC only. Each session preserves conversation, working state, and tool evidence, and uses its own client-supplied working directory. Model turns and time budgets reset per prompt. Tool progress and assistant messages are emitted as `session/update`; model text is delivered after each model response, rather than token by token.

Cancellation interrupts active work. A cancelled model request permits follow-up; an uncertain tool/checker outcome blocks further execution in that session. Create a new session after checking the external outcome. App-owned skills and declared MCP servers remain the capability boundary: client MCP configuration cannot add tools. Text and resource links are supported; links are passed as metadata without fetching. Session reload, image/audio input, client filesystem/terminal calls, and interactive authentication are not implemented. See [ACP interface](docs/16_ACP_인터페이스.md) for protocol details and limits.

### New

```bash
dust new summarizer "Summarize input as JSON"
dust new reviewer "Review a diff" --stdout
```

| Option | Purpose |
| :--- | :--- |
| `-m, --model MODEL` | Select the scaffold model |
| `--stdout` | Print the generated manifest without creating files |

### Pack and install

```bash
dust pack ./apps/summarizer -o ./summarizer-0.1.0.dustpkg
dust install ./summarizer-0.1.0.dustpkg
dust run summarizer "input"
```

| Command option | Purpose |
| :--- | :--- |
| `pack -o, --output PATH` | Set the archive output path; defaults to `<name>-<version>.dustpkg` |
| `install --store PATH` | Override the installation store; defaults to `~/.dustagent/packages` or `DUST_PACKAGE_HOME` |

The archive output must be outside its source directory, with an existing parent directory. Existing output files and installed packages are not overwritten. For an explicit `--store`, set `DUST_PACKAGE_HOME` to the same directory when running by installed name, or run the installed directory directly.

### Learn

```bash
dust learn summarizer
dust learn summarizer --list
```

| Option | Purpose |
| :--- | :--- |
| `--list` | Inspect records and review reasons without calling the model |
| `-m, --model MODEL` | Select the review model |
| `--experience-dir PATH` | Override the experience directory |

### Patch

```bash
dust patch --file src/app.py --range 15:30 --dry-run "Add error handling"
```

| Option | Purpose |
| :--- | :--- |
| `-f, --file FILE` | Target file; required |
| `-r, --range RANGE` | Optional line range, such as `15:30` or `42` |
| `-m, --model MODEL` | Override the model |
| `--dry-run` | Print generated patch blocks without modifying the file |

`patch` takes its instruction as an argument rather than reading it from STDIN.

## App-owned skills and portable packages

```bash
# Run source or an archive without installing it.
dust run ./apps/coverage-reader "discovered=290 observed=100 distinct items"
dust pack ./apps/coverage-reader
dust run ./coverage-reader-0.1.0.dustpkg "discovered=290 observed=100 distinct items"

# Installation is optional; it gives the app a reusable local name.
dust install ./coverage-reader-0.1.0.dustpkg
dust run coverage-reader "discovered=290 observed=100 distinct items"
```

A package contains root `app.json`, its own `skills/`, and optional README/LICENSE files. The manifest declares `package: {"name":"coverage-reader","version":"0.1.0","dust_version":">=0.1.0"}` and `skills: ["coverage"]`. Each declared skill has YAML-frontmatter `SKILL.md` plus optional `references/`, `scripts/`, and `assets/`, following the [Agent Skills directory format](https://agentskills.io/specification). Names and descriptions are shown first; `dustagent__read_skill` loads instructions or UTF-8 resource files on demand. Undeclared skills, path traversal, and symlinks are rejected. Reads are limited to 64 KiB; binaries can be bundled but cannot be read through this text tool.

`.dustpkg` is a bounded tar.gz archive with root contents. Packing and installing validate declared skills and never run hooks or skill scripts. Existing destinations are not overwritten. Runtime compatibility accepts an exact `X.Y.Z` or `>=X.Y.Z`. The default store is `~/.dustagent/packages`; set `DUST_PACKAGE_HOME`, or use `install --store` with the same store when resolving installed names. Archive execution uses a private temporary directory and removes it after the run. Package content hashes bind checkpoint resume across extraction locations.

Skill access restrictions apply to the native skill reader. Explicit MCP servers and checkers keep their declared capabilities; this is not a process sandbox. Their executables/dependencies must already be installed, and relative command/checker paths still resolve from the caller's working directory. `allowed-tools` metadata does not grant execution permissions. See [package and skill guide](docs/14_앱_패키지_및_스킬.md).

### Model-specific skill loading

Configure skill delivery in `app.json` by actual model ID. An exact match wins over `*`; absent configuration keeps the default `catalog` mode. `--model` and ACP model selection choose the matching policy.

```json
{
  "skills": ["source-review", "reporting"],
  "model_configurations": {
    "*": { "skills": { "mode": "catalog" } },
    "my-local-model": {
      "skills": { "mode": "preload", "include_references": true }
    },
    "gpt-6.1-sol": {
      "skills": {
        "mode": "selective",
        "include": [
          { "skill": "source-review", "paths": ["SKILL.md"] },
          { "skill": "reporting", "paths": ["references/output-format.md"] }
        ],
        "max_preload_bytes": 32768
      }
    }
  }
}
```

`catalog` sends names and descriptions only. `preload` adds every declared `SKILL.md`; set `include_references: true` in that skill policy to also preload all `references/` files recursively. The option defaults to false and is valid only in preload mode. `selective` adds exactly the specified files. Preload modes include a relative resource inventory, per-file skill/path provenance and an indication of which files are already included. With `preload` plus `include_references: true`, `dustagent__read_skill` is removed and direct calls are rejected, including reads of non-preloaded scripts/assets. Other loading modes retain skill lookup. Scripts are never executed by loading. Missing, undeclared or invalid selected resources and oversized preload payloads fail before model execution. The 32 KiB default budget (configurable up to 256 KiB) covers the complete serialized skill prompt in preload modes. Catalog mode retains its existing metadata bounds. See [configuration and resume behavior](docs/20_모델별_스킬_로딩.md).

## Context compaction and original transcripts

Dust keeps original messages and tool results in a private append-only JSONL archive, separate from the model-facing conversation. Automatic compaction summarizes older completed exchanges while preserving app instructions, the original/current request, and recent complete tool batches. Full tool outputs are archived before report/context truncation. When compaction is enabled, model-facing tool feedback has a smaller configurable-budget-derived cap and points to the original record; the report retains its existing evidence limits. Each compaction also stores a discovery directory: an unverified topic description, the original record range, literal keywords with record indexes, and previews. A bounded hint stays in context; older entries remain discoverable with `dustagent__history_directory`. Keywords and locations come from actual originals, without an extra model call. The model can inspect its own originals with `dustagent__history_search` and `dustagent__history_read`; these tools accept record indexes and pagination, rather than arbitrary file paths.

```json
{"compaction":{"enabled":true}}
```

Defaults derive from the configured model's context capacity, reserving 10% each (at least 1,024 tokens) for output and tool growth. Codex uses its local `models_cache.json` capacity hint; the official OpenAI endpoint uses a small exact-ID capacity table. Unknown/custom deployments use a conservative 32,768-token fallback; set `context_window_tokens` to their actual deployment limit. Recent complete exchanges are selected by estimated token budget, and the summary allowance scales with that budget. `trigger_tokens`, `keep_recent_messages`, `max_summary_tokens`, and `max_summary_bytes` default to `0` (automatic); explicit overrides still work. The summary token cap defaults to one eighth of the trigger budget. Dust uses provider-reported output tokens when available; `max_summary_bytes` is a separate UTF-8 safety ceiling, not the summary's token budget. Normal turn records and compaction attempts retain provider usage, including input, output, and cached input tokens when the provider returns them. If usage is absent, Dust estimates summary tokens from UTF-8 length; pre-request context sizing also remains an approximate serialized-text estimate. Compaction adds a model call only at the threshold, uses the same time/turn budget, and installs a summary only after it is valid and reduces context. Empty, oversized, or unexpected tool-call summaries may retry twice by default (`max_summary_retries: 0..3`); every retry consumes a normal turn and the same deadline. Each attempt is retained in `compaction_attempts` with its turn, status, response bytes, summary token count and whether it was provider-reported or estimated, provider usage, both limits, elapsed time, and original record index, while `compactions` remains the list of successful compactions. Diagnostics remain readable even if an external runner removes its temporary archive. Set `enabled:false` to disable automatic summaries; original recording remains enabled. Output checkers keep the existing bounded tool-evidence report; full originals remain in the separate archive.

An oversized completed latest tool batch can be summarized in full instead of forcing its retention. Dust keeps the largest recent complete suffix that fits; an explicit recent-message count caps the desired suffix and yields to hard bounds. If the summary request is too large, Dust processes contiguous UTF-8 chunks of the JSON reference source, carrying forward bounded, unverified continuation notes. Request estimates include JSON escaping, preserved instructions/requests, prior notes, retry feedback and output reserve. Every chunk and retry consumes a turn and the same deadline. Originals and complete active batches remain intact until all chunks succeed and the final replacement passes hard bounds. Intermediate valid notes appear as `accepted` attempts; only installed replacements appear in `compactions`. Checkpoints preserve originals and archived intermediate responses, and resume may restart summarization; the temporary chunk cursor is not persisted. This bounds requests without guaranteeing the model preserves every fact in its notes.

Archives live under `~/.dustagent/history` or `DUST_HISTORY_HOME`, with owner-only Unix permissions. Each execution or ACP session has its own archive. Reports expose the archive reference; checkpoints bind it by app/cwd, count, and content hash. Archives are not automatically deleted. History reads return up to 16 KiB per page. Storage limits stop execution instead of silently discarding originals. See [compaction and transcript guide](docs/18_컨텍스트_압축_및_원문_기록.md).

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
| `7` | `cancelled` | Library cancellation; ACP returns a cancelled prompt response without exiting the process |

Without `--json`, only completed runs emit task output on STDOUT. With `--json`, failed runs also emit their report; callers must still inspect the exit code. `--report` saves the report atomically with owner-only permissions on Unix. Reports can include sensitive task and tool data.

When a provider reports usage, JSON output and `--report` include a top-level `token_usage` aggregate across ordinary model turns and compaction responses. It reports input, cached input, uncached input where both counters were returned, output, reasoning, and total tokens; cached input is part of input, and reasoning is part of output. The provider's per-response counters remain under `turns[].provider_usage` and `compaction_attempts[].provider_usage`. Missing provider counters stay unknown, so aggregate values can be partial; Dust does not estimate general request usage. Save and inspect the aggregate with `dust run crawler --report run-report.json "..."` followed by `jq '.token_usage' run-report.json`.

Example:

```json
{
  "token_usage": {
    "responses_with_usage": 3,
    "input_tokens": 8120,
    "cached_input_tokens": 6144,
    "uncached_input_tokens": 1976,
    "responses_with_uncached_input": 3,
    "output_tokens": 870,
    "reasoning_tokens": 320,
    "total_tokens": 8990
  }
}
```

Defaults are ten model turns, five minutes overall, and thirty seconds per tool call. App fields `max_turns`, `timeout_ms`, and `tool_timeout_ms` set defaults; CLI options override them. Timeout values support 1–86400000 milliseconds. The CLI's overall deadline includes MCP startup, automatic experience review, tool discovery, model requests, and tool calls. Bounded cancellation cleanup can add up to one second and shutdown has a separate five-second budget. Synchronous input/report file I/O is outside the async deadline.

Timed-out MCP clients are discarded to avoid consuming a late response as a later request's result. A timed-out remote operation can still have executed; the runtime does not automatically replay it. Recovered tool failures remain visible in the report and prevent admission as successful experience examples. Output-check failures also prevent admission.

Library callers use `execute_report()` or `execute_report_with_experience()` for structured results. Existing `execute()` methods return errors on incomplete execution. Library execution budgets cover discovery and execution; initialization is separately bounded. `completed` describes the execution contract, not exhaustive crawl coverage or factual correctness.

Reports are written at termination. Each fresh CLI `run` automatically keeps a checkpoint under the system temporary directory, for example `/tmp/dust-run-XXXXXX/state.json` (`TMPDIR` can change the temporary root). The private directory and checkpoint survive process exit. STDERR announces the path, and JSON reports expose `checkpoint_path`; plain task output on STDOUT is unchanged. The path is an assigned destination: startup errors can occur before a checkpoint is written. Temporary-system cleanup or manual deletion removes it, so use an explicit path for longer retention. This adds checkpoint file writes during execution, not extra model calls. Library and ACP sessions do not automatically create these temporary checkpoints.

Use the reported path with `--resume`, or choose a destination explicitly:

```bash
dust run crawler --max-turns 1 --report run-report.json "https://example.com"
# After a resumable stop, read checkpoint_path from run-report.json.
dust run crawler --resume /tmp/dust-run-XXXXXX/state.json --max-turns 10 --json
```

The temporary path above is illustrative; use the actual reported path. A completed checkpoint requires a nonempty follow-up prompt. Unsafe interrupted runs remain non-resumable.

```bash
dust run crawler --checkpoint state/crawl.json --max-turns 10 "https://example.com"
dust run crawler --resume state/crawl.json --max-turns 10 --json
```

Resume preserves the original input and requires the same manifest and working directory. To correct a completed response, append a follow-up prompt: `dust run crawler --resume /tmp/dust-run-XXXXXX/state.json --max-turns 5 "Return the previous result in the required JSON format."`. The text becomes a new user message after the saved conversation; checker/experience input remains the original task. Whitespace-only follow-ups are rejected. Each resumed invocation grants an additional turn/time budget. Confirmed tool results remain in the conversation; they are not automatically replayed. MCP processes restart, so browser sessions and remote state are not restored. The model can still request new operations.

Checkpoints are atomically saved with filesystem synchronization, owner-only Unix permissions, and a 32 MiB size limit. The CLI holds an exclusive sidecar lock released automatically when its process exits. A safe boundary after a tool batch or before a model request can resume; an interrupted tool call, unknown transport outcome, or interrupted output checker cannot. Completed checkpoints may continue only with a nonempty follow-up prompt. There is no force-resume option or automatic continuation. Keep `--report` and checkpoint paths distinct. See [checkpoint behavior](docs/13_체크포인트_및_재개.md) for details. Building from source requires Rust 1.89 or newer.

## Completion feedback, recovery, and working state

Apps can opt into feedback validation without prescribing an exploration sequence. When a final response candidate is checked, the app's checker decides `complete`, `continue`, or `blocked`. `continue` adds the checker reason to the conversation and gives the model another turn within the same budget. No additional supervisor model is called. Existing `passed`/`reason` checkers remain terminal checks by default.

```json
{
  "working_state": true,
  "provider_retry": {"max_retries": 2, "base_delay_ms": 250},
  "validation": {
    "mode": "feedback",
    "command": "python3",
    "args": ["${DUST_APP_ROOT}/checkers/coverage.py"],
    "timeout_ms": 5000
  }
}
```

Feedback checkers receive `input`, `output`, recorded `tool_calls`, and `state`; they return a JSON verdict such as `{"decision":"continue","reason":"Body evidence is missing"}`. Tool records include status, call IDs, actual retained outputs, and truncation flags. The checker determines what evidence establishes completion. Working-state claims alone do not prove observation. Checker errors and malformed verdicts stop execution. `${DUST_APP_ROOT}` in the checker command/arguments resolves to the current package root; ordinary relative paths still use the caller's working directory.

Transient execution-loop provider failures are retried by default at most twice, with 250 ms then 500 ms waits inside the existing time budget. Authentication/request errors and invalid model responses are not retried. Set `max_retries` to `0` to disable retries. Retries request the same conversation before any new tool dispatch; they never replay completed tools or unknown tool outcomes. They may incur additional inference cost. This policy applies to the execution loop, not standalone `new` or experience review calls.

With `working_state: true`, the model receives `dustagent__state_get`, `dustagent__state_put`, and `dustagent__state_list`. JSON memos are local to one execution, retained in reports/checkpoints, and restored on resume. New executions start empty. Limits are 256 entries, 16 KiB per JSON value, 256 KiB total serialized state, and 20 keys per list page. The runtime stores memos without deciding exploration order.

Reports add `validation_history`, `provider_retries`, and optional `working_state`. A continued validation can produce a resumable `Ready` checkpoint; an interrupted checker remains non-resumable. The [coverage reader example](apps/coverage-reader/README.md) includes a checker for supplied counts; it does not verify real browser coverage. See [execution recovery contracts](docs/15_완료_피드백_및_실행_복구.md) for payloads, limits, and compatibility.

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
| `dustagent__history_directory` | Optional `query`, `cursor`, `limit` | Discover archived topics, literal keywords and original record locations |
| `dustagent__history_search` | `query`, optional `cursor`, `limit` | Search this execution/session's original messages |
| `dustagent__history_read` | `index`, optional `offset`, `limit` | Read an original message with byte pagination |

Scoped declarations keep unrelated external tools out of a task. They are not a sandbox: declared tools and checker commands retain their own capabilities, and native environment access remains available.

### Optional stdio MCP chroot (Linux)

An individual MCP server can run in a prepared filesystem root:

```json
{
  "mcp_servers": {
    "files": {
      "command": "/usr/bin/node",
      "args": ["/mcp/server.js", "/workspace"],
      "chroot": { "root": "/srv/dust-jails/files", "user": "1000:1000" },
      "env": { "HOME": "/tmp", "PATH": "/usr/bin:/bin" }
    }
  }
}
```

`root` is a host path; command and argument paths are inside the jail. Supply the executable, libraries, MCP code and dependencies (including `node_modules`) there beforehand. Dust does not copy files, mount paths, install packages or elevate privileges. GNU `chroot` must be available and the launcher must have permission to use it. The server runs as the specified non-root UID/GID, starts in `/`, and receives only its explicit `env`. A configured jail that cannot start or initialize stops app startup; there is no host fallback.

This restricts the configured server's filesystem view. It does not isolate networking or processes, jail Dust's native tools/checkers, or constrain other servers without `chroot`. See [setup and boundaries](docs/19_MCP_chroot.md).

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
