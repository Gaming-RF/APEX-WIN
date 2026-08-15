use crate::Args;
use anyhow::{bail, Result};
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

/// Resolve which tier to run at, and whether that came from an explicit,
/// user-authored source rather than a heuristic default.
///
/// Explicit sources: the `--tier` CLI flag, or a hash the user (or `--trust`)
/// put in `rules.json` themselves. Heuristic sources: an app-database match
/// (APEX-WIN's own profile for a known app), the wizard's auto-detect
/// heuristics, the untrusted-path fallback, or the unmapped-binary fallback.
/// Only the explicit path is a promise the user actually made, which is what
/// the fail-secure Tier 3 check in `execute()` depends on.
///
/// Extracted from `execute()` so this resolution logic — the part most worth
/// getting right — is testable without standing up a Wine prefix.
fn resolve_tier(
    args: &Args,
    exe: &str,
    rules: &RulesFile,
    explicit_entry: &Option<win_sandbox_common::rules_schema::RuleEntry>,
    matched_entry: &Option<win_sandbox_common::rules_schema::RuleEntry>,
) -> Result<(Tier, bool)> {
    if let Some(ref tier_str) = args.tier {
        let t: Tier = tier_str.parse()?;
        info!("Forced tier: {t}");
        return Ok((t, true));
    }
    if let Some(ref entry) = explicit_entry {
        info!("Matched rule '{}', tier: {}", entry.name, entry.tier);
        return Ok((entry.tier, true));
    }
    if let Some(ref entry) = matched_entry {
        info!(
            "Matched app-database/heuristic entry '{}', tier: {}",
            entry.name, entry.tier
        );
        return Ok((entry.tier, false));
    }
    if is_untrusted_path(exe) {
        let t = rules.defaults.untrusted_path_tier;
        warn!("Untrusted path '{}', using tier {t}", exe);
        return Ok((t, false));
    }
    let t = rules.defaults.unmapped_tier;
    info!("No rule matched, using default tier {t}");
    Ok((t, false))
}

/// Check whether an explicit Tier 3 request can be honored as real
/// ephemeral-overlay isolation. `Err` carries the human-readable reason it
/// cannot, for both the refusal message and the `--dry-run` report.
fn check_tier3_available(
    tier3_available: bool,
    bwrap_version: &Option<String>,
) -> Result<(), String> {
    if tier3_available {
        return Ok(());
    }
    Err(match bwrap_version {
        Some(v) => format!(
            "bubblewrap {v} does not support unprivileged overlay mounts \
             (needs >= 0.10.0), and OverlayFS via mount(8) requires root"
        ),
        None => "bubblewrap not found, and OverlayFS via mount(8) requires root".to_string(),
    })
}

/// Check whether Tier 1/2/3 exist as sandboxing mechanisms *at all* on this
/// platform, independent of whether a specific one (like Tier 3's overlay)
/// is currently configured correctly. This is the platform-support gate;
/// `check_tier3_available` is the finer-grained Linux capability gate that
/// runs after this one passes.
///
/// Kept as its own function (not folded into `check_tier3_available`) so the
/// existing, already-tested Tier 3 overlay logic is untouched: that check
/// answers "is overlay isolation configured on this Linux host", this one
/// answers "does Tier N exist on this OS at all".
fn check_tier_implemented(
    tier: Tier,
    caps: &crate::capabilities::Capabilities,
) -> Result<(), String> {
    check_tier_implemented_for_os(tier, caps, cfg!(target_os = "linux"), std::env::consts::OS)
}

