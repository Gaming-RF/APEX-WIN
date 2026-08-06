use crate::Args;
use anyhow::{bail, Result};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};
use tracing::{debug, info, warn};

/// System directories that get read-only bind-mounted into the container.
const RO_SYSTEM_DIRS: &[&str] = &[
    "/usr", "/lib", "/lib32", "/lib64", "/bin", "/sbin", "/opt", "/etc",
];

/// Tier 2: Bubblewrap container with optional networking.
#[allow(dead_code)]
pub fn run(args: &Args) -> Result<ExitCode> {
    run_with_network(args, false)
}

/// Tier 2: Bubblewrap container with explicit network control.
///
/// When `network` is true, starts the TAP bridge daemon and bind-mounts
/// its Unix socket + sys_netmp.dll into the container for isolated
/// Wine networking. When false, the container runs without network.
pub fn run_with_network(args: &Args, network: bool) -> Result<ExitCode> {
    info!("Tier 2: Bubblewrap container for {} (network={network})", args.exe);

    let config = crate::config::load_config(None);

    let nvidia = crate::nvidia::detect();
    let amd_gpu = crate::amd::detect();
    let audio = crate::audio::detect();
    let display = crate::display::detect();

    if nvidia.is_some() {
        warn!("Nvidia GPU detected — GPU passthrough with bwrap may fail");
        warn!("Consider using --tier 1 (Landlock) for Nvidia systems");
    }

    // Resolve display mode and launch nested display server if needed
    let display_mode = crate::display::resolve_display_mode(
        args.nested_x11,
        args.xvfb,
        args.host_x11,
        args.wayland,
        2, // Tier 2
    );
    crate::display::warn_display_security(&display_mode);

    // Launch Xephyr/Xvfb if needed (kept alive by DisplayHandle Drop guard)
    let _display_handle: Option<crate::display::DisplayHandle> = match display_mode {
        crate::display::DisplayMode::NestedX11 => crate::display::launch_nested_x11(),
        crate::display::DisplayMode::Xvfb => crate::display::launch_xvfb(),
        _ => None,
    };

    let hostname = sandbox_hostname();

    let mut cmd = Command::new("bwrap");

    // Namespace flags
    cmd.arg("--unshare-all");
    cmd.arg("--share-net");
    cmd.arg("--die-with-parent");
    cmd.args(["--hostname", &hostname]);

    // Root filesystem: new tmpfs
    cmd.args(["--tmpfs", "/"]);

    // System directories: read-only binds
    for dir in existing_ro_dirs() {
        cmd.args(["--ro-bind", dir, dir]);
    }

    // Proc, dev, tmp
    cmd.args(["--proc", "/proc"]);
    cmd.args(["--dev", "/dev"]);
    cmd.args(["--tmpfs", "/tmp"]);

    // Home directory: new tmpfs (isolated from host)
    let username = std::env::var("USER").unwrap_or_else(|_| "user".into());
    cmd.args(["--tmpfs", "/home"]);
    cmd.args(["--tmpfs", &format!("/home/{username}")]);

    // WINEPREFIX: bind-mount if it exists
    let wine_prefix = &config.wine_prefix;
    if std::path::Path::new(wine_prefix).exists() {
        cmd.args(["--ro-bind", wine_prefix, wine_prefix]);
        let drive_c = format!("{wine_prefix}/drive_c");
        if std::path::Path::new(&drive_c).exists() {
            cmd.args(["--bind", &drive_c, &drive_c]);
        }
    }

    // The executable's directory: bind RW
    if let Some(parent) = exe_parent_dir(&args.exe) {
        cmd.args(["--bind", parent, parent]);
    }

    // XDG_RUNTIME_DIR for audio/display sockets
    if let Some(runtime) = existing_xdg_runtime() {
        cmd.args(["--bind", &runtime, &runtime]);
    }

    // GPU passthrough
    bind_gpu_devices(&mut cmd, nvidia.is_some(), amd_gpu.is_some());

    // Gamepad/controller access (Plan §11: /dev/input isolation)
    if args.gamepad {
        bind_gamepad_devices(&mut cmd);
    }

    // Audio socket
    if let Some(ref server) = audio {
        let socket_path = match server {
            crate::audio::AudioServer::PipeWire { socket } => socket,
            crate::audio::AudioServer::PulseAudio { socket } => socket,
        };
        let socket_str = socket_path.to_string_lossy();
        if socket_path.exists() {
            cmd.args(["--bind", &socket_str, &socket_str]);
        }
    }

    // NTsync (Wine 9+ synchronization device)
    crate::net::bind_ntsync(&mut cmd);

    // Sanitized environment — clear inherited env, apply allowlisted vars only
    let sandbox_env = crate::env_sanitize::build_sandbox_env(&config)?;
    cmd.env_clear();
    for (key, val) in &sandbox_env {
        cmd.env(key, val);
    }

    // Apply display environment based on resolved mode
    apply_display_for_mode(&mut cmd, &display_mode, &display, _display_handle.as_ref());

    // Networking: TAP bridge or isolated
    if let Err(e) = crate::net::configure_bwrap_networking(&mut cmd, network, &config) {
        warn!("Networking setup failed: {e}");
        warn!("Continuing without bridge networking");
    }

    // The command to run inside the sandbox
    cmd.args(["--", "wine"]);
    cmd.arg(&args.exe);
    cmd.args(&args.args);

    info!("Executing bwrap for {}", args.exe);

    let err = cmd.exec();
    bail!("Failed to exec bwrap: {err}");
}

