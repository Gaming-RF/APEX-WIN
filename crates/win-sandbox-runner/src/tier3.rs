use crate::Args;
use anyhow::{bail, Result};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};
use tracing::{info, warn};

/// Base directory for overlay work (RAM-backed via /dev/shm).
const OVERLAY_BASE: &str = "/dev/shm/win-run";

/// Tier 3: OverlayFS + RAM ephemeral sandbox.
#[allow(dead_code)]
pub fn run(args: &Args) -> Result<ExitCode> {
    run_with_network(args, false)
}

/// Tier 3: OverlayFS + RAM ephemeral sandbox with explicit network control.
///
/// When `network` is true, starts the TAP bridge and sets up Wine DLL
/// environment for isolated networking through the overlay.
pub fn run_with_network(args: &Args, network: bool) -> Result<ExitCode> {
    let exe = args.exe.as_deref().unwrap();
    info!("Tier 3: OverlayFS ephemeral sandbox for {exe} (network={network})");

    let config = crate::config::load_config(None);
    let pid = std::process::id();
    let dirs = overlay_dirs(pid);

    // Create directory structure in /dev/shm (RAM-backed)
    std::fs::create_dir_all(&dirs.upper)?;
    std::fs::create_dir_all(&dirs.work)?;
    std::fs::create_dir_all(&dirs.merged)?;

    // The lowerdir is the existing wine prefix (or a minimal stub)
    let lower_dir = resolve_lower_dir(&config.wine_prefix, &dirs.base)?;

    // Mount OverlayFS
    let opts = mount_opts(&lower_dir, &dirs.upper, &dirs.work);
    info!("Mounting OverlayFS: lower={lower_dir}, upper={}", dirs.upper);

    let status = Command::new("mount")
        .args(["-t", "overlay", "overlay", "-o", &opts, &dirs.merged])
        .status();

    match status {
        Ok(s) if s.success() => {
            info!("OverlayFS mounted at {}", dirs.merged);
        }
        Ok(s) => {
            bail!("OverlayFS mount failed with status: {s}");
        }
        Err(e) => {
            bail!("Failed to execute mount: {e} (try running with sudo or use --tier 2)");
        }
    }

    // Set up cleanup hooks
    crate::cleanup::install_cleanup_hooks(dirs.merged.clone());
    let _pipe_fd = crate::cleanup::init_cleanup_pipe()?;
    crate::cleanup::install_sigchld_handler()?;

    // Resolve display mode and launch nested display server if needed
    let display_mode = crate::display::resolve_display_mode(
        args.nested_x11,
        args.xvfb,
        args.host_x11,
        args.wayland,
        3, // Tier 3
    );
    crate::display::warn_display_security(&display_mode);

    // Launch Xephyr/Xvfb if needed (kept alive by DisplayHandle Drop guard)
    let _display_handle: Option<crate::display::DisplayHandle> = match display_mode {
        crate::display::DisplayMode::NestedX11 => crate::display::launch_nested_x11(),
        crate::display::DisplayMode::Xvfb => crate::display::launch_xvfb(),
        _ => None,
    };

    // Exec wine with WINEPREFIX pointing to the merged overlay
    info!("Launching wine in OverlayFS sandbox (changes lost on exit)");

    let sandbox_env = crate::env_sanitize::build_sandbox_env(&config)?;
    let mut cmd = Command::new("wine");
    cmd.arg(exe).args(&args.args);
    cmd.env_clear();
    for (key, val) in &sandbox_env {
        cmd.env(key, val);
    }
    // Override WINEPREFIX to point at the overlay merged dir
    cmd.env("WINEPREFIX", &dirs.merged);

    // Apply display environment based on resolved mode
    apply_display_for_mode(&mut cmd, &display_mode, _display_handle.as_ref());

    // Networking: TAP bridge via Wine DLL
    if network {
        if let Err(e) = crate::net::ensure_bridge_running(Some(&config.tap_bridge_socket)) {
            warn!("Failed to start TAP bridge: {e}");
            warn!("Continuing without bridge networking");
        } else {
            if let Some(dll_dir) = crate::net::find_dll_path() {
                if let Some(parent) = dll_dir.parent() {
                    let wine_dllpath = std::env::var("WINEDLLPATH")
                        .map(|existing| format!("{existing}:{}", parent.display()))
                        .unwrap_or_else(|_| parent.display().to_string());
                    cmd.env("WINEDLLPATH", wine_dllpath);
                }
            }
            cmd.env("WINE_BRIDGE_SOCKET", &config.tap_bridge_socket);
            info!("Networking enabled via TAP bridge for OverlayFS sandbox");
        }
    }

    let err = cmd.exec();

    // exec() only returns on error — clean up before bailing
    crate::cleanup::cleanup_overlay(&dirs.merged);
    bail!("Failed to exec wine: {err}");
}

