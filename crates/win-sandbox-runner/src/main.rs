mod amd;
mod appdb;
mod audio;
mod cleanup;
mod config;
mod dispatch;
mod display;
mod env_sanitize;
mod hasher;
mod net;
mod netopt;
mod nvidia;
mod prefix;
mod rules;
mod tier0;
mod tier1;
mod tier2;
mod tier3;
mod wizard;

use anyhow::Result;
use clap::Parser;
use std::process::ExitCode;
use tracing::{debug, info, warn};

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

    /// Mark this app as trusted (no sandboxing) and save to rules.json.
    /// On next launch, the app will run without sandboxing automatically.
    #[arg(long)]
    trust: bool,

    /// Apply network optimizations for gaming (BBR, SQM, socket buffers, DSCP).
    /// Requires root for tc/iptables/sysctl changes.
    #[arg(long)]
    optimize_net: bool,

    /// Remove network optimizations previously applied by --optimize-net.
    #[arg(long)]
    cleanup_net: bool,
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
    // --cleanup-net: remove network optimizations and exit
    if args.cleanup_net {
        let config = netopt::load_config(None);
        netopt::cleanup(&config)?;
        return Ok(ExitCode::SUCCESS);
    }

    // --optimize-net: apply network optimizations and exit
    if args.optimize_net {
        let config = netopt::load_config(None);
        let result = netopt::optimize(&config)?;
        println!("{result}");
        return Ok(ExitCode::SUCCESS);
    }

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

    // Load the built-in app database
    let app_db = appdb::AppDatabase::load_embedded();

    // --trust flag: save this app as trusted in rules.json
    if args.trust {
        save_trusted_rule(&args.exe, &hash)?;
    }

    // Auto-apply network optimization for game profiles
    if let Some((profile, _)) = app_db.lookup_by_name(&args.exe) {
        if profile.network && profile.gpu {
            // This looks like a game — auto-optimize network
            let net_config = netopt::load_config(None);
            match netopt::optimize(&net_config) {
                Ok(result) => {
                    if result.bbr_applied || result.sqm_applied {
                        info!("Network optimized for gaming");
                        println!("{result}");
                    }
                }
                Err(e) => {
                    debug!("Auto network optimization skipped: {e}");
                }
            }
        }
    }

    // Dispatch to the appropriate tier
    dispatch::execute(args, &hash, &rules, &app_db)
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

/// Save a trusted rule for the given exe into rules.json.
/// If the hash already exists, updates it. Otherwise, adds a new entry.
fn save_trusted_rule(exe_path: &str, hash: &str) -> Result<()> {
    use win_sandbox_common::rules_schema::{RuleEntry, RulesFile};

    let name = std::path::Path::new(exe_path)
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let rules_path = config::find_rules_path(None)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            std::path::PathBuf::from(format!("{home}/.config/win-sandbox/rules.json"))
        });

    // Load existing rules or create defaults
    let mut rules = rules::load_rules(Some(&rules_path)).unwrap_or(RulesFile {
        version: 1,
        entries: vec![],
        defaults: Default::default(),
    });

    // Check if entry already exists
    let existing = rules.entries.iter().position(|e| e.hash == hash);
    let entry = RuleEntry {
        hash: hash.to_string(),
        name: name.clone(),
        tier: win_sandbox_common::tier::Tier::Tier0,
        allowed_paths: vec![],
        network: true,
        gpu: true,
        trusted: true,
        dxvk: false,
        winetricks: vec![],
        env: std::collections::HashMap::new(),
        wine_variant: "system".into(),
    };

    if let Some(pos) = existing {
        rules.entries[pos] = entry;
        info!("Updated trusted rule for '{name}' in rules.json");
    } else {
        rules.entries.push(entry);
        info!("Saved trusted rule for '{name}' to rules.json");
    }

    // Ensure parent directory exists
    if let Some(parent) = rules_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_string_pretty(&rules)?;
    std::fs::write(&rules_path, json)?;
    info!("Rules saved to {}", rules_path.display());

    Ok(())
}