/// Generate a sandbox hostname from the current PID.
fn sandbox_hostname() -> String {
    format!("winrun-{:08x}", std::process::id())
}

/// Return system dirs from RO_SYSTEM_DIRS that exist on this system.
fn existing_ro_dirs() -> Vec<&'static str> {
    RO_SYSTEM_DIRS
        .iter()
        .copied()
        .filter(|d| std::path::Path::new(d).exists())
        .collect()
}

/// Return the parent directory of the exe, if non-root and non-empty.
fn exe_parent_dir(exe: &str) -> Option<&str> {
    let parent = std::path::Path::new(exe).parent()?;
    let s = parent.to_str()?;
    if s.is_empty() || s == "/" {
        None
    } else {
        Some(s)
    }
}

/// Return $XDG_RUNTIME_DIR if set and exists.
fn existing_xdg_runtime() -> Option<String> {
    let dir = std::env::var("XDG_RUNTIME_DIR").ok()?;
    if std::path::Path::new(&dir).exists() {
        Some(dir)
    } else {
        None
    }
}

/// Bind-mount GPU device nodes into the container.
fn bind_gpu_devices(cmd: &mut Command, has_nvidia: bool, has_amd: bool) {
    if has_nvidia {
        if std::path::Path::new("/dev/dri").exists() {
            cmd.args(["--dev-bind", "/dev/dri", "/dev/dri"]);
        }
        for i in 0..8 {
            let dev = format!("/dev/nvidia{i}");
            if std::path::Path::new(&dev).exists() {
                cmd.args(["--dev-bind", &dev, &dev]);
            }
        }
        for dev in &["/dev/nvidiactl", "/dev/nvidia-uvm"] {
            if std::path::Path::new(dev).exists() {
                cmd.args(["--dev-bind", dev, dev]);
            }
        }
    } else if has_amd && std::path::Path::new("/dev/dri").exists() {
        cmd.args(["--dev-bind", "/dev/dri", "/dev/dri"]);
    }
}

/// Bind-mount gamepad/controller devices from /dev/input.
/// Scans for /dev/input/event* devices and bind-mounts only gamepad-related ones.
/// Falls back to binding the entire /dev/input directory if sysfs scanning fails.
fn bind_gamepad_devices(cmd: &mut Command) {
    let input_dir = std::path::Path::new("/dev/input");
    if !input_dir.exists() {
        debug!("Gamepad: /dev/input does not exist");
        return;
    }

    // Try to read /sys/class/input/*/device/name to identify gamepads
    let gamepad_keywords = ["gamepad", "controller", "joystick", "xbox", "playstation",
                            "dualshock", "dualsense", "switch", "pro controller", "joy-con"];

    let mut found_gamepads = false;

    if let Ok(entries) = std::fs::read_dir("/sys/class/input") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            // Only look at event* entries
            if !name_str.starts_with("event") {
                continue;
            }

            // Read the device name from sysfs
            let device_name_path = entry.path().join("device").join("name");
            if let Ok(device_name) = std::fs::read_to_string(&device_name_path) {
                let device_name_lower = device_name.trim().to_lowercase();
                let is_gamepad = gamepad_keywords.iter().any(|kw| device_name_lower.contains(kw));

                if is_gamepad {
                    let event_path = format!("/dev/input/{name_str}");
                    if std::path::Path::new(&event_path).exists() {
                        cmd.args(["--dev-bind", &event_path, &event_path]);
                        info!("Gamepad: bind-mounted {event_path} ({})", device_name.trim());
                        found_gamepads = true;
                    }
                }
            }
        }
    }

    if !found_gamepads {
        debug!("Gamepad: no gamepad devices found in /dev/input");
        debug!("Gamepad: if you have a controller, ensure it is connected before launching");
    }
}

