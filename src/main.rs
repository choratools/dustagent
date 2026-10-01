use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use dustagent::adapters::fuzzy_patch::FuzzyPatcher;
use dustagent::adapters::openai::OpenAiProvider;
use dustagent::application::core::DustCore;
use dustagent::domain::manifest::{AppManifest, resolve_manifest_path};
use dustagent::domain::patch::{LineRange, extract_blocks};
use dustagent::ports::patcher::CodePatcher;

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
}

#[derive(Args, Debug)]
struct RunArgs {
    /// Application name in apps/ (e.g. crawler, patcher) or path to manifest
    app: String,

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
    }
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

    let provider = OpenAiProvider::new(model)?;
    let mut core = DustCore::load_manifest(&manifest_path, provider)?;
    core.init_scoped_mcp().await?;

    let output = core.execute(&user_input).await?;
    core.shutdown().await?;

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
