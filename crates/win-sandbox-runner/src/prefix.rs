use anyhow::{Context, Result};
use nix::unistd::{Gid, Uid};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// State file written into each per-app prefix to track what's been installed.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PrefixState {
    /// Whether DXVK has been installed into this prefix.
    pub dxvk_installed: bool,
    /// Winetricks components that have been installed.
    pub installed_components: Vec<String>,
    /// Wine version that created this prefix.
    pub wine_version: String,
    /// Timestamp of last setup (ISO 8601).
    pub last_setup: Option<String>,
}

/// Per-app Wine prefix manager.
///
/// Layout:
///   ~/.local/share/win-sandbox/prefixes/<hash>/   — WINEPREFIX
///   ~/.local/share/win-sandbox/prefixes/<hash>/state.json — installation state
pub struct PrefixManager {
    base_dir: PathBuf,
}

impl PrefixManager {
    /// Create a prefix manager rooted from the **current process** environment.
    ///
    /// Only correct for direct CLI use, where the process already belongs to the
    /// invoking user. The daemon must NOT use this: it runs as root under
    /// systemd, where HOME is `/root` (EACCES) or unset (falls back to `/tmp`,
    /// which Wine refuses with "not owned by you"). Named explicitly so the
    /// wrong choice is visible at the call site rather than hidden behind
    /// `new()`. Daemon paths use [`PrefixManager::for_user`].
    pub fn for_current_process() -> Self {
        let base_dir = dirs_or_fallback("XDG_DATA_HOME", ".local/share/win-sandbox/prefixes");
        Self { base_dir }
    }

    /// Create a prefix manager rooted at the *invoking user's* home.
    ///
    /// `user_env` is the environment forwarded over the daemon FIFO. Uses
    /// XDG_DATA_HOME if present, else HOME. Falls back to
    /// [`Self::for_current_process`] when neither is available, preserving
    /// direct-CLI behaviour.
    pub fn for_user(user_env: &std::collections::HashMap<String, String>) -> Self {
        if let Some(xdg) = user_env.get("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
            return Self {
                base_dir: PathBuf::from(xdg).join("win-sandbox/prefixes"),
            };
        }
        if let Some(home) = user_env.get("HOME").filter(|v| !v.is_empty()) {
            return Self {
                base_dir: PathBuf::from(home).join(".local/share/win-sandbox/prefixes"),
            };
        }
        Self::for_current_process()
    }