/// Apply display environment variables based on the resolved display mode.
fn apply_display_for_mode(
    cmd: &mut Command,
    mode: &crate::display::DisplayMode,
    detected: &crate::display::DisplayServer,
    _display_handle: Option<&crate::display::DisplayHandle>,
) {
    match mode {
        crate::display::DisplayMode::NestedX11 | crate::display::DisplayMode::Xvfb => {
            // Use the display from Xephyr/Xvfb if launched
            if let Some(handle) = _display_handle {
                cmd.env("DISPLAY", &handle.display);
                info!("Sandbox DISPLAY={}", handle.display);
            } else {
                // Fallback: use detected display
                apply_display_env(cmd, detected);
            }
        }
        crate::display::DisplayMode::Wayland => {
            if let crate::display::DisplayServer::Wayland { display: wl }
            | crate::display::DisplayServer::XWayland { wayland_display: wl, .. } = detected
            {
                cmd.env("WAYLAND_DISPLAY", wl);
                cmd.env("WINE_WAYLAND_DRIVER", "1");
                info!("Sandbox WAYLAND_DISPLAY={}", wl);
            } else {
                warn!("--wayland specified but no Wayland display detected");
            }
        }
        crate::display::DisplayMode::HostX11 => {
            apply_display_env(cmd, detected);
        }
    }
}

/// Set DISPLAY/WAYLAND_DISPLAY env vars based on detected display server.
fn apply_display_env(cmd: &mut Command, display: &crate::display::DisplayServer) {
    match display {
        crate::display::DisplayServer::X11 { display } => {
            cmd.env("DISPLAY", display);
        }
        crate::display::DisplayServer::Wayland { display } => {
            cmd.env("WAYLAND_DISPLAY", display);
            cmd.env("WINE_WAYLAND_DRIVER", "1");
        }
        crate::display::DisplayServer::XWayland { x11_display, .. } => {
            cmd.env("DISPLAY", x11_display);
        }
        crate::display::DisplayServer::Headless => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_format() {
        let h = sandbox_hostname();
        assert!(h.starts_with("winrun-"));
        assert_eq!(h.len(), 15);
        let hex_part = &h[7..];
        assert!(u32::from_str_radix(hex_part, 16).is_ok());
    }

    #[test]
    fn ro_dirs_filter_existing() {
        let dirs = existing_ro_dirs();
        assert!(dirs.contains(&"/usr"));
        assert!(dirs.contains(&"/etc"));
        assert!(dirs.contains(&"/bin"));
        for d in &dirs {
            assert!(RO_SYSTEM_DIRS.contains(d));
        }
    }

    #[test]
    fn exe_parent_dir_basic() {
        assert_eq!(exe_parent_dir("/home/user/game.exe"), Some("/home/user"));
        assert_eq!(exe_parent_dir("/game.exe"), None);
    }

    #[test]
    fn exe_parent_dir_empty() {
        assert_eq!(exe_parent_dir("game.exe"), None);
    }

    #[test]
    fn apply_display_env_x11() {
        let display = crate::display::DisplayServer::X11 {
            display: ":0".into(),
        };
        let mut cmd = Command::new("true");
        apply_display_env(&mut cmd, &display);
    }

    #[test]
    fn apply_display_env_wayland() {
        let display = crate::display::DisplayServer::Wayland {
            display: "wayland-0".into(),
        };
        let mut cmd = Command::new("true");
        apply_display_env(&mut cmd, &display);
    }

    #[test]
    fn apply_display_env_headless() {
        let display = crate::display::DisplayServer::Headless;
        let mut cmd = Command::new("true");
        apply_display_env(&mut cmd, &display);
    }

    #[test]
    fn bind_gpu_no_gpu() {
        let mut cmd = Command::new("true");
        bind_gpu_devices(&mut cmd, false, false);
    }

    #[test]
    fn bind_gamepad_does_not_panic() {
        let mut cmd = Command::new("true");
        bind_gamepad_devices(&mut cmd);
    }
}
