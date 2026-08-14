//! Detects sandbox capabilities actually available on this machine.
//!
//! `rules.json` can request Tier 3 (OverlayFS ephemeral isolation), but
//! whether that promise can actually be kept depends on the host: `mount(8)`
//! needs root for overlay mounts, and bubblewrap only gained unprivileged
//! `--overlay` in 0.10. This module is the single place that answers "is
//! Tier 3 real here", so dispatch's fail-secure check and `--status`'s
//! capability report cannot silently disagree.

use std::process::Command;
use tracing::debug;

/// What this host can actually provide for each isolation tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// Highest Landlock ABI enforced, if any (Tier 1 depends on this).
    pub landlock_abi: Option<u8>,
    /// bubblewrap version string as reported by `bwrap --version`, if found.
    pub bwrap_version: Option<String>,
    /// Whether bubblewrap can create an unprivileged OverlayFS mount
    /// (`--overlay`, added in bubblewrap 0.10). Tier 3's ephemeral-overlay
    /// promise is only real when this is true; otherwise Tier 3 degrades to
    /// Tier 2 isolation, which is a materially weaker guarantee.
    pub unprivileged_overlay: bool,
}

impl Capabilities {
    /// Probe the host. Cheap enough to call once at daemon startup and once
    /// per `--status` request; nothing here talks to the network or blocks
    /// meaningfully.
    pub fn detect() -> Self {
        Self {
            landlock_abi: detect_landlock_abi_level(),
            bwrap_version: detect_bwrap_version(),
            unprivileged_overlay: detect_unprivileged_overlay(),
        }
    }

    /// Whether a Tier 3 request can be honored as real ephemeral-overlay
    /// isolation on this host, as opposed to silently becoming Tier 2.
    pub fn tier3_available(&self) -> bool {
        self.unprivileged_overlay
    }
}

fn detect_landlock_abi_level() -> Option<u8> {
    // Delegates to tier1's probe rather than reimplementing it. An earlier
    // version of this function called `Ruleset::create()` directly, on the
    // assumption that `handle_access()` alone might not reflect the real
    // kernel. Empirically (this session, kernel 7.0) `.create()` agreed with
    // `handle_access()` for every ABI level, so the extra syscall bought
    // nothing — and a second copy of the probe is exactly the kind of
    // duplication that caused the binfmt mask bug to recur three times.
    crate::tier1::detect_landlock_abi()
        .ok()
        .map(|abi| abi as u8)
}

fn detect_bwrap_version() -> Option<String> {
    let output = Command::new("bwrap").arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    // "bubblewrap 0.9.0\n" -> "0.9.0"
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .last()
        .map(str::to_string)
}

/// Parse a bubblewrap version string and check it is >= 0.10.0, the first
/// release with unprivileged `--overlay` support.
fn version_supports_overlay(version: &str) -> bool {
    let parts: Vec<u32> = version.split('.').filter_map(|p| p.parse().ok()).collect();
    match parts.as_slice() {
        [major, minor, ..] => *major > 0 || *minor >= 10,
        _ => false,
    }
}

fn detect_unprivileged_overlay() -> bool {
    match detect_bwrap_version() {
        Some(v) => {
            let supported = version_supports_overlay(&v);
            if !supported {
                debug!("bwrap {v} does not support unprivileged --overlay (needs >= 0.10)");
            }
            supported
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parsing_below_threshold() {
        assert!(!version_supports_overlay("0.9.0"));
        assert!(!version_supports_overlay("0.4.1"));
        assert!(!version_supports_overlay("0.9.99"));
    }

    #[test]
    fn version_parsing_at_or_above_threshold() {
        assert!(version_supports_overlay("0.10.0"));
        assert!(version_supports_overlay("0.11.2"));
        assert!(version_supports_overlay("1.0.0"));
        assert!(version_supports_overlay("2.3.4"));
    }

    #[test]
    fn version_parsing_handles_garbage() {
        assert!(!version_supports_overlay(""));
        assert!(!version_supports_overlay("not-a-version"));
    }

    /// Real probe against whatever is installed on the machine running the
    /// test. Documents the actual constraint this project has hit: Zorin
    /// 18.1 ships bubblewrap 0.9.0, so Tier 3 cannot provide a real overlay
    /// there. This test doesn't assert a fixed outcome (CI's bwrap version
    /// may differ) — it asserts detection doesn't panic and is internally
    /// consistent with the version string it found.
    #[test]
    fn detect_matches_installed_bwrap() {
        let caps = Capabilities::detect();
        match &caps.bwrap_version {
            Some(v) => {
                assert_eq!(caps.unprivileged_overlay, version_supports_overlay(v));
            }
            None => {
                assert!(!caps.unprivileged_overlay, "no bwrap means no overlay");
            }
        }
    }
}
