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
    /// Linux-only: always `None` on other platforms, since Landlock is a
    /// Linux LSM with no equivalent elsewhere.
    pub landlock_abi: Option<u8>,
    /// bubblewrap version string as reported by `bwrap --version`, if found.
    /// Linux-only in practice: bubblewrap is not packaged for macOS.
    pub bwrap_version: Option<String>,
    /// Whether bubblewrap can create an unprivileged OverlayFS mount
    /// (`--overlay`, added in bubblewrap 0.10). Tier 3's ephemeral-overlay
    /// promise is only real when this is true; otherwise Tier 3 degrades to
    /// Tier 2 isolation, which is a materially weaker guarantee.
    pub unprivileged_overlay: bool,
    /// macOS only: whether `sandbox-exec` (Seatbelt) is present on this
    /// host. `None` on non-macOS platforms, where the field does not apply
    /// — deliberately not defaulted to `Some(false)`, so a caller can tell
    /// "not macOS" apart from "macOS but sandbox-exec missing".
    ///
    /// `sandbox-exec` is deprecated by Apple and its profile language is
    /// undocumented for third-party use (see HANDOFF.md), so even when this
    /// is `true` it is a best-effort filesystem restriction, not a Landlock-
    /// or bubblewrap-equivalent security boundary. Tier 1/2 on macOS report
    /// this honestly rather than claiming an isolation level they cannot
    /// back up the way the Linux tiers can.
    pub seatbelt_available: Option<bool>,
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
            seatbelt_available: detect_seatbelt_available(),
        }
    }

    /// Whether a Tier 3 request can be honored as real ephemeral-overlay
    /// isolation on this host, as opposed to silently becoming Tier 2.
    pub fn tier3_available(&self) -> bool {
        self.unprivileged_overlay
    }

    /// Whether Tier 1/2 (filesystem-restriction isolation) can be honored on
    /// this host at all. On Linux this means Landlock or bubblewrap
    /// respectively — callers should check `landlock_abi`/`bwrap_version`
    /// directly for which. On macOS it means `sandbox-exec` is present; see
    /// the caveat on [`Capabilities::seatbelt_available`] about what that
    /// isolation is actually worth.
    pub fn tier12_available(&self) -> bool {
        self.landlock_abi.is_some()
            || self.bwrap_version.is_some()
            || self.seatbelt_available == Some(true)
    }
}

/// macOS only: detect whether `sandbox-exec` is present. Always `None` on
/// other platforms — see [`Capabilities::seatbelt_available`].
#[cfg(target_os = "macos")]
fn detect_seatbelt_available() -> Option<bool> {
    Some(
        Command::new("sandbox-exec")
            .arg("-n")
            .arg("no-network")
            .arg("/usr/bin/true")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false),
    )
}

#[cfg(not(target_os = "macos"))]
fn detect_seatbelt_available() -> Option<bool> {
    None
}

#[cfg(target_os = "linux")]
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

/// Landlock is a Linux LSM; there is nothing to probe on other platforms.
#[cfg(not(target_os = "linux"))]
fn detect_landlock_abi_level() -> Option<u8> {
    None
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
