mod amd;
mod audio;
mod cleanup;
mod config;
mod dispatch;
mod display;
mod env_sanitize;
mod hasher;
mod net;
mod nvidia;
mod rules;
mod tier0;
mod tier1;
mod tier2;
mod tier3;

use anyhow::Result;
use clap::Parser;
use std::process::ExitCode;
use tracing::{info, warn};

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

    /// Allow gamepad/controller access in sandbox (Tier 2/3).
    /// Bind-mounts /dev/input/event* for gamepad devices.
    #[arg(long)]
    gamepad: bool,

    /// Use nested X11 (Xephyr) for display isolation (default for Tier 2/3).
    #[arg(long)]
    nested_x11: bool,

    /// Use Xvfb virtual framebuffer (headless, no visible window).
    #[arg(long)]
    xvfb: bool,

    /// Use host X11 directly (DANGEROUS: enables keylogger attacks).
    #[arg(long)]
    host_x11: bool,

    /// Use Wayland with Wine Wayland driver.
    #[arg(long)]
    wayland: bool,

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
    // Check Wine version before proceeding
    if let Err(e) = check_wine_version() {
        warn!("Wine version check failed: {e}");
    }

    // Hash the binary
    let hash = hasher::hash_file(&args.exe)?;
    info!("Binary hash: {hash}");

    // Load rules
    let rules_path = config::find_rules_path(args.rules.as_deref());
    let rules = rules::load_rules(rules_path.as_deref())?;

    // Dispatch to the appropriate tier
    dispatch::execute(args, &hash, &rules)
}

/// Check Wine version and warn if < 9.0.
/// Wine 9.0+ adds NTsync, Wayland driver, and important fixes.
fn check_wine_version() -> Result<()> {
    use std::process::Command;
    let output = Command::new("wine").arg("--version").output()?;
    let version_str = String::from_utf8_lossy(&output.stdout);
    let version_str = version_str.trim();
    info!("Wine version: {version_str}");

    // Parse "wine-X.Y.Z" or "wine-X.Y"
    if let Some(ver) = version_str.strip_prefix("wine-") {
        let major: u32 = ver.split('.').next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if major < 9 {
            warn!("Wine {ver} detected — Wine 9.0+ recommended");
            warn!("Older Wine may lack NTsync, Wayland driver, and important fixes");
        }
    }

    Ok(())
}
