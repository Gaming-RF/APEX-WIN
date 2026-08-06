use crate::Args;
use anyhow::Result;
use std::process::{Command, ExitCode};
use tracing::info;

/// Tier 0: Direct wine execution with no sandboxing.
///
/// Sets up the environment (WINEPREFIX, display, audio) and exec's wine directly.
pub fn run(args: &Args) -> Result<ExitCode> {
    info!("Tier 0: Direct wine execution for {}", args.exe);

    let config = crate::config::load_config(None);
    let env = crate::env_sanitize::build_sandbox_env(&config)?;

    let mut cmd = Command::new("wine");
    cmd.arg(&args.exe);
    cmd.args(&args.args);

    // Apply sanitized environment
    for (key, value) in &env {
        cmd.env(key, value);
    }

    // Set recursion guard
    cmd.env("WIN_SANDBOX_ACTIVE", "1");

    let status = cmd.status()?;
    if status.success() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
    }
}
