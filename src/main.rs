use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use dustagent::adapters::fuzzy_patch::FuzzyPatcher;
use dustagent::adapters::openai::OpenAiProvider;
use dustagent::application::ExperienceStore;
use dustagent::application::core::DustCore;
use dustagent::domain::manifest::{AppManifest, resolve_manifest_path};
use dustagent::domain::patch::{LineRange, extract_blocks};
use dustagent::ports::llm::{ChatMessage, LlmProvider};
use dustagent::ports::patcher::CodePatcher;
use dustagent::{ExecutionReport, StopReason};

/// Built-in scaffold system prompt used when `apps/scaffold.json` is absent.
const SCAFFOLD_SYSTEM_PROMPT: &str = r#"You are DustAgent Scaffold. Generate a DustAgent application manifest.

Output ONLY a raw JSON object (no markdown fences, no explanation, no comments):
{
  "$schema": "dustagent/app-v1",
  "name": "placeholder",
  "description": "<one sentence>",
  "default_model": "gpt-4o-mini",
  "system_prompt": "<focused system prompt — zero chatter, pure output only>",
  "mcp_servers": {},
  "max_turns": 5,
  "output_format": "text"
}

Rules:
- system_prompt must laser-focus on the single task. No pleasantries. No explanations in output.
- For web fetching needs: add mcp_servers.fetch = {"command": "uvx", "args": ["mcp-server-fetch"]}
- For browser automation: add mcp_servers.playwright = {"command": "npx", "args": ["-y", "@playwright/mcp", "--headless", "--browser", "chromium"]}
- Keep max_turns minimal (3-5). Browser/multi-step tasks: 15-20.
- output_format: "raw_json" for data extraction, "text" for analysis/writing
- Output ONLY the JSON object. Nothing else."#;

#[derive(Parser, Debug)]
#[command(
    name = "dust",
    version,
    about = "DustAgent: Ultra-lightweight Unix-style Agent as an Application (AaaA) runtime"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run a specialized application manifest
    Run(RunArgs),

    /// Pack an application directory as a portable .dustpkg archive
    Pack(PackArgs),

    /// Install a local package without executing its contents
    Install(InstallArgs),

    /// Direct in-place code patcher
    Patch(PatchArgs),

    /// Scaffold a new application manifest using LLM
    New(NewArgs),

    /// Automatically review recorded examples and improve future runs
    Learn(LearnArgs),
}

#[derive(Args, Debug)]
struct PackArgs {
    source: PathBuf,
    #[arg(short, long)]
    output: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct InstallArgs {
    source: PathBuf,
    /// Override the installed app store (~/.dustagent/packages)
    #[arg(long)]
    store: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct LearnArgs {
    app: String,
    /// Inspect records and review reasons without calling the model
    #[arg(long)]
    list: bool,
    #[arg(short, long)]
    model: Option<String>,
    #[arg(long)]
    experience_dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct RunArgs {
    /// Application name in apps/ (e.g. crawler, patcher) or path to manifest
    app: String,

    /// Record runs and automatically review/reuse useful past examples
    #[arg(long)]
    experience: bool,

    /// Override experience directory (implies --experience)
    #[arg(long)]
    experience_dir: Option<PathBuf>,

    /// Emit a structured execution report, including partial results
    #[arg(long)]
    json: bool,

    /// Save an execution report to this JSON file
    #[arg(long)]
    report: Option<PathBuf>,

    /// Persist resumable conversation checkpoints during execution
    #[arg(long, conflicts_with = "resume")]
    checkpoint: Option<PathBuf>,

    /// Continue a safe checkpoint using its original input and conversation
    #[arg(long, conflicts_with = "checkpoint")]
    resume: Option<PathBuf>,

    /// Override overall budget (including startup and automatic review)
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=86_400_000))]
    timeout_ms: Option<u64>,

    /// Override per-tool timeout
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=86_400_000))]
    tool_timeout_ms: Option<u64>,

    /// Override maximum model turns
    #[arg(long)]
    max_turns: Option<usize>,

    /// Input query, URL, or prompt (reads from STDIN if omitted)
    #[arg(trailing_var_arg = true)]
    input: Vec<String>,

    /// Override default LLM model
    #[arg(short, long)]
    model: Option<String>,
}

#[derive(Args, Debug)]
struct PatchArgs {
    /// Target file path to edit
    #[arg(short, long)]
    file: PathBuf,

    /// Line range to edit (e.g. 15:30 or 42)
    #[arg(short, long)]
    range: Option<String>,

    /// Override default LLM model
    #[arg(short, long)]
    model: Option<String>,

