use crate::Args;
use anyhow::Result;
use std::process::ExitCode;
use tracing::info;

/// Tier 3: OverlayFS + RAM ephemeral sandbox.
///
/// Creates an OverlayFS mount with:
/// - lowerdir = base wine prefix (read-only)
/// - upperdir = /dev/shm/win-run-{pid}/upper (RAM-backed)
/// - workdir = /dev/shm/win-run-{pid}/work
/// - merged = /dev/shm/win-run-{pid}/merged (WINEPREFIX)
///
/// All changes are lost when the process exits. Cleanup via self-pipe trick.
pub fn run(args: &Args) -> Result<ExitCode> {
    info!("Tier 3: OverlayFS ephemeral sandbox for {}", args.exe);

    // TODO: Create /dev/shm/win-run-{pid}/ directory structure
    // TODO: Mount OverlayFS with lowerdir/upperdir/workdir
    // TODO: Set up self-pipe trick for SIGCHLD cleanup
    // TODO: Set up atexit + panic hook for overlay unmount
    // TODO: Exec wine with WINEPREFIX pointing to merged overlay
    // TODO: Unmount overlay and clean up on exit

    todo!("Tier 3 (OverlayFS) not yet implemented")
}