    /// Create a prefix manager with a custom base directory (for testing).
    #[allow(dead_code)]
    pub fn with_base(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Get the prefix directory for a given binary hash.
    pub fn prefix_dir(&self, hash: &str) -> PathBuf {
        self.base_dir.join(hash)
    }

    /// Get the WINEPREFIX path for a given binary hash.
    pub fn wine_prefix(&self, hash: &str) -> PathBuf {
        self.prefix_dir(hash).join("prefix")
    }

    /// Get the state file path for a given binary hash.
    pub fn state_path(&self, hash: &str) -> PathBuf {
        self.prefix_dir(hash).join("state.json")
    }

    /// Load the installation state for a prefix.
    pub fn load_state(&self, hash: &str) -> PrefixState {
        let path = self.state_path(hash);
        if path.exists() {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        } else {
            PrefixState::default()
        }
    }

    /// Save the installation state for a prefix.
    pub fn save_state(&self, hash: &str, state: &PrefixState) -> Result<()> {
        let path = self.state_path(hash);
        let json = serde_json::to_string_pretty(state)?;
        std::fs::write(&path, json)
            .with_context(|| format!("Failed to write state to {}", path.display()))?;
        Ok(())
    }

    /// Ensure a Wine prefix exists for the given hash. Creates it if missing.
    /// Returns the WINEPREFIX path.
    ///
    /// When `uid` is set (daemon mode), created directories are chowned to that user.
    pub fn ensure_prefix(&self, hash: &str, uid: Option<u32>) -> Result<PathBuf> {
        let prefix = self.wine_prefix(hash);
        if !prefix.exists() {
            info!("Creating Wine prefix at {}", prefix.display());
            std::fs::create_dir_all(&prefix)
                .with_context(|| format!("Failed to create prefix dir {}", prefix.display()))?;

            // Chown prefix directory tree to the target user (daemon runs as root)
            if let Some(uid) = uid {
                chown_recursive(&self.prefix_dir(hash), uid);
            }

            // Initialize the prefix with wineboot
            let status = std::process::Command::new("wineboot")
                .arg("--init")
                .env("WINEPREFIX", &prefix)
                .env("WINEDEBUG", "-all")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();

            match status {
                Ok(s) if s.success() => {
                    info!("Wine prefix initialized at {}", prefix.display());
                }
                Ok(s) => {
                    warn!("wineboot exited with status: {}", s);
                }
                Err(e) => {
                    warn!("Failed to run wineboot: {e}");
                    warn!("Prefix created but not initialized — wine will init on first run");
                }
            }
        }
        Ok(prefix)
    }

    /// Check if all required dependencies are installed for an app.
    /// Returns (missing_dxvk, missing_winetricks_components).
    pub fn check_deps(
        &self,
        hash: &str,
        needs_dxvk: bool,
        winetricks: &[String],
    ) -> (bool, Vec<String>) {
        let state = self.load_state(hash);
        let missing_dxvk = needs_dxvk && !state.dxvk_installed;
        let missing_wt: Vec<String> = winetricks
            .iter()
            .filter(|c| !state.installed_components.contains(c))
            .cloned()
            .collect();
        (missing_dxvk, missing_wt)
    }

    /// Install DXVK into the prefix at the given hash.
    /// This runs `setup_dxvk.sh install` inside the prefix.
    pub fn install_dxvk(&self, hash: &str) -> Result<()> {
        let prefix = self.wine_prefix(hash);
        info!("Installing DXVK into {}", prefix.display());

        // Try system-installed DXVK setup script first
        let script_candidates = [
            "/usr/share/dxvk/setup_dxvk.sh",
            "/usr/local/share/dxvk/setup_dxvk.sh",
        ];

        for script in &script_candidates {
            if Path::new(script).exists() {
                let status = std::process::Command::new("bash")
                    .arg(script)
                    .arg("install")
                    .env("WINEPREFIX", &prefix)
                    .env("WINEDEBUG", "-all")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();

                match status {
                    Ok(s) if s.success() => {
                        info!("DXVK installed via {script}");
                        let mut state = self.load_state(hash);
                        state.dxvk_installed = true;
                        self.save_state(hash, &state)?;
                        return Ok(());
                    }
                    Ok(s) => {
                        warn!("DXVK setup at {script} exited with: {s}");
                    }
                    Err(e) => {
                        warn!("Failed to run DXVK setup at {script}: {e}");
                    }
                }
            }
        }

        // If no system script, try winetricks
        info!("No system DXVK script found, attempting via winetricks");
        let status = std::process::Command::new("winetricks")
            .arg("dxvk")
            .env("WINEPREFIX", &prefix)
            .env("WINEDEBUG", "-all")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        match status {
            Ok(s) if s.success() => {
                info!("DXVK installed via winetricks");
                let mut state = self.load_state(hash);
                state.dxvk_installed = true;
                self.save_state(hash, &state)?;
                Ok(())
            }
            Ok(s) => {
                warn!("winetricks dxvk exited with: {s}");
                warn!("DXVK may not be installed — app may fall back to wined3d");
                Ok(()) // Non-fatal
            }
            Err(e) => {
                warn!("winetricks not found or failed: {e}");
                warn!("Install winetricks: sudo apt install winetricks");
                Ok(()) // Non-fatal
            }
        }
    }

    /// Install winetricks components into the prefix.
    /// Skips components already recorded in the state.
    pub fn install_winetricks(&self, hash: &str, components: &[String]) -> Result<()> {
        if components.is_empty() {
            return Ok(());
        }

        let prefix = self.wine_prefix(hash);
        let mut state = self.load_state(hash);

        let missing: Vec<&String> = components
            .iter()
            .filter(|c| !state.installed_components.contains(c))
            .collect();

        if missing.is_empty() {
            info!("All winetricks components already installed");
            return Ok(());
        }

        info!("Installing winetricks components: {:?}", missing);

        let status = std::process::Command::new("winetricks")
            .args(&missing)
            .env("WINEPREFIX", &prefix)
            .env("WINEDEBUG", "-all")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        match status {
            Ok(s) if s.success() => {
                for c in &missing {
                    state.installed_components.push(c.to_string());
                }
                state.last_setup = Some(chrono_now());
                self.save_state(hash, &state)?;
                info!("Winetricks components installed successfully");
                Ok(())
            }
            Ok(s) => {
                warn!("winetricks exited with: {s}");
                // Still mark as attempted to avoid retry loops
                for c in &missing {
                    state.installed_components.push(c.to_string());
                }
                state.last_setup = Some(chrono_now());
                self.save_state(hash, &state)?;
                Ok(())
            }
            Err(e) => {
                warn!("winetricks not found or failed: {e}");
                warn!("Install winetricks: sudo apt install winetricks");
                Ok(()) // Non-fatal
            }
        }
    }

    /// Full setup: ensure prefix exists, install DXVK and winetricks if needed.
    /// Returns the WINEPREFIX path. Idempotent — skips already-installed deps.
    ///
    /// When `uid` is set (daemon mode), created directories are chowned to that user.
    pub fn setup_app(
        &self,
        hash: &str,
        needs_dxvk: bool,
        winetricks: &[String],
        uid: Option<u32>,
    ) -> Result<PathBuf> {
        let prefix = self.ensure_prefix(hash, uid)?;

        let (missing_dxvk, missing_wt) = self.check_deps(hash, needs_dxvk, winetricks);

        if !missing_dxvk && missing_wt.is_empty() {
            info!("All app dependencies already satisfied");
            return Ok(prefix);
        }

        if missing_dxvk {
            if let Err(e) = self.install_dxvk(hash) {
                warn!("DXVK install failed (non-fatal): {e}");
            }
        }

        if !missing_wt.is_empty() {
            if let Err(e) = self.install_winetricks(hash, &missing_wt) {
                warn!("Winetricks install failed (non-fatal): {e}");
            }
        }

        Ok(prefix)
    }
}

impl Default for PrefixManager {
    fn default() -> Self {
        Self::for_current_process()
    }
}

/// Build a path from $HOME + suffix, or fallback.
fn dirs_or_fallback(env: &str, suffix: &str) -> PathBuf {
    let base = if let Ok(val) = std::env::var(env) {
        PathBuf::from(val)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
    } else {
        PathBuf::from("/tmp")
    };
    base.join(suffix)
}

/// Recursively chown a directory tree to the given UID.
/// Looks up the user's primary group via getpwuid.
fn chown_recursive(path: &Path, uid: u32) {
    let uid = Uid::from_raw(uid);
    // Look up primary group; fallback to keeping current group
    let gid = unsafe {
        let passwd = libc::getpwuid(uid.as_raw());
        if passwd.is_null() {
            None
        } else {
            Some(Gid::from_raw((*passwd).pw_gid))
        }
    };

    if let Err(e) = nix::unistd::chown(path, Some(uid), gid) {
        warn!("Failed to chown {}: {e}", path.display());
    }

    // Recurse into subdirectories
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let child = entry.path();
            if child.is_dir() {
                chown_recursive(&child, uid.as_raw());
            } else if let Err(e) = nix::unistd::chown(&child, Some(uid), gid) {
                warn!("Failed to chown {}: {e}", child.display());
            }
        }
    }
}