    /// Print diff blocks without modifying file
    #[arg(long)]
    dry_run: bool,

    /// Edit instruction for the code
    instruction: String,
}

#[derive(Args, Debug)]
struct NewArgs {
    /// Agent name (saved as apps/<name>/app.json)
    name: String,

    /// Natural-language description of the agent's purpose
    description: String,

    /// Override default LLM model
    #[arg(short, long)]
    model: Option<String>,

    /// Print generated manifest to stdout instead of saving to file
    #[arg(long)]
    stdout: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    // Tracing logs strictly to stderr so stdout remains clean for Unix pipelines
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Pack(args) => artifact_exit(dustagent::application::package::pack(
            &args.source,
            args.output.as_deref(),
        )),
        Commands::Install(args) => {
            let result = args
                .store
                .map(Ok)
                .unwrap_or_else(dustagent::application::package::default_store)
                .and_then(|store| dustagent::application::package::install(&args.source, &store));
            artifact_exit(result)
        }
        Commands::Learn(args) => match handle_learn(args).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("Error: {err}");
                ExitCode::FAILURE
            }
        },
        Commands::Run(args) => match handle_run(args).await {
            Ok(code) => ExitCode::from(code),
            Err(err) => {
                eprintln!("Error: {err}");
                ExitCode::FAILURE
            }
        },
        Commands::Patch(args) => match handle_patch(args).await {
            Ok(_) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("Error: {err}");
                ExitCode::FAILURE
            }
        },
        Commands::New(args) => match handle_new(args).await {
            Ok(_) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("Error: {err}");
                ExitCode::FAILURE
            }
        },
    }
}

