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

/// Check if Xephyr is available for nested X11.
pub fn has_xephyr() -> bool {
    which("Xephyr").is_some()
}

/// Warn about display security implications.
pub fn warn_display_security(mode: &DisplayMode) {
    match mode {
        DisplayMode::HostX11 => {
            warn!("SECURITY: Using host X11 directly. Any process with X11 access can keylog.");
            warn!("Consider using --nested-x11 or --wayland for isolation.");
        }
        DisplayMode::NestedX11 => {
            if !has_xephyr() {
                warn!("Xephyr not found. Install xserver-xephyr for nested X11 support.");
                warn!("Falling back to Xvfb (headless).");
            }
        }
        DisplayMode::Xvfb => {
            info!("Using Xvfb (virtual framebuffer) — no visible window.");
        }
        DisplayMode::Wayland => {
            info!("Using Wayland native — Wine Wayland support is experimental.");
        }
    }
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
        // Just verify it doesn't panic
        warn_display_security(&DisplayMode::HostX11);
    }
}
