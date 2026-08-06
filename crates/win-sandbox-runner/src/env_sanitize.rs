use crate::config::Config;
use anyhow::Result;
use std::collections::HashMap;
use tracing::debug;

/// Environment variables that are safe to forward into the sandbox.
const ALLOWED_ENV_VARS: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
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
pub fn build_sandbox_env(config: &Config) -> Result<HashMap<String, String>> {
    let mut env = HashMap::new();

    // Forward allowed variables from current environment
    for &var in ALLOWED_ENV_VARS {
        if let Ok(val) = std::env::var(var) {
            debug!("Forwarding env: {var}");
            env.insert(var.to_string(), val);
        }
    }

    // Always set WINEPREFIX from config
    env.insert("WINEPREFIX".to_string(), config.wine_prefix.clone());

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
        let config = Config::default();
        let env = build_sandbox_env(&config).unwrap();
        assert!(env.contains_key("WINEPREFIX"));
        assert!(env.contains_key("WIN_SANDBOX_ACTIVE"));
        assert!(env.contains_key("HOSTNAME"));
    }

    #[test]
    fn build_sandbox_env_strips_secrets() {
        // Set a secret env var
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "supersecret");
        let config = Config::default();
        let env = build_sandbox_env(&config).unwrap();
        assert!(!env.contains_key("AWS_SECRET_ACCESS_KEY"));
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");
    }
}