fn artifact_exit(result: dustagent::Result<PathBuf>) -> ExitCode {
    match result {
        Ok(path) => {
            println!("{}", path.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn handle_new(args: NewArgs) -> anyhow::Result<()> {
    let current_dir = std::env::current_dir()?;
    let apps_dir = current_dir.join("apps");
    if args.name.is_empty()
        || args.name.len() > 64
        || !args
            .name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
        || args.name.starts_with('-')
        || args.name.ends_with('-')
        || args.name.contains("--")
    {
        anyhow::bail!("App name must be a lowercase slug, at most 64 characters");
    }
    let package_dir = apps_dir.join(&args.name);
    if !args.stdout && package_dir.exists() {
        anyhow::bail!("Package already exists: {}", package_dir.display());
    }

    // Determine system prompt: prefer apps/scaffold.json if it exists
    let system_prompt = {
        let scaffold_path = apps_dir.join("scaffold.json");
        if scaffold_path.is_file() {
            let manifest = AppManifest::from_file(&scaffold_path)?;
            manifest
                .system_prompt
                .unwrap_or_else(|| SCAFFOLD_SYSTEM_PROMPT.to_string())
        } else {
            SCAFFOLD_SYSTEM_PROMPT.to_string()
        }
    };

    let model = args
        .model
        .clone()
        .unwrap_or_else(|| "gpt-4o-mini".to_string());

    let provider = OpenAiProvider::new(&model)?;

    let messages = vec![
        ChatMessage::system(&system_prompt),
        ChatMessage::user(format!(
            "Generate a DustAgent manifest for: {}",
            args.description
        )),
    ];

    let response = provider.chat(&messages, None).await?;
    let raw = response
        .content
        .ok_or_else(|| anyhow::anyhow!("LLM returned no content"))?;

    // Parse JSON: try full text first, then extract ```json block
    let mut json_val: serde_json::Value = serde_json::from_str(raw.trim()).or_else(|_| {
        // Try to extract a ```json ... ``` fenced block
        let start = raw
            .find("```json")
            .map(|i| i + 7)
            .or_else(|| raw.find("```").map(|i| i + 3));
        let end = start.and_then(|s| raw[s..].find("```").map(|e| s + e));
        match (start, end) {
            (Some(s), Some(e)) => serde_json::from_str(raw[s..e].trim()),
            _ => serde_json::from_str(raw.trim()),
        }
    })?;

    // Keep generated output a valid manifest before creating the package directory.
    if !json_val.is_object() {
        anyhow::bail!("Scaffold must return a manifest JSON object");
    }
    if let Some(obj) = json_val.as_object_mut() {
        obj.insert(
            "package".into(),
            serde_json::json!({"name":args.name,"version":"0.1.0","dust_version":">=0.1.0"}),
        );
        obj.insert("skills".into(), serde_json::json!([]));
        obj.insert(
            "name".to_string(),
            serde_json::Value::String(args.name.clone()),
        );
    }

    let pretty = serde_json::to_string_pretty(&json_val)?;
    AppManifest::from_json_str(&pretty)?;

    if args.stdout {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        writeln!(handle, "{pretty}")?;
        handle.flush()?;
    } else {
        std::fs::create_dir_all(&apps_dir)?;
        std::fs::create_dir(&package_dir)?;
        std::fs::create_dir(package_dir.join("skills"))?;
        let out_path = package_dir.join("app.json");
        std::fs::write(&out_path, format!("{pretty}\n"))?;
        eprintln!("✓ Created {}", out_path.display());
    }

    Ok(())
}

async fn handle_run(args: RunArgs) -> anyhow::Result<u8> {
    let current_dir = std::env::current_dir()?;
    let loaded_app = dustagent::application::package::load(&args.app, &current_dir)?;
    let manifest = loaded_app.manifest.clone();
    let catalog =
        dustagent::application::skills::SkillCatalog::load(&loaded_app.root, &manifest.skills)?;
    let resource_hash = dustagent::application::core::resource_hash(
        loaded_app.digest.as_deref(),
        &catalog,
        !manifest.skills.is_empty(),
    );
    let checkpoint_path = args.resume.as_ref().or(args.checkpoint.as_ref());
    let checkpoint_guard = checkpoint_path
        .map(|path| acquire_checkpoint_lease(path))
        .transpose()?;
    let checkpoint_path = checkpoint_guard.as_ref().map(|(path, _lease)| path.clone());
    if let Some(checkpoint_path) = &checkpoint_path {
        if args.checkpoint.is_some() && checkpoint_path.exists() {
            anyhow::bail!("Checkpoint already exists; use --resume or a new path");
        }
        if let Some(report_path) = &args.report {
            let normalized = normalize_artifact_path(report_path)?;
            if normalized == *checkpoint_path {
                anyhow::bail!("--report must not overwrite the checkpoint");
            }
        }
    }
    let resumed = if args.resume.is_some() {
        if !args.input.is_empty() {
            anyhow::bail!(
                "--resume uses the checkpoint's original input; do not provide new input"
            );
        }
        let checkpoint =
            dustagent::application::checkpoint::load(checkpoint_path.as_ref().unwrap())?;
        checkpoint.validate_for(&manifest)?;
        checkpoint.validate_resources(resource_hash.as_deref())?;
        checkpoint.ensure_resumable()?;
        Some(checkpoint)
    } else {
        None
    };
    let user_input = if let Some(checkpoint) = &resumed {
        checkpoint.user_input.clone()
    } else if !args.input.is_empty() {
        args.input.join(" ")
    } else if !std::io::stdin().is_terminal() {
        let mut buffer = String::new();
        std::io::stdin().read_to_string(&mut buffer)?;
        buffer.trim().to_string()
    } else {
        anyhow::bail!("No input provided via argument or STDIN.");
    };

    if user_input.is_empty() {
        anyhow::bail!("Input was empty.");
    }

    let model = args
        .model
        .or_else(|| manifest.default_model.clone())
        .unwrap_or_else(|| "gpt-4o-mini".to_string());

    let timeout_ms = args.timeout_ms.or(manifest.timeout_ms).unwrap_or(300_000);
    let tool_timeout_ms = args
        .tool_timeout_ms
        .or(manifest.tool_timeout_ms)
        .unwrap_or(30_000);
    if !(1..=86_400_000).contains(&timeout_ms) || !(1..=86_400_000).contains(&tool_timeout_ms) {
        anyhow::bail!("Timeouts must be between 1 and 86400000 milliseconds");
    }
    let provider = OpenAiProvider::new(model.clone())?;
    let mut core = DustCore::new(manifest.clone(), provider)
        .with_timeouts(timeout_ms, tool_timeout_ms)
        .with_app_resources(&loaded_app.root, loaded_app.digest.as_deref())?;
    if let Some(path) = &checkpoint_path {
        core = core.with_checkpoint(path);
    }
    if let Some(turns) = args.max_turns {
        core = core.with_max_turns(turns);
    }
    let started = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let mut warnings = Vec::new();
    let operation = async {
        match tokio::time::timeout_at(deadline, core.init_scoped_mcp()).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return setup_report(StopReason::ExecutionError, &err.to_string()),
            Err(_) => {
                return setup_report(StopReason::TimeLimit, "Budget exhausted during MCP startup");
            }
        }
        let store = if args.experience || args.experience_dir.is_some() {
            match experience_directory(args.experience_dir.clone()) {
                Ok(path) => Some(ExperienceStore::new(path)),
                Err(err) => return setup_report(StopReason::ExecutionError, &err.to_string()),
            }
        } else {
            None
        };
        if resumed.is_none()
            && let Some(store) = &store
        {
            let reviewer = match OpenAiProvider::new(model) {
                Ok(provider) => provider,
                Err(err) => return setup_report(StopReason::ExecutionError, &err.to_string()),
            };
            match tokio::time::timeout_at(
                deadline,
                dustagent::application::reinforcement::reinforce_with_research(
                    &manifest, store, reviewer,
                ),
            )
            .await
            {
                Ok(Ok(review)) => eprintln!(
                    "[dustagent] automatic review: {}",
                    serde_json::to_string(&review).unwrap_or_default()
                ),
                Ok(Err(err)) => warnings.push(format!("Automatic review failed: {err}")),
                Err(_) => {
                    return setup_report(
                        StopReason::TimeLimit,
                        "Budget exhausted during automatic review",
                    );
                }
            }
        }
        let remaining = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .as_millis() as u64;
        if remaining == 0 {
            return setup_report(StopReason::TimeLimit, "Budget exhausted before execution");
        }
        core.set_timeouts(remaining, tool_timeout_ms);
        if let Some(checkpoint) = &resumed {
            if let Some(store) = &store {
                core.resume_report_with_experience(checkpoint, store).await
            } else {
                core.resume_report(checkpoint).await
            }
        } else if let Some(store) = &store {
            core.execute_report_with_experience(&user_input, store)
                .await
        } else {
            core.execute_report(&user_input).await
        }
    };
    let mut report = operation.await;
    report.warnings.extend(core.take_lifecycle_warnings());
    if let Err(err) = core.shutdown().await {
        report.warnings.push(format!("Shutdown: {err}"));
    }
    report.warnings.extend(warnings);
    let prior_elapsed = resumed
        .as_ref()
        .map_or(0, |checkpoint| checkpoint.report.elapsed_ms);
    report.elapsed_ms =
        prior_elapsed.saturating_add(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
    let report_error = args
        .report
        .as_ref()
        .and_then(|path| write_report(path, &report).err());
    if let Some(error) = &report_error {
        report
            .warnings
            .push(format!("Report file was not saved: {error}"));
    }
    let mut stdout = std::io::stdout().lock();
    if args.json {
        writeln!(stdout, "{}", serde_json::to_string(&report)?)?;
    } else if report.is_complete() && report_error.is_none() {
        writeln!(stdout, "{}", report.output.as_deref().unwrap_or(""))?;
    }
    stdout.flush()?;
    for warning in &report.warnings {
        eprintln!("[dustagent] warning: {warning}");
    }
    if let Some(error) = report_error {
        return Err(error);
    }
    let code = match report.stop_reason {
        StopReason::Completed => 0,
        StopReason::TurnLimit => 2,
        StopReason::TimeLimit | StopReason::ToolTimeout => 3,
        StopReason::EmptyResponse => 4,
        StopReason::ExecutionError => 5,
        StopReason::ValidationFailed => 6,
    };
    if code != 0 {
        eprintln!(
            "[dustagent] stopped: {:?}; turns={}, elapsed_ms={}; {}",
            report.stop_reason,
            report.turns_used,
            report.elapsed_ms,
            report
                .error
                .as_deref()
                .unwrap_or("See --json or --report for retained evidence")
        );
    }
    Ok(code)
}

fn setup_report(reason: StopReason, error: &str) -> ExecutionReport {
    ExecutionReport {
        stop_reason: reason,
        output: None,
        turns_used: 0,
        elapsed_ms: 0,
        tool_calls: Vec::new(),
        turns: Vec::new(),
        error: Some(error.into()),
        warnings: Vec::new(),
        validation: None,
        ..ExecutionReport::default()
    }
}

fn write_report(path: &std::path::Path, report: &ExecutionReport) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".dust-report-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> anyhow::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(report)?)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

async fn handle_patch(args: PatchArgs) -> anyhow::Result<()> {
    if !args.file.exists() {
        anyhow::bail!("Target file not found: {}", args.file.display());
    }

    let current_dir = std::env::current_dir()?;
    let manifest_path = resolve_manifest_path("patcher", &current_dir)?;
    let manifest = AppManifest::from_file(&manifest_path)?;

    let file_content = std::fs::read_to_string(&args.file)?;
    let lines: Vec<&str> = file_content.lines().collect();

    let user_prompt = if let Some(ref range_str) = args.range {
        let range: LineRange = range_str.parse()?;
        let start_line = range.start();
        let end_line = range.end();

        let win_start = start_line.saturating_sub(30).max(1);
        let win_end = (end_line + 30).min(lines.len());

        let mut context_snippet = String::new();
        for i in (win_start - 1)..win_end {
            if i < lines.len() {
                context_snippet.push_str(&format!("{:4} | {}\n", i + 1, lines[i]));
            }
        }

        let mut target_snippet = String::new();
        for i in (start_line - 1)..end_line {
            if i < lines.len() {
                target_snippet.push_str(lines[i]);
                target_snippet.push('\n');
            }
        }

        format!(
            "Target file: {} (Lines {start_line}-{end_line})\n\
             Surrounding context:\n```\n{context_snippet}```\n\n\
             Target snippet to edit:\n```\n{target_snippet}```\n\n\
             Instruction: {}",
            args.file.display(),
            args.instruction
        )
    } else {
        format!(
            "File: {}\n\n```\n{file_content}\n```\n\nInstruction: {}",
            args.file.display(),
            args.instruction
        )
    };

    let model = args
        .model
        .or_else(|| manifest.default_model.clone())
        .unwrap_or_else(|| "gpt-4o-mini".to_string());

    let provider = OpenAiProvider::new(model)?;
    let mut core = DustCore::load_manifest(&manifest_path, provider)?;
    core.init_scoped_mcp().await?;

    let execution = core.execute(&user_prompt).await;
    core.shutdown().await?;
    let result = execution?;

    let blocks = extract_blocks(&result);
    if blocks.is_empty() {
        eprintln!("Error: Model did not produce valid SEARCH/REPLACE blocks.");
        eprintln!("Model output was:\n{result}");
        std::process::exit(3);
    }

    if args.dry_run {
        println!("[Dry Run] Generated {} patch block(s):", blocks.len());
        println!("{result}");
    } else {
        let patcher = FuzzyPatcher::new();
        patcher.apply_to_file(&args.file, &blocks)?;
        eprintln!(
            "Successfully applied {} patch block(s) to {}",
            blocks.len(),
            args.file.display()
        );
    }

    Ok(())
}

fn experience_directory(path: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(path) = path {
        return Ok(path);
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow::anyhow!("HOME missing; use --experience-dir"))?;
    Ok(PathBuf::from(home).join(".dustagent/experiences"))
}

async fn handle_learn(args: LearnArgs) -> anyhow::Result<()> {
    let loaded_app = dustagent::application::package::load(&args.app, &std::env::current_dir()?)?;
    let manifest = loaded_app.manifest.clone();
    let store = ExperienceStore::new(experience_directory(args.experience_dir)?);
    if args.list {
        let records: Vec<_> = store
            .list(&manifest)?
            .into_iter()
            .map(|e| {
                serde_json::json!({
                    "input": e.input, "output": e.output, "completed": e.completed,
                    "selected": e.approved, "reviewed": e.reviewed, "reason": e.review_reason,
                    "timestamp_ms": e.timestamp_ms, "validation": e.validation, "sources": e.sources
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&records)?);
    } else {
        let records = store.list(&manifest)?;
        if records
            .iter()
            .all(|e| e.reviewed || e.approved || !e.completed)
        {
            println!(
                "{}",
                serde_json::json!({"reviewed":0,"selected":0,"rejected":0,"skipped":records.len()})
            );
            return Ok(());
        }
        let model = args
            .model
            .or_else(|| manifest.default_model.clone())
            .unwrap_or_else(|| "gpt-4o-mini".into());
        let report = dustagent::application::reinforcement::reinforce_with_research(
            &manifest,
            &store,
            OpenAiProvider::new(model)?,
        )
        .await?;
        println!("{}", serde_json::to_string(&report)?);
    }
    Ok(())
}

fn normalize_artifact_path(path: &std::path::Path) -> anyhow::Result<PathBuf> {
    if path.exists() {
        return Ok(path.canonicalize()?);
    }
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Artifact path must name a file"))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    Ok(parent.canonicalize()?.join(name))
}

fn acquire_checkpoint_lease(path: &std::path::Path) -> anyhow::Result<(PathBuf, std::fs::File)> {
    let path = normalize_artifact_path(path)?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Checkpoint must name a file"))?;
    let lease_path = path.with_file_name(format!(".{}.lock", name.to_string_lossy()));
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lease = options.open(lease_path)?;
    lease.try_lock().map_err(|error| {
        anyhow::anyhow!("Checkpoint is already in use or locking failed: {error}")
    })?;
    Ok((path, lease))
}