/// Apply display environment variables based on the resolved display mode.
fn apply_display_for_mode(
    cmd: &mut Command,
    mode: &crate::display::DisplayMode,
    _display_handle: Option<&crate::display::DisplayHandle>,
) {
    match mode {
        crate::display::DisplayMode::NestedX11 | crate::display::DisplayMode::Xvfb => {
            if let Some(handle) = _display_handle {
                cmd.env("DISPLAY", &handle.display);
                info!("Sandbox DISPLAY={}", handle.display);
            } else {
                // Fallback: pass host DISPLAY (not ideal but works)
                if let Ok(display) = std::env::var("DISPLAY") {
                    cmd.env("DISPLAY", &display);
                }
            }
        }
        crate::display::DisplayMode::Wayland => {
            if let Ok(wl) = std::env::var("WAYLAND_DISPLAY") {
                cmd.env("WAYLAND_DISPLAY", &wl);
                cmd.env("WINE_WAYLAND_DRIVER", "1");
                info!("Sandbox WAYLAND_DISPLAY={wl}");
            }
        }
        crate::display::DisplayMode::HostX11 => {
            if let Ok(display) = std::env::var("DISPLAY") {
                cmd.env("DISPLAY", &display);
            }
        }
    }
}

/// Paths for an overlay instance.
#[derive(Debug, Clone)]
pub struct OverlayDirs {
    pub base: String,
    pub upper: String,
    pub work: String,
    pub merged: String,
}

/// Compute overlay directory paths for a given PID.
pub fn overlay_dirs(pid: u32) -> OverlayDirs {
    let base = format!("{OVERLAY_BASE}-{pid}");
    OverlayDirs {
        upper: format!("{base}/upper"),
        work: format!("{base}/work"),
        merged: format!("{base}/merged"),
        base,
    }
}

/// Build the mount options string for overlayfs.
pub fn mount_opts(lower: &str, upper: &str, work: &str) -> String {
    format!("lowerdir={lower},upperdir={upper},workdir={work}")
}

/// Resolve the lower directory: use existing wine prefix, or create a stub.
fn resolve_lower_dir(wine_prefix: &str, base_dir: &str) -> Result<String> {
    if std::path::Path::new(wine_prefix).exists() {
        Ok(wine_prefix.to_string())
    } else {
        let stub = format!("{base_dir}/lower");
        std::fs::create_dir_all(&stub)?;
        Ok(stub)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_dirs_paths() {
        let dirs = overlay_dirs(12345);
        assert_eq!(dirs.base, "/dev/shm/win-run-12345");
        assert_eq!(dirs.upper, "/dev/shm/win-run-12345/upper");
        assert_eq!(dirs.work, "/dev/shm/win-run-12345/work");
        assert_eq!(dirs.merged, "/dev/shm/win-run-12345/merged");
    }

    #[test]
    fn mount_opts_format() {
        let opts = mount_opts("/home/user/.wine", "/tmp/upper", "/tmp/work");
        assert_eq!(opts, "lowerdir=/home/user/.wine,upperdir=/tmp/upper,workdir=/tmp/work");
    }

    #[test]
    fn resolve_lower_with_existing_prefix() {
        let result = resolve_lower_dir("/usr", "/tmp/test-overlay");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/usr");
    }

    #[test]
    fn resolve_lower_with_missing_prefix() {
        let result = resolve_lower_dir(
            "/nonexistent/wine/prefix",
            "/tmp/win-run-test-nonexistent",
        );
        assert!(result.is_ok());
        assert!(result.unwrap().contains("lower"));
        let _ = std::fs::remove_dir_all("/tmp/win-run-test-nonexistent");
    }
}
