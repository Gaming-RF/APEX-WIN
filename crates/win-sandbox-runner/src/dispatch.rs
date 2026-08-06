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
pub fn execute(args: &Args, hash: &str, rules: &RulesFile) -> Result<ExitCode> {
    // Determine tier
    let tier = if let Some(ref tier_str) = args.tier {
        let t: Tier = tier_str.parse()?;
        info!("Forced tier: {t}");
        t
    } else if let Some(entry) = rules::lookup_by_hash(rules, hash) {
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

    // Determine network permission
    let network = resolve_network_permission(rules, hash);
    info!("Network access: {network}");

    if args.dry_run {
        info!("[DRY RUN] Would execute tier {tier} for {} (network={network})", args.exe);
        return Ok(ExitCode::SUCCESS);
    }

    info!("Executing tier {tier} for {}", args.exe);

    match tier {
        Tier::Tier0 => crate::tier0::run(args),
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
        use win_sandbox_common::rules_schema::{RuleDefaults, RuleEntry};
        use win_sandbox_common::tier::Tier;

        let rules = RulesFile {
            version: 1,
            entries: vec![RuleEntry {
                hash: "abc123".into(),
                name: "test".into(),
                tier: Tier::Tier2,
                allowed_paths: vec![],
                network: true,
                gpu: false,
            }],
            defaults: RuleDefaults {
                unmapped_tier: Tier::Tier0,
                untrusted_path_tier: Tier::Tier2,
                network_default: false,
                gpu_default: false,
            },
        };

        assert!(resolve_network_permission(&rules, "abc123"));
        assert!(!resolve_network_permission(&rules, "unknown"));
    }
}
