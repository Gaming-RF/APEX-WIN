use crate::config::Config;
use anyhow::Result;
use std::collections::HashMap;
use tracing::debug;

/// Environment variables that are safe to forward into the sandbox.
const ALLOWED_ENV_VARS: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "XDG_SESSION_TYPE",
    "HOME",
    "TERM",
    "USER",
    "WINEDEBUG",
    "WINEDLLOVERRIDES",
    "WINEPREFIX",
    "XDG_RUNTIME_DIR",
    "PULSE_SERVER",
    "PIPEWIRE_RUNTIME_DIR",
];

/// Environment variables that are explicitly stripped (security-sensitive).
const STRIPPED_ENV_VARS: &[&str] = &[
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "DBUS_SESSION_BUS_ADDRESS",
    "GPG_AGENT_INFO",
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "http_proxy",
    "https_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "no_proxy",
    "NO_PROXY",
];

/// Build a sanitized environment for the sandboxed process.
///
/// Only allows explicitly whitelisted env vars through. Strips secrets,
/// proxy configs, and other sensitive data. Randomizes HOSTNAME.
///
/// When `user_env` is provided (daemon mode), those values take priority
/// over the current process environment for allowed variables.
pub fn build_sandbox_env(
    config: &Config,
    user_env: Option<&HashMap<String, String>>,
) -> Result<HashMap<String, String>> {
    let mut env = HashMap::new();

    // Forward allowed variables from current environment
    for &var in ALLOWED_ENV_VARS {
        if let Ok(val) = std::env::var(var) {
            debug!("Forwarding env: {var}");
            env.insert(var.to_string(), val);
        }
    }

    // Overlay user-provided env (daemon mode: user's display, runtime dir, etc.)
    if let Some(user) = user_env {
        for &var in ALLOWED_ENV_VARS {
            if let Some(val) = user.get(var) {
                debug!("User env override: {var}");
                env.insert(var.to_string(), val.clone());
            }
        }
    }

    // WINEPREFIX resolution order (most specific wins):
    //   1. WINEPREFIX already in env (set by dispatch for the per-app prefix)
    //   2. user_env override (daemon mode)
    //   3. config default (~/.wine)
    // Never clobber an explicitly-resolved per-app prefix.
    if !env.contains_key("WINEPREFIX") {
        env.insert("WINEPREFIX".to_string(), config.wine_prefix.clone());
    }

    // Set sanitized PATH
    env.insert("PATH".to_string(), build_sanitized_path());

    // Randomize HOSTNAME to prevent fingerprinting
    env.insert("HOSTNAME".to_string(), random_hostname());

    // Set recursion guard
    env.insert("WIN_SANDBOX_ACTIVE".to_string(), "1".to_string());

    // Verify stripped vars are not present
    for &var in STRIPPED_ENV_VARS {
        if env.contains_key(var) {
            env.remove(var);
            debug!("Stripped sensitive env: {var}");
        }
    }

    Ok(env)
}

/// Build a sanitized PATH that excludes common development tool directories.
fn build_sanitized_path() -> String {
    let default = "/usr/bin:/bin:/usr/sbin:/sbin".to_string();
    if let Ok(current) = std::env::var("PATH") {
        // Filter out paths that might expose host tools
        let safe: Vec<&str> = current
            .split(':')
            .filter(|p| {
                // Keep standard system paths
                p.starts_with("/usr/bin")
                    || p.starts_with("/bin")
                    || p.starts_with("/usr/sbin")
                    || p.starts_with("/sbin")
                    || p.starts_with("/usr/local/bin")
            })
            .collect();
        if safe.is_empty() {
            default
        } else {
            safe.join(":")
        }
    } else {
        default
    }
}

