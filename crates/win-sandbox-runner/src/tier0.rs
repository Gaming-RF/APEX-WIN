use crate::Args;
use anyhow::Result;
use std::process::{Command, ExitCode};
use tracing::info;

/// Tier 0: Direct wine execution with no sandboxing.
///
/// Sets up a sanitized environment (allowlisted vars only, no secrets)
/// and exec's wine directly.
pub fn run(args: &Args) -> Result<ExitCode> {
    info!("Tier 0: Direct wine execution for {}", args.exe);

    let config = crate::config::load_config(None);
    let sandbox_env = crate::env_sanitize::build_sandbox_env(&config)?;

    let mut cmd = Command::new("wine");
    cmd.arg(&args.exe);
    cmd.args(&args.args);

    // Clear inherited environment and apply only sanitized vars
    cmd.env_clear();
    for (key, val) in &sandbox_env {
        cmd.env(key, val);
    }

    let status = cmd.status()?;
    if status.success() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
    }
}