/// Current timestamp as ISO 8601 string (simple, no chrono dep).
fn chrono_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// The daemon runs as root, where HOME is /root or unset. It must root
    /// prefixes at the invoking user's home, taken from the forwarded FIFO
    /// env, or Wine gets an unwritable path (/root) or one it refuses (/tmp).
    #[test]
    fn for_user_uses_forwarded_home() {
        let mut env = std::collections::HashMap::new();
        env.insert("HOME".to_string(), "/home/alice".to_string());

        let mgr = PrefixManager::for_user(&env);
        let prefix = mgr.wine_prefix("abc123");

        assert!(
            prefix.starts_with("/home/alice/.local/share/win-sandbox/prefixes"),
            "prefix must live under the user's home, got {}",
            prefix.display()
        );
        assert!(!prefix.starts_with("/root"), "must never use root's home");
        assert!(
            !prefix.starts_with("/tmp"),
            "Wine refuses /tmp prefixes: 'not owned by you'"
        );
    }

    /// XDG_DATA_HOME takes priority over HOME when the user sets it.
    #[test]
    fn for_user_prefers_xdg_data_home() {
        let mut env = std::collections::HashMap::new();
        env.insert("HOME".to_string(), "/home/alice".to_string());
        env.insert("XDG_DATA_HOME".to_string(), "/home/alice/data".to_string());

        let mgr = PrefixManager::for_user(&env);
        assert!(mgr
            .wine_prefix("abc123")
            .starts_with("/home/alice/data/win-sandbox/prefixes"));
    }

    /// Empty values are ignored rather than producing a relative path.
    #[test]
    fn for_user_ignores_empty_values() {
        let mut env = std::collections::HashMap::new();
        env.insert("XDG_DATA_HOME".to_string(), String::new());
        env.insert("HOME".to_string(), "/home/bob".to_string());

        let mgr = PrefixManager::for_user(&env);
        assert!(mgr
            .wine_prefix("h")
            .starts_with("/home/bob/.local/share/win-sandbox/prefixes"));
    }

    /// With no forwarded env, behaviour matches the direct-CLI constructor.
    #[test]
    fn for_user_falls_back_to_process_env() {
        let env = std::collections::HashMap::new();
        assert_eq!(
            PrefixManager::for_user(&env).wine_prefix("x"),
            PrefixManager::for_current_process().wine_prefix("x")
        );
    }

    #[test]
    fn prefix_dir_structure() {
        let tmp = TempDir::new().unwrap();
        let mgr = PrefixManager::with_base(tmp.path().to_path_buf());

        let dir = mgr.prefix_dir("abc123");
        assert!(dir.ends_with("abc123"));

        let wine = mgr.wine_prefix("abc123");
        assert!(wine.ends_with("abc123/prefix"));

        let state = mgr.state_path("abc123");
        assert!(state.ends_with("abc123/state.json"));
    }

    #[test]
    fn state_round_trip() {
        let tmp = TempDir::new().unwrap();
        let mgr = PrefixManager::with_base(tmp.path().to_path_buf());

        let hash = "test_hash";
        std::fs::create_dir_all(mgr.prefix_dir(hash)).unwrap();

        let state = PrefixState {
            dxvk_installed: true,
            installed_components: vec!["dotnet48".into(), "vcrun2019".into()],
            wine_version: "wine-9.0".into(),
            last_setup: Some("1234567890".into()),
        };

        mgr.save_state(hash, &state).unwrap();
        let loaded = mgr.load_state(hash);
        assert!(loaded.dxvk_installed);
        assert_eq!(loaded.installed_components.len(), 2);
        assert!(loaded.installed_components.contains(&"dotnet48".into()));
    }

    #[test]
    fn check_deps_reports_missing() {
        let tmp = TempDir::new().unwrap();
        let mgr = PrefixManager::with_base(tmp.path().to_path_buf());

        let hash = "check_hash";
        std::fs::create_dir_all(mgr.prefix_dir(hash)).unwrap();

        // Nothing installed yet
        let (dxvk, wt) = mgr.check_deps(hash, true, &["dotnet48".into(), "vcrun2019".into()]);
        assert!(dxvk);
        assert_eq!(wt.len(), 2);

        // Mark DXVK as installed
        let state = PrefixState {
            dxvk_installed: true,
            ..Default::default()
        };
        mgr.save_state(hash, &state).unwrap();

        let (dxvk, wt) = mgr.check_deps(hash, true, &["dotnet48".into()]);
        assert!(!dxvk); // DXVK done
        assert_eq!(wt, vec!["dotnet48"]); // winetricks still missing
    }

    #[test]
    fn ensure_prefix_creates_directory() {
        let tmp = TempDir::new().unwrap();
        let mgr = PrefixManager::with_base(tmp.path().to_path_buf());

        let hash = "new_prefix";
        assert!(!mgr.wine_prefix(hash).exists());

        // ensure_prefix will try to run wineboot, which may fail in test env
        // but it should still create the directory
        let _ = mgr.ensure_prefix(hash, None);
        assert!(mgr.wine_prefix(hash).exists());
    }

    #[test]
    fn empty_winetricks_is_noop() {
        let tmp = TempDir::new().unwrap();
        let mgr = PrefixManager::with_base(tmp.path().to_path_buf());

        let hash = "empty_wt";
        std::fs::create_dir_all(mgr.prefix_dir(hash)).unwrap();

        // Should succeed without doing anything
        mgr.install_winetricks(hash, &[]).unwrap();
    }
}