/// Generate a random 12-hex-digit hostname.
fn random_hostname() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("winrun-{:012x}", nanos & 0xfffffffffff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that mutate process-global environment variables.
    /// Rust runs tests in parallel threads that share one environment, so
    /// without this two env-mutating tests can observe each other's writes.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn sanitized_path_contains_system() {
        let path = build_sanitized_path();
        assert!(path.contains("/usr/bin"));
    }

    #[test]
    fn random_hostname_format() {
        let host = random_hostname();
        assert!(host.starts_with("winrun-"));
        assert_eq!(host.len(), 19); // "winrun-" + 12 hex chars
    }

    #[test]
    fn build_sandbox_env_has_wineprefix() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let config = Config::default();
        let env = build_sandbox_env(&config, None).unwrap();
        assert!(env.contains_key("WINEPREFIX"));
        assert!(env.contains_key("WIN_SANDBOX_ACTIVE"));
        assert!(env.contains_key("HOSTNAME"));
    }

    #[test]
    fn build_sandbox_env_strips_secrets() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Set a secret env var
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "supersecret");
        let config = Config::default();
        let env = build_sandbox_env(&config, None).unwrap();
        assert!(!env.contains_key("AWS_SECRET_ACCESS_KEY"));
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");
    }

    #[test]
    fn build_sandbox_env_merges_user_env() {
        let config = Config::default();
        let mut user_env = HashMap::new();
        user_env.insert("DISPLAY".to_string(), ":99".to_string());
        user_env.insert("WAYLAND_DISPLAY".to_string(), "wayland-1".to_string());
        let env = build_sandbox_env(&config, Some(&user_env)).unwrap();
        assert_eq!(env.get("DISPLAY").unwrap(), ":99");
        assert_eq!(env.get("WAYLAND_DISPLAY").unwrap(), "wayland-1");
    }

    #[test]
    fn build_sandbox_env_user_env_only_allowed() {
        let config = Config::default();
        let mut user_env = HashMap::new();
        user_env.insert("DISPLAY".to_string(), ":0".to_string());
        user_env.insert("SECRET_VAR".to_string(), "should_not_pass".to_string());
        let env = build_sandbox_env(&config, Some(&user_env)).unwrap();
        assert_eq!(env.get("DISPLAY").unwrap(), ":0");
        assert!(!env.contains_key("SECRET_VAR"));
    }

    /// X11 sessions need XAUTHORITY or Wine cannot open the display.
    /// GDM stores the cookie at /run/user/<uid>/gdm/Xauthority with no
    /// ~/.Xauthority fallback, so dropping this var breaks every launch.
    #[test]
    fn build_sandbox_env_forwards_xauthority() {
        let config = Config::default();
        let mut user_env = HashMap::new();
        user_env.insert(
            "XAUTHORITY".to_string(),
            "/run/user/1000/gdm/Xauthority".to_string(),
        );
        user_env.insert("XDG_SESSION_TYPE".to_string(), "x11".to_string());
        let env = build_sandbox_env(&config, Some(&user_env)).unwrap();
        assert_eq!(
            env.get("XAUTHORITY").unwrap(),
            "/run/user/1000/gdm/Xauthority",
            "XAUTHORITY must reach Wine or X11 auth fails"
        );
        assert_eq!(env.get("XDG_SESSION_TYPE").unwrap(), "x11");
    }

    /// dispatch::execute resolves a per-app WINEPREFIX and exports it before
    /// calling into a tier. The sanitizer must not overwrite it with the
    /// config default, or every app collapses into one shared prefix.
    #[test]
    fn build_sandbox_env_preserves_per_app_wineprefix() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let per_app = "/home/u/.local/share/win-sandbox/prefixes/deadbeef/prefix";
        std::env::set_var("WINEPREFIX", per_app);

        let config = Config {
            wine_prefix: "/home/u/.wine".to_string(),
            ..Config::default()
        };

        let env = build_sandbox_env(&config, None).unwrap();
        assert_eq!(
            env.get("WINEPREFIX").unwrap(),
            per_app,
            "per-app prefix must survive sanitization"
        );

        std::env::remove_var("WINEPREFIX");
    }

    /// With nothing pre-set, the config default is still applied.
    #[test]
    fn build_sandbox_env_falls_back_to_config_wineprefix() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("WINEPREFIX");
        let config = Config {
            wine_prefix: "/home/u/.wine".to_string(),
            ..Config::default()
        };
        let env = build_sandbox_env(&config, None).unwrap();
        assert_eq!(env.get("WINEPREFIX").unwrap(), "/home/u/.wine");
    }
}