/// `check_tier_implemented`'s actual logic, with the "which OS is this"
/// question taken as a parameter instead of read via `cfg!`/`env::consts`
/// directly. Both branches need real, deterministic test coverage (this is
/// the fail-secure gate that stands between an explicit `--tier 1/2/3`
/// request and running unsandboxed on a platform that can't provide it) but
/// a plain `cfg!(target_os = "linux")` bakes the answer in at compile time,
/// so a unit test built on Linux could never exercise the non-Linux branch,
/// and one built on macOS could never exercise the Linux branch. Splitting
/// the OS lookup out as a parameter makes both reachable from any host.
fn check_tier_implemented_for_os(
    tier: Tier,
    caps: &crate::capabilities::Capabilities,
    is_linux: bool,
    os_name: &str,
) -> Result<(), String> {
    if tier == Tier::Tier0 {
        return Ok(()); // Tier 0 (direct Wine exec) has no OS-specific sandbox.
    }
    if is_linux {
        return Ok(()); // Tier 1/2/3 all have Linux implementations.
    }
    // Non-Linux: tier1.rs/tier2.rs/tier3.rs are Linux-only modules and do
    // not exist in this build at all, regardless of what capabilities.rs
    // detects (Seatbelt availability does not currently back a real Tier
    // 1/2 implementation — see HANDOFF.md for the macOS isolation gap).
    Err(match tier {
        Tier::Tier2 | Tier::Tier3 => format!(
            "Tier {} needs Landlock/bubblewrap/OverlayFS, none of which exist on {os_name}",
            tier.level()
        ),
        Tier::Tier1 => format!(
            "Tier 1 needs Landlock, which does not exist on {os_name}{}",
            if caps.seatbelt_available == Some(true) {
                " (sandbox-exec is present, but APEX-WIN does not yet implement a Tier \
                  1 sandbox using it — see HANDOFF.md)"
            } else {
                ""
            }
        ),
        Tier::Tier0 => unreachable!("handled above"),
    })
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
    // This is the only path that counts as an *explicit, user-authored*
    // decision: the user (or --trust) put this hash in rules.json themselves.
    // App-database matches and wizard heuristics are APEX-WIN's own guesses,
    // not a promise the user made — that distinction is what fail-secure
    // Tier 3 handling below depends on.
    let explicit_entry = rules::lookup_by_hash(rules, hash).cloned();
    let mut matched_entry: Option<win_sandbox_common::rules_schema::RuleEntry> =
        explicit_entry.clone();

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
        // Root the prefix at the *invoking user's* home. In daemon mode the
        // process runs as root, where HOME is /root or unset, so relying on
        // the ambient environment would create an unusable prefix.
        let prefix_mgr = crate::prefix::PrefixManager::for_user(&args.user_env);
        let wine_prefix = prefix_mgr.setup_app(hash, entry.dxvk, &entry.winetricks, args.uid)?;
        std::env::set_var("WINEPREFIX", &wine_prefix);
        info!("WINEPREFIX: {}", wine_prefix.display());
    }

    // --- Step 5: Trusted apps — no sandboxing ---
    if let Some(ref entry) = matched_entry {
        if entry.trusted {
            info!("Trusted app '{}', no sandboxing", entry.name);

            if args.dry_run {
                info!(
                    "[DRY RUN] Would run trusted '{}' with wine directly",
                    entry.name
                );
                return Ok(ExitCode::SUCCESS);
            }

            return crate::tier0::run_with_env(args, &entry.env);
        }
    }

    // --- Step 6: Resolve tier ---
    let (tier, tier_is_explicit) = resolve_tier(args, exe, rules, &explicit_entry, &matched_entry)?;

    // One capability probe, reused by both gates below: the cross-platform
    // "does Tier N exist here" check and the Linux-specific "is Tier 3's
    // overlay actually configured" check.
    let caps = crate::capabilities::Capabilities::detect();

    // Cross-platform gate: does Tier N exist as a sandbox mechanism on this
    // OS at all? On Linux this always passes (all four tiers are
    // implemented) and falls through to the finer-grained Tier 3 overlay
    // check below. On other platforms (currently: macOS build targets with
    // no tier1/tier2/tier3 modules), only Tier 0 exists.
    //
    // Same fail-secure split as Tier 3's overlay check: an EXPLICIT request
    // for a tier this OS cannot provide refuses, so a security decision the
    // user actually made is never silently weakened. A HEURISTIC match
    // (app-database/wizard/path defaults) degrades to Tier 0 with a loud
    // warning instead — direct execution, no sandboxing at all — since that
    // was never a promise the user made, and Tier 0 is strictly less
    // isolated than any Tier 1/2/3 the heuristic intended, not a silent
    // downgrade to a similar-but-weaker tier the way Tier3->Tier2 is.
    let tier = if let Err(reason) = check_tier_implemented(tier, &caps) {
        if tier_is_explicit {
            if args.dry_run {
                info!(
                    "[DRY RUN] Tier {} was explicitly requested for {exe} but is not \
                     available on this platform: {reason}. A real run would refuse.",
                    tier.level()
                );
                return Ok(ExitCode::SUCCESS);
            }
            bail!(
                "Refusing to run '{exe}': Tier {} was explicitly requested but is not \
                 available on this platform ({reason}). Use --tier 0 to run without a \
                 sandbox, or run this on Linux for real Tier 1/2/3 isolation.",
                tier.level()
            );
        }
        warn!(
            "Tier {} was suggested by a heuristic but is not available on this platform \
             ({reason}). Falling back to Tier 0 — direct execution, no sandboxing. This \
             app is not isolated from the rest of your system.",
            tier.level()
        );
        Tier::Tier0
    } else {
        tier
    };

    // Fail secure: an explicit Tier 3 request must get real ephemeral-overlay
    // isolation or be refused, not silently served as Tier 2. Tier 2 and
    // Tier 3 have different threat models (persistent bwrap namespace vs.
    // OverlayFS changes discarded on exit); an app the user explicitly
    // pinned to Tier 3 may be relying on that guarantee.
    if tier == Tier::Tier3 && tier_is_explicit {
        if let Err(reason) = check_tier3_available(caps.tier3_available(), &caps.bwrap_version) {
            if args.dry_run {
                info!(
                    "[DRY RUN] Tier 3 was explicitly requested for {exe} but is not available: \
                     {reason}. A real run would refuse rather than silently use Tier 2."
                );
                return Ok(ExitCode::SUCCESS);
            }
            bail!(
                "Refusing to run '{exe}': Tier 3 was explicitly requested but this host cannot \
                 provide it ({reason}). Serving it as Tier 2 would silently weaken an isolation \
                 guarantee you asked for. Use --tier 2 to accept that explicitly, or install \
                 bubblewrap >= 0.10 for real Tier 3 support."
            );
        }
    }

    let network = resolve_network_permission(rules, hash);
    info!("Network access: {network}");

    if args.dry_run {
        info!("[DRY RUN] Would execute tier {tier} for {exe} (network={network})",);
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
        #[cfg(target_os = "linux")]
        Tier::Tier1 => crate::tier1::run(args),
        #[cfg(target_os = "linux")]
        Tier::Tier2 => crate::tier2::run_with_network(args, network),
        #[cfg(target_os = "linux")]
        Tier::Tier3 => crate::tier3::run_with_network(args, network),
        // On non-Linux platforms, `check_tier_implemented` above already
        // forced `tier` down to Tier0 (refusing outright for an explicit
        // request, or downgrading a heuristic one with a warning) before
        // execution ever reaches this match. Reaching Tier1/2/3 here would
        // mean that gate has a bug, not that this is a legitimate case to
        // silently handle — hence bail! rather than a silent no-op.
        #[cfg(not(target_os = "linux"))]
        Tier::Tier1 | Tier::Tier2 | Tier::Tier3 => bail!(
            "internal error: reached Tier {} dispatch on a platform without a Tier 1/2/3 \
             implementation; check_tier_implemented() should have refused or downgraded \
             this before now",
            tier.level()
        ),
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

    /// Minimal `Args` for tests that only exercise tier resolution, following
    /// the same literal pattern used in tier1.rs's tests (no `Default` impl
    /// exists on `Args` because clap derives its own construction).
    fn test_args(tier: Option<&str>) -> Args {
        Args {
            exe: Some("/home/test/game.exe".into()),
            tier: tier.map(String::from),
            rules: None,
            verbose: false,
            no_gui: true,
            dry_run: false,
            gamepad: false,
            nested_x11: false,
            xvfb: false,
            host_x11: false,
            wayland: false,
            args: vec![],
            trust: false,
            optimize_net: false,
            cleanup_net: false,
            configure_net: false,
            daemon: false,
            status: false,
            reload: false,
            stop: false,
            unregister: false,
            user_env: std::collections::HashMap::new(),
            uid: None,
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

    // --- resolve_tier: explicit vs heuristic provenance ---
    //
    // This is the behavior the fail-secure Tier 3 check depends on entirely:
    // it only refuses when `tier_is_explicit` is true. Every branch of
    // resolve_tier needs its own test to pin which ones set that flag.

    #[test]
    fn resolve_tier_cli_flag_is_explicit() {
        let args = test_args(Some("3"));
        let rules = RulesFile {
            version: 1,
            entries: vec![],
            defaults: RuleDefaults::default(),
        };
        let (tier, explicit) = resolve_tier(&args, "/home/u/x.exe", &rules, &None, &None).unwrap();
        assert_eq!(tier, Tier::Tier3);
        assert!(explicit, "--tier flag must count as an explicit request");
    }

    #[test]
    fn resolve_tier_explicit_rule_is_explicit() {
        let args = test_args(None);
        let rules = RulesFile {
            version: 1,
            entries: vec![],
            defaults: RuleDefaults::default(),
        };
        let entry = make_entry("h1", Tier::Tier3, false, false);
        let (tier, explicit) = resolve_tier(
            &args,
            "/home/u/x.exe",
            &rules,
            &Some(entry.clone()),
            &Some(entry),
        )
        .unwrap();
        assert_eq!(tier, Tier::Tier3);
        assert!(
            explicit,
            "a hash the user put in rules.json is an explicit promise"
        );
    }

    #[test]
    fn resolve_tier_app_database_match_is_not_explicit() {
        // matched_entry set (app-database/wizard match) but explicit_entry is
        // None: this is APEX-WIN's own guess, not something the user typed.
        let args = test_args(None);
        let rules = RulesFile {
            version: 1,
            entries: vec![],
            defaults: RuleDefaults::default(),
        };
        let heuristic_entry = make_entry("h1", Tier::Tier3, false, false);
        let (tier, explicit) = resolve_tier(
            &args,
            "/home/u/x.exe",
            &rules,
            &None,
            &Some(heuristic_entry),
        )
        .unwrap();
        assert_eq!(tier, Tier::Tier3);
        assert!(
            !explicit,
            "an app-database/wizard match must NOT count as explicit, even at tier 3"
        );
    }

    #[test]
    fn resolve_tier_untrusted_path_fallback_is_not_explicit() {
        let args = test_args(None);
        let rules = RulesFile {
            version: 1,
            entries: vec![],
            defaults: RuleDefaults {
                unmapped_tier: Tier::Tier0,
                untrusted_path_tier: Tier::Tier3,
                network_default: false,
                gpu_default: false,
            },
        };
        let (tier, explicit) =
            resolve_tier(&args, "/tmp/unknown.exe", &rules, &None, &None).unwrap();
        assert_eq!(tier, Tier::Tier3);
        assert!(
            !explicit,
            "the untrusted-path default is a heuristic, not a promise"
        );
    }

    #[test]
    fn resolve_tier_unmapped_fallback_is_not_explicit() {
        let args = test_args(None);
        let rules = RulesFile {
            version: 1,
            entries: vec![],
            defaults: RuleDefaults {
                unmapped_tier: Tier::Tier3,
                untrusted_path_tier: Tier::Tier2,
                network_default: false,
                gpu_default: false,
            },
        };
        let (tier, explicit) =
            resolve_tier(&args, "/home/u/unknown.exe", &rules, &None, &None).unwrap();
        assert_eq!(tier, Tier::Tier3);
        assert!(
            !explicit,
            "the unmapped-binary default is a heuristic, not a promise"
        );
    }

    #[test]
    fn resolve_tier_invalid_cli_flag_errors() {
        let args = test_args(Some("not-a-tier"));
        let rules = RulesFile {
            version: 1,
            entries: vec![],
            defaults: RuleDefaults::default(),
        };
        assert!(resolve_tier(&args, "/home/u/x.exe", &rules, &None, &None).is_err());
    }

    // --- check_tier3_available: the actual fail-secure gate ---

    #[test]
    fn tier3_available_when_capability_present() {
        assert!(check_tier3_available(true, &Some("0.10.0".into())).is_ok());
    }

    #[test]
    fn tier3_unavailable_reports_bwrap_version_when_known() {
        let err = check_tier3_available(false, &Some("0.9.0".into())).unwrap_err();
        assert!(
            err.contains("0.9.0"),
            "refusal reason must name the installed version so a user can act on it: {err}"
        );
        assert!(err.contains(">= 0.10.0"));
    }

    #[test]
    fn tier3_unavailable_reports_missing_bwrap() {
        let err = check_tier3_available(false, &None).unwrap_err();
        assert!(err.contains("not found"));
    }

    // --- check_tier_implemented_for_os: the cross-platform tier-existence gate ---

    fn no_caps() -> crate::capabilities::Capabilities {
        crate::capabilities::Capabilities {
            landlock_abi: None,
            bwrap_version: None,
            unprivileged_overlay: false,
            seatbelt_available: None,
        }
    }

    #[test]
    fn tier0_always_implemented_regardless_of_os() {
        let caps = no_caps();
        assert!(check_tier_implemented_for_os(Tier::Tier0, &caps, true, "linux").is_ok());
        assert!(check_tier_implemented_for_os(Tier::Tier0, &caps, false, "macos").is_ok());
    }

    #[test]
    fn tier1_2_3_all_implemented_on_linux() {
        let caps = no_caps();
        for tier in [Tier::Tier1, Tier::Tier2, Tier::Tier3] {
            assert!(
                check_tier_implemented_for_os(tier, &caps, true, "linux").is_ok(),
                "Tier {} must be implemented when is_linux=true",
                tier.level()
            );
        }
    }

    #[test]
    fn tier1_2_3_all_refused_on_non_linux() {
        let caps = no_caps();
        for tier in [Tier::Tier1, Tier::Tier2, Tier::Tier3] {
            let err = check_tier_implemented_for_os(tier, &caps, false, "macos").unwrap_err();
            assert!(
                err.contains("macos"),
                "refusal reason must name the actual OS so a user knows why: {err}"
            );
        }
    }

    #[test]
    fn tier2_3_refusal_names_the_real_mechanisms() {
        let caps = no_caps();
        for tier in [Tier::Tier2, Tier::Tier3] {
            let err = check_tier_implemented_for_os(tier, &caps, false, "macos").unwrap_err();
            assert!(err.contains("Landlock"));
            assert!(err.contains("bubblewrap"));
            assert!(err.contains("OverlayFS"));
        }
    }

    /// Tier 1's refusal message is the one place `check_tier_implemented`
    /// reads `seatbelt_available` at all: when Seatbelt IS present, the
    /// message must say so and explain why that doesn't help yet (no Tier 1
    /// implementation built on it), rather than implying no isolation
    /// mechanism exists on the host whatsoever.
    #[test]
    fn tier1_refusal_mentions_seatbelt_when_present() {
        let mut caps = no_caps();
        caps.seatbelt_available = Some(true);
        let err = check_tier_implemented_for_os(Tier::Tier1, &caps, false, "macos").unwrap_err();
        assert!(err.contains("sandbox-exec"));
        assert!(err.contains("does not yet implement"));
    }

    #[test]
    fn tier1_refusal_omits_seatbelt_mention_when_absent() {
        let mut caps = no_caps();
        caps.seatbelt_available = Some(false);
        let err = check_tier_implemented_for_os(Tier::Tier1, &caps, false, "macos").unwrap_err();
        assert!(!err.contains("sandbox-exec"));
    }

    #[test]
    fn tier1_refusal_omits_seatbelt_mention_when_not_applicable() {
        // seatbelt_available: None means "not macOS" (see the field's own
        // doc comment) -- must not be conflated with Some(false) here.
        let caps = no_caps();
        let err = check_tier_implemented_for_os(Tier::Tier1, &caps, false, "windows").unwrap_err();
        assert!(!err.contains("sandbox-exec"));
    }

    /// The public `check_tier_implemented` wrapper must actually forward to
    /// `cfg!(target_os = "linux")`/`std::env::consts::OS`, not some other
    /// hardcoded value -- this pins that wiring down. It can only assert
    /// against whichever OS actually runs this test, which is why the
    /// `_for_os` tests above exist to cover the branch this build isn't on.
    #[test]
    fn public_wrapper_reflects_actual_host_os() {
        let caps = no_caps();
        let result = check_tier_implemented(Tier::Tier1, &caps);
        if cfg!(target_os = "linux") {
            assert!(result.is_ok());
        } else {
            assert!(result.is_err());
        }
    }
}
