use crate::Args;
use anyhow::Result;
use std::process::ExitCode;
use tracing::info;

/// Tier 1: Landlock LSM sandbox.
///
/// Applies Landlock ABI v2 rules to restrict filesystem and network access.
/// Read-only: /usr, /lib, /opt, wine prefix.
/// Read-write: binary dir, /tmp/win-runtime-$$.
/// Network: port-restricted via Landlock.
pub fn run(args: &Args) -> Result<ExitCode> {
    info!("Tier 1: Landlock sandbox for {}", args.exe);

    // TODO: Create Landlock ruleset with ABI v2
    // TODO: Add read-only rules for system dirs
    // TODO: Add read-write rules for binary dir and temp dir
    // TODO: Add network port restrictions
    // TODO: Enforce ruleset via landlock_restrict_self()
    // TODO: Exec wine with sanitized environment

    todo!("Tier 1 (Landlock) not yet implemented")
}
