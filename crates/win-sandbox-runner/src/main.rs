#[allow(dead_code)]
mod amd;
#[allow(dead_code)]
mod audio;
#[allow(dead_code)]
mod cleanup;
#[allow(dead_code)]
mod config;
mod dispatch;
#[allow(dead_code)]
mod display;
#[allow(dead_code)]
mod env_sanitize;
mod hasher;
#[allow(dead_code)]
mod nvidia;
mod rules;
mod tier0;
#[allow(dead_code)]
mod tier1;
#[allow(dead_code)]
mod tier2;
#[allow(dead_code)]
mod tier3;

use anyhow::Result;
use clap::Parser;
use std::process::ExitCode;
use tracing::info;

/// win-sandbox-runner: Transparent tiered sandbox for Windows executables via Wine.
#[derive(Parser, Debug)]
#[command(name = "win-sandbox-runner", version, about)]
struct Args {
    /// Path to the Windows executable (.exe).
    #[arg(short, long)]
    exe: String,

    /// Force a specific tier (0–3), overriding rules.json.
    #[arg(short, long)]
    tier: Option<String>,

    /// Path to rules.json (default: auto-discover).
    #[arg(short, long)]
    rules: Option<String>,

    /// Increase logging verbosity.
    #[arg(short, long)]
    verbose: bool,

    /// Disable GUI prompts (headless mode).
    #[arg(long)]
    no_gui: bool,

    /// Show what would be done without executing.
    #[arg(long)]
    dry_run: bool,

    /// Additional arguments passed to the Windows executable.
    #[arg(trailing_var_arg = true)]
    args: Vec<String>,
}

fn main() -> ExitCode {
    let args = Args::parse();

    // Initialize logging.
    let filter = if args.verbose {
        tracing_subscriber::EnvFilter::new("debug")
    } else {
        tracing_subscriber::EnvFilter::new("info")
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // Recursion guard: if WIN_SANDBOX_ACTIVE is already set, pass through to wine.
    if std::env::var("WIN_SANDBOX_ACTIVE").is_ok() {
        info!("Recursion guard active, passing through to wine");
        return dispatch::passthrough_to_wine(&args.exe, &args.args);
    }

    match run(&args) {
        Ok(code) => code,
        Err(e) => {
            tracing::error!("Fatal: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<ExitCode> {
    // Hash the binary
    let hash = hasher::hash_file(&args.exe)?;
    info!("Binary hash: {hash}");

    // Load rules
    let rules_path = config::find_rules_path(args.rules.as_deref());
    let rules = rules::load_rules(rules_path.as_deref())?;

    // Dispatch to the appropriate tier
    dispatch::execute(args, &hash, &rules)
}
