use crate::Args;
use anyhow::Result;
use std::process::ExitCode;
use tracing::{info, warn};
use win_sandbox_common::rules_schema::RulesFile;
use win_sandbox_common::tier::Tier;

use crate::{appdb, rules, wizard};

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
/// Full resolution flow:
///   1. Look up by hash in rules.json (exact match)
///   2. Look up by exe name in app database (fuzzy match)
///   3. Run first-launch wizard (auto-detect heuristics)
///   4. Ensure Wine prefix exists and install deps
///   5. If trusted, run wine directly (no sandbox)
///   6. Otherwise, resolve tier and execute in sandbox
pub fn execute(
    args: &Args,
    exe: &str,
    hash: &str,
    rules: &RulesFile,
    app_db: &appdb::AppDatabase,
) -> Result<ExitCode> {
    // --- Step 1: Exact hash match in rules.json ---
    let mut matched_entry: Option<win_sandbox_common::rules_schema::RuleEntry> =
        rules::lookup_by_hash(rules, hash).cloned();

    // --- Step 2: Name-based match in app database ---
    if matched_entry.is_none() {
        if let Some((profile, entry)) = app_db.lookup_by_name(exe) {
            info!(
                "App database match: '{}' -> '{}'",
                std::path::Path::new(exe)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("?"),
                profile.name
            );
            if !profile.notes.is_empty() {
                info!("  Note: {}", profile.notes);
            }
            matched_entry = Some(entry);
        }
    }

    // --- Step 3: First-launch wizard for unknown apps ---
    if matched_entry.is_none() {
        let result = wizard::run_wizard(exe, app_db, args.no_gui, hash);
        info!("First launch: {}", wizard::describe_decision(&result));
        matched_entry = Some(result.entry);
    }

    // --- Step 4: Per-app prefix management ---
    if let Some(ref entry) = matched_entry {
        let prefix_mgr = crate::prefix::PrefixManager::new();
        let wine_prefix = prefix_mgr.setup_app(hash, entry.dxvk, &entry.winetricks, args.uid)?;
        std::env::set_var("WINEPREFIX", &wine_prefix);
        info!("WINEPREFIX: {}", wine_prefix.display());
    }

    // --- Step 5: Trusted apps — no sandboxing ---
    if let Some(ref entry) = matched_entry {
        if entry.trusted {
            info!("Trusted app '{}', no sandboxing", entry.name);

            if args.dry_run {
                info!("[DRY RUN] Would run trusted '{}' with wine directly", entry.name);
                return Ok(ExitCode::SUCCESS);
            }

            return crate::tier0::run_with_env(args, &entry.env);
        }
    }

    // --- Step 6: Resolve tier ---
    let tier = if let Some(ref tier_str) = args.tier {
        let t: Tier = tier_str.parse()?;
        info!("Forced tier: {t}");
        t
    } else if let Some(ref entry) = matched_entry {
        info!("Matched rule '{}', tier: {}", entry.name, entry.tier);
        entry.tier
    } else if is_untrusted_path(exe) {
        let t = rules.defaults.untrusted_path_tier;
        warn!("Untrusted path '{}', using tier {t}", exe);
        t
    } else {
        let t = rules.defaults.unmapped_tier;
        info!("No rule matched, using default tier {t}");
        t
    };

    let network = resolve_network_permission(rules, hash);
    info!("Network access: {network}");

    if args.dry_run {
        info!(
            "[DRY RUN] Would execute tier {tier} for {exe} (network={network})",
        );
        return Ok(ExitCode::SUCCESS);
    }

    // Nvidia + user namespaces: downgrade Tier 2 to Tier 1
    let tier = if tier == Tier::Tier2 && crate::nvidia::detect().is_some() {
        warn!("Nvidia GPU detected — downgrading Tier 2 to Tier 1");
        Tier::Tier1
    } else {
        tier
    };

    let app_env: std::collections::HashMap<String, String> = matched_entry
        .as_ref()
        .map(|e| e.env.clone())
        .unwrap_or_default();

    // Merge user env (from daemon FIFO) with app env
    let mut merged_env = app_env;
    for (k, v) in &args.user_env {
        merged_env.entry(k.clone()).or_insert_with(|| v.clone());
    }

    info!("Executing tier {tier} for {exe}");

    match tier {
        Tier::Tier0 => crate::tier0::run_with_env(args, &merged_env),
        Tier::Tier1 => crate::tier1::run(args),
        Tier::Tier2 => crate::tier2::run_with_network(args, network),
        Tier::Tier3 => crate::tier3::run_with_network(args, network),
    }
}

/// Pass through directly to wine (used when recursion guard is active).
pub fn passthrough_to_wine(exe: &str, args: &[String]) -> ExitCode {
    use std::process::Command;
    let status = Command::new("wine").arg(exe).args(args).status();
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
    }
}
