use tracing::{debug, info, warn};

/// Detected display server configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayServer {
    /// Wayland compositor (native).
    Wayland { display: String },
    /// X11 server.
    X11 { display: String },
    /// Both Wayland and X11 (XWayland) — prefer X11 for Wine compatibility.
    XWayland {
        wayland_display: String,
        x11_display: String,
    },
    /// No display available (headless).
    Headless,
}

/// Display mode configuration for the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayMode {
    /// Use host X11 directly (DANGEROUS — keylogger vector).
    HostX11,
    /// Launch nested X11 via Xephyr (default for Tier 2/3).
    NestedX11,
    /// Virtual framebuffer, no visible window.
    Xvfb,
    /// Wayland native (experimental in Wine).
    Wayland,
}

/// Handle for a background display server process (Xephyr/Xvfb).
pub struct DisplayHandle {
    /// The DISPLAY value to set for the sandbox (e.g. ":42").
    pub display: String,
    /// Child process (dropped = killed).
    _child: Option<std::process::Child>,
}

impl Drop for DisplayHandle {
    fn drop(&mut self) {
        if let Some(ref mut child) = self._child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Detect the current display server environment.
pub fn detect() -> DisplayServer {
    let wayland = std::env::var("WAYLAND_DISPLAY").ok();
    let x11 = std::env::var("DISPLAY").ok();

    match (wayland, x11) {
        (Some(wl), Some(x)) => {
            info!("Display: XWayland detected (WAYLAND_DISPLAY={wl}, DISPLAY={x})");
            DisplayServer::XWayland {
                wayland_display: wl,
                x11_display: x,
            }
        }
        (Some(wl), None) => {
            info!("Display: Wayland detected (WAYLAND_DISPLAY={wl})");
            DisplayServer::Wayland { display: wl }
        }
        (None, Some(x)) => {
            info!("Display: X11 detected (DISPLAY={x})");
            DisplayServer::X11 { display: x }
        }
        (None, None) => {
            debug!("Display: Headless (no WAYLAND_DISPLAY or DISPLAY)");
            DisplayServer::Headless
        }
    }
}

/// Resolve display mode from CLI flags and current environment.
/// Priority: explicit flag > config default > auto-detect.
pub fn resolve_display_mode(
    nested_x11: bool,
    xvfb: bool,
    host_x11: bool,
    wayland: bool,
    tier: u8,
) -> DisplayMode {
    // Explicit flags take priority
    if host_x11 {
        return DisplayMode::HostX11;
    }
    if xvfb {
        return DisplayMode::Xvfb;
    }
    if wayland {
        return DisplayMode::Wayland;
    }
    if nested_x11 {
        return DisplayMode::NestedX11;
    }

    // Auto: Tier 2/3 default to nested X11, Tier 0/1 use host display
    if tier >= 2 {
        DisplayMode::NestedX11
    } else {
        // Check if Wayland is available
        if std::env::var("WAYLAND_DISPLAY").is_ok() {
            DisplayMode::Wayland
        } else {
            DisplayMode::HostX11
        }
    }
}

/// Log security warnings for the chosen display mode.
pub fn warn_display_security(mode: &DisplayMode) {
    match mode {
        DisplayMode::HostX11 => {
            warn!("SECURITY: Using host X11 directly. Any process with X11 access can keylog.");
            warn!("Consider using --nested-x11 or --wayland for isolation.");
        }
        DisplayMode::NestedX11 => {
            if !has_xephyr() {
                warn!("Xephyr not found. Install xserver-xephyr for nested X11.");
                warn!("Falling back to Xvfb (headless).");
            }
        }
        DisplayMode::Xvfb => {
            info!("Using Xvfb (virtual framebuffer) — no visible window.");
        }
        DisplayMode::Wayland => {
            warn!("Using Wayland native — Wine Wayland support is EXPERIMENTAL.");
            warn!("Many Windows apps may not render correctly or may crash.");
            warn!("If you encounter issues, try --nested-x11 or --host-x11.");
        }
    }
}

/// Launch a nested X11 server (Xephyr) on a free display.
/// Returns a DisplayHandle with the display string and child process.
pub fn launch_nested_x11() -> Option<DisplayHandle> {
    if !has_xephyr() {
        warn!("Xephyr not found — falling back to Xvfb");
        return launch_xvfb();
    }

    let display_num = find_free_display();
    let disp = format!(":{display_num}");

    info!("Launching Xephyr on {}", disp);

    let child = std::process::Command::new("Xephyr")
        .args([&disp, "-resizeable", "-title", "win-sandbox-runner"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    // Give Xephyr a moment to start
    std::thread::sleep(std::time::Duration::from_millis(200));

    info!("Xephyr started on {} (pid={})", disp, child.id());

    Some(DisplayHandle {
        display: disp,
        _child: Some(child),
    })
}

/// Launch Xvfb virtual framebuffer on a free display.
pub fn launch_xvfb() -> Option<DisplayHandle> {
    if which("Xvfb").is_none() {
        warn!("Xvfb not found — install xvfb for headless mode");
        return None;
    }

    let display_num = find_free_display();
    let disp = format!(":{display_num}");

    info!("Launching Xvfb on {}", disp);

    let child = std::process::Command::new("Xvfb")
        .args([&disp, "-screen", "0", "1920x1080x24", "-ac"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    std::thread::sleep(std::time::Duration::from_millis(100));

    info!("Xvfb started on {} (pid={})", disp, child.id());

    Some(DisplayHandle {
        display: disp,
        _child: Some(child),
    })
}

/// Check if Xephyr is available for nested X11.
pub fn has_xephyr() -> bool {
    which("Xephyr").is_some()
}

/// Find the next free X display number by checking /tmp/.X*-lock files.
fn find_free_display() -> u32 {
    for n in 1..100 {
        let lock = format!("/tmp/.X{n}-lock");
        if !std::path::Path::new(&lock).exists() {
            return n;
        }
    }
    // Fallback: use PID as display number (unlikely collision)
    std::process::id() % 100 + 100
}

/// Find a command in PATH.
fn which(cmd: &str) -> Option<String> {
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let full = format!("{dir}/{cmd}");
            if std::path::Path::new(&full).exists() {
                return Some(full);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_does_not_panic() {
        let _display = detect();
    }

    #[test]
    fn host_x11_warns() {
        warn_display_security(&DisplayMode::HostX11);
    }

    #[test]
    fn resolve_tier2_defaults_to_nested() {
        // Tier 2+ with no explicit flags should default to NestedX11
        let mode = resolve_display_mode(false, false, false, false, 2);
        assert_eq!(mode, DisplayMode::NestedX11);
    }

    #[test]
    fn resolve_tier3_defaults_to_nested() {
        let mode = resolve_display_mode(false, false, false, false, 3);
        assert_eq!(mode, DisplayMode::NestedX11);
    }

    #[test]
    fn resolve_explicit_flags_override() {
        assert_eq!(
            resolve_display_mode(false, true, false, false, 2),
            DisplayMode::Xvfb
        );
        assert_eq!(
            resolve_display_mode(false, false, true, false, 2),
            DisplayMode::HostX11
        );
        assert_eq!(
            resolve_display_mode(false, false, false, true, 2),
            DisplayMode::Wayland
        );
        assert_eq!(
            resolve_display_mode(true, false, false, false, 0),
            DisplayMode::NestedX11
        );
    }

    #[test]
    fn find_free_display_returns_number() {
        let n = find_free_display();
        assert!(n > 0);
    }
}
