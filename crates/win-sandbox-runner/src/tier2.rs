use crate::Args;
use anyhow::Result;
use std::process::ExitCode;
use tracing::info;

/// Tier 2: Bubblewrap container.
///
/// Launches wine inside a bubblewrap container with:
/// - New mount, PID, IPC namespaces
/// - tmpfs for /, /home, /tmp
/// - Read-only binds for /usr, /lib, /etc
/// - GPU passthrough (if detected and requested)
/// - Audio socket binding
/// - Display socket binding (nested X11 by default)
pub fn run(args: &Args) -> Result<ExitCode> {
    info!("Tier 2: Bubblewrap container for {}", args.exe);

    // TODO: Detect GPU (nvidia/amd) and build bind-mount args
    // TODO: Detect audio server (pipewire/pulse)
    // TODO: Detect display server (wayland/x11) and set up nested display
    // TODO: Build bwrap command line with all namespace and bind-mount args
    // TODO: Set WINEPREFIX, WIN_SANDBOX_ACTIVE, display, audio env vars
    // TODO: Exec bwrap with --die-with-parent
    // TODO: Handle Nvidia downgrade to Tier 1 if user namespaces fail

    todo!("Tier 2 (Bubblewrap) not yet implemented")
}
