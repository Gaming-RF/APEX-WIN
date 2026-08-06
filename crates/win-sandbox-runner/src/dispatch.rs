use crate::Args;
use anyhow::Result;
use std::process::ExitCode;
use tracing::{info, warn};
use win_sandbox_common::rules_schema::RulesFile;
use win_sandbox_common::tier::Tier;

use crate::rules;

/// Check if a path is in an untrusted location.
pub fn is_untrusted_path(path: &str) -> bool {
    let untrusted = ["/tmp", "/mnt", "/media", "/var/tmp", "/dev/shm"];
    untrusted.iter().any(|prefix| path.starts_with(prefix))
}

/// Resolve whether network access is allowed for this binary.
fn resolve_network_permission(rules: &RulesFile, hash: &str) -> bool {
    if let Some(entry) = rules::lookup_by_hash(rules, hash) {
        entry.network
    } else {
        rules.defaults.network_default
    }
}

/// Execute the appropriate tier for the given binary.
///
/// Decision flow:
///   1. Look up rule by hash
///   2. If matched, ensure Wine prefix exists and install deps (DXVK, winetricks)
///   3. If `trusted: true`, run wine directly (no sandbox)
///   4. Otherwise, resolve tier and execute in sandbox
pub fn execute(args: &Args, hash: &str, rules: &RulesFile) -> Result<ExitCode> {
    let matched_entry = rules::lookup_by_hash(rules, hash);

    // --- Per-app prefix management for matched rules ---
    if let Some(entry) = &matched_entry {
        let prefix_mgr = crate::prefix::PrefixManager::new();

        // Ensure prefix exists and install any missing deps
        let wine_prefix = prefix_mgr.setup_app(
            hash,
            entry.dxvk,
            &entry.winetricks,
        )?;

        // Set WINEPREFIX for all subsequent wine calls
        std::env::set_var("WINEPREFIX", &wine_prefix);
        info!("WINEPREFIX: {}", wine_prefix.display());
    }

    // --- Trusted apps: no sandboxing, just run wine directly ---
    if let Some(entry) = &matched_entry {
        if entry.trusted {
            info!("Trusted app '{}', no sandboxing", entry.name);

            if args.dry_run {
                info!("[DRY RUN] Would run trusted '{}' with wine directly", entry.name);
                return Ok(ExitCode::SUCCESS);
            }

            return crate::tier0::run_with_env(args, &entry.env);
        }
    }

    // --- Resolve tier ---
    let tier = if let Some(ref tier_str) = args.tier {
        let t: Tier = tier_str.parse()?;
        info!("Forced tier: {t}");
        t
    } else if let Some(entry) = matched_entry {
        info!("Matched rule '{}', tier: {}", entry.name, entry.tier);
        entry.tier
    } else if is_untrusted_path(&args.exe) {
        let t = rules.defaults.untrusted_path_tier;
        warn!("Untrusted path '{}', using tier {t}", args.exe);
        t
    } else {
        let t = rules.defaults.unmapped_tier;
        info!("No rule matched, using default tier {t}");
        t
    };

    // Resolve network
    let network = resolve_network_permission(rules, hash);
    info!("Network access: {network}");

    if args.dry_run {
        info!("[DRY RUN] Would execute tier {tier} for {} (network={network})", args.exe);
        return Ok(ExitCode::SUCCESS);
    }

    // Nvidia + user namespaces: downgrade Tier 2 to Tier 1 (plan §11 edge case)
    let tier = if tier == Tier::Tier2 && crate::nvidia::detect().is_some() {
        warn!("Nvidia GPU detected — downgrading Tier 2 (Bubblewrap) to Tier 1 (Landlock)");
        warn!("Bubblewrap user namespaces can break Nvidia VK initialization");
        Tier::Tier1
    } else {
        tier
    };

    // Collect app-specific env vars for tier 0
    let app_env: std::collections::HashMap<String, String> = matched_entry
        .as_ref()
        .map(|e| e.env.clone())
        .unwrap_or_default();

    info!("Executing tier {tier} for {}", args.exe);

    match tier {
        Tier::Tier0 => crate::tier0::run_with_env(args, &app_env),
        Tier::Tier1 => crate::tier1::run(args),
        Tier::Tier2 => crate::tier2::run_with_network(args, network),
        Tier::Tier3 => crate::tier3::run_with_network(args, network),
    }
}

/// Pass through directly to wine (used when recursion guard is active).
pub fn passthrough_to_wine(exe: &str, args: &[String]) -> ExitCode {
    use std::process::Command;
    let status = Command::new("wine")
        .arg(exe)
        .args(args)
        .status();
    match status {
        Ok(s) => {
            if s.success() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(s.code().unwrap_or(1) as u8)
            }
        }
        Err(e) => {
            tracing::error!("Failed to exec wine: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use win_sandbox_common::rules_schema::{RuleDefaults, RuleEntry};

    fn make_entry(hash: &str, tier: Tier, network: bool, trusted: bool) -> RuleEntry {
        RuleEntry {
            hash: hash.into(),
            name: "test".into(),
            tier,
            allowed_paths: vec![],
            network,
            gpu: false,
            trusted,
            dxvk: false,
            winetricks: vec![],
            env: std::collections::HashMap::new(),
            wine_variant: "system".into(),
        }
    }

    #[test]
    fn untrusted_paths_detected() {
        assert!(is_untrusted_path("/tmp/foo.exe"));
        assert!(is_untrusted_path("/mnt/usb/game.exe"));
        assert!(is_untrusted_path("/media/cdrom/setup.exe"));
        assert!(is_untrusted_path("/var/tmp/test.exe"));
        assert!(!is_untrusted_path("/home/user/game.exe"));
        assert!(!is_untrusted_path("/opt/wine-prefix/drive_c/app.exe"));
    }

    #[test]
    fn resolve_network_from_rules() {
        let rules = RulesFile {
            version: 1,
            entries: vec![make_entry("abc123", Tier::Tier2, true, false)],
            defaults: RuleDefaults::default(),
        };

        assert!(resolve_network_permission(&rules, "abc123"));
        assert!(!resolve_network_permission(&rules, "unknown"));
    }

    #[test]
    fn trusted_flag_skips_sandbox() {
        let entry = make_entry("abc", Tier::Tier0, true, true);
        assert!(entry.trusted);

        let entry = make_entry("abc", Tier::Tier2, false, false);
        assert!(!entry.trusted);
    }

    #[test]
    fn trusted_with_custom_env() {
        let mut env = std::collections::HashMap::new();
        env.insert("DXVK_HUD".into(), "1".into());
        env.insert("MESA_GL_VERSION_OVERRIDE".into(), "4.5".into());

        let entry = RuleEntry {
            hash: "abc".into(),
            name: "fusion360".into(),
            tier: Tier::Tier0,
            allowed_paths: vec![],
            network: true,
            gpu: true,
            trusted: true,
            dxvk: false,
            winetricks: vec![],
            env,
            wine_variant: "proton".into(),
        };

        assert!(entry.trusted);
        assert_eq!(entry.env.get("DXVK_HUD").unwrap(), "1");
        assert_eq!(entry.wine_variant, "proton");
    }
}
