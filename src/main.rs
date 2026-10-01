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

    /// Direct in-place code patcher
    Patch(PatchArgs),

    /// Scaffold a new application manifest using LLM
    New(NewArgs),

    /// Automatically review recorded examples and improve future runs
    Learn(LearnArgs),
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
    /// Agent name (saved as apps/<name>.json)
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
        Commands::Learn(args) => match handle_learn(args).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("Error: {err}");
                ExitCode::FAILURE
            }
        },
        Commands::Run(args) => match handle_run(args).await {
            Ok(_) => ExitCode::SUCCESS,
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

async fn handle_new(args: NewArgs) -> anyhow::Result<()> {
    let current_dir = std::env::current_dir()?;
    let apps_dir = current_dir.join("apps");

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

    // Overwrite `name` field with the provided name
    if let Some(obj) = json_val.as_object_mut() {
        obj.insert(
            "name".to_string(),
            serde_json::Value::String(args.name.clone()),
        );
    }

    let pretty = serde_json::to_string_pretty(&json_val)?;

    if args.stdout {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        writeln!(handle, "{pretty}")?;
        handle.flush()?;
    } else {
        std::fs::create_dir_all(&apps_dir)?;
        let out_path = apps_dir.join(format!("{}.json", args.name));
        std::fs::write(&out_path, format!("{pretty}\n"))?;
        eprintln!("✓ Created {}", out_path.display());
    }

    Ok(())
}

async fn handle_run(args: RunArgs) -> anyhow::Result<()> {
    let current_dir = std::env::current_dir()?;
    let manifest_path = resolve_manifest_path(&args.app, &current_dir)?;

    let user_input = if !args.input.is_empty() {
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

    let manifest = AppManifest::from_file(&manifest_path)?;
    let model = args
        .model
        .or_else(|| manifest.default_model.clone())
        .unwrap_or_else(|| "gpt-4o-mini".to_string());

    let provider = OpenAiProvider::new(model.clone())?;
    let mut core = DustCore::load_manifest(&manifest_path, provider)?;
    core.init_scoped_mcp().await?;

    let result = if args.experience || args.experience_dir.is_some() {
        let store = ExperienceStore::new(experience_directory(args.experience_dir)?);
        match dustagent::application::reinforcement::reinforce_with_research(
            &manifest,
            &store,
            OpenAiProvider::new(model)?,
        )
        .await
        {
            Ok(report) => eprintln!(
                "[dustagent] automatic review: {}",
                serde_json::to_string(&report)?
            ),
            Err(err) => eprintln!("[dustagent] automatic review failed: {err}"),
        }
        core.execute_with_experience(&user_input, &store).await
    } else {
        core.execute(&user_input).await
    };
    core.shutdown().await?;
    let output = result?;

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    writeln!(handle, "{output}")?;
    handle.flush()?;

    Ok(())
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

    let result = core.execute(&user_prompt).await?;
    core.shutdown().await?;

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
    let manifest =
        AppManifest::from_file(resolve_manifest_path(&args.app, &std::env::current_dir()?)?)?;
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
