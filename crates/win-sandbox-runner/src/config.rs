use std::path::PathBuf;
use tracing::debug;

/// Paths where rules.json is searched, in priority order.
const RULES_SEARCH_PATHS: &[&str] = &[
    "~/.config/win-sandbox/rules.json",
    "/etc/win-sandbox-runner/rules.json",
];

/// Paths where the config file is searched.
const CONFIG_SEARCH_PATHS: &[&str] = &[
    "~/.config/win-sandbox/win-sandbox-runner.conf",
    "/etc/win-sandbox-runner.conf",
];

/// Runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub wine_prefix: String,
    pub rules_path: Option<PathBuf>,
    pub gui_enabled: bool,
    pub default_tier: u8,
    pub display_mode: DisplayMode,
    pub log_level: String,
    pub tap_bridge_socket: String,
    pub tap_device: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayMode {
    HostX11,
    NestedX11,
    Xvfb,
    Wayland,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            wine_prefix: default_wine_prefix(),
            rules_path: None,
            gui_enabled: true,
            default_tier: 0,
            display_mode: DisplayMode::NestedX11,
            log_level: "info".into(),
            tap_bridge_socket: "/var/run/win-tap-bridge.sock".into(),
            tap_device: "winrunner-tap0".into(),
        }
    }
}

/// Return the default WINEPREFIX (~/.wine).
fn default_wine_prefix() -> String {
    dirs_or_fallback("HOME", ".wine")
}

/// Build a path from $HOME + suffix, or fallback.
fn dirs_or_fallback(home_env: &str, suffix: &str) -> String {
    if let Ok(home) = std::env::var(home_env) {
        format!("{home}/{suffix}")
    } else {
        format!("/tmp/{suffix}")
    }
}

/// Find the rules.json path: explicit override > user config > system config.
pub fn find_rules_path(override_path: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = override_path {
        return Some(PathBuf::from(p));
    }
    for path in RULES_SEARCH_PATHS {
        let expanded = expand_tilde(path);
        if expanded.exists() {
            debug!("Found rules at: {}", expanded.display());
            return Some(expanded);
        }
    }
    None
}

/// Find and load the INI config file.
pub fn load_config(override_path: Option<&str>) -> Config {
    let mut config = Config::default();

    // Try to load from file
    let config_path = if let Some(p) = override_path {
        Some(PathBuf::from(p))
    } else {
        CONFIG_SEARCH_PATHS.iter().find_map(|p| {
            let expanded = expand_tilde(p);
            if expanded.exists() {
                Some(expanded)
            } else {
                None
            }
        })
    };

    if let Some(path) = config_path {
        debug!("Loading config from: {}", path.display());
        // TODO: Parse INI format
    }

    // Environment variable overrides
    if let Ok(prefix) = std::env::var("WINEPREFIX") {
        config.wine_prefix = prefix;
    }

    config
}

/// Expand ~ to $HOME in a path string.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(format!("{home}/{rest}"));
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_sane() {
        let config = Config::default();
        assert!(config.gui_enabled);
        assert_eq!(config.default_tier, 0);
        assert_eq!(config.display_mode, DisplayMode::NestedX11);
    }

    #[test]
    fn expand_tilde_works() {
        // Only test if HOME is set
        if std::env::var("HOME").is_ok() {
            let expanded = expand_tilde("~/test/path");
            assert!(expanded.to_str().unwrap().ends_with("/test/path"));
            assert!(!expanded.to_str().unwrap().starts_with("~"));
        }
    }

    #[test]
    fn find_rules_no_override() {
        // Should return None if no files exist (common in test env)
        let result = find_rules_path(None);
        // We just check it doesn't panic
        let _ = result;
    }
}
