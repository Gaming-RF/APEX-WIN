use tracing::info;
use win_sandbox_common::rules_schema::RuleEntry;
use win_sandbox_common::tier::Tier;

use crate::appdb;

/// First-launch wizard result.
pub struct WizardResult {
    pub entry: RuleEntry,
    pub source: WizardSource,
}

/// Where the wizard got its decision from.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum WizardSource {
    /// Auto-detected from the app database by exe name.
    AppDatabase,
    /// Auto-detected by heuristics (installer, known safe, etc.).
    AutoDetect,
    /// User chose interactively.
    UserChoice,
    /// User chose to always trust this app.
    UserTrusted,
}

/// Run the first-launch wizard for an unknown .exe.
///
/// In headless mode (no_gui=true), auto-detects tier from the app database
/// and heuristics. In interactive mode, prompts the user.
pub fn run_wizard(
    exe_path: &str,
    app_db: &appdb::AppDatabase,
    no_gui: bool,
    hash: &str,
) -> WizardResult {
    // 1. Try app database match first
    if let Some((profile, entry)) = app_db.lookup_by_name(exe_path) {
        info!(
            "First launch: matched '{}' in app database -> '{}'",
            exe_path, profile.name
        );
        if !profile.notes.is_empty() {
            info!("Note: {}", profile.notes);
        }
        return WizardResult {
            entry,
            source: WizardSource::AppDatabase,
        };
    }

    // 2. Auto-detect from heuristics
    let (suggested_tier, reason) = appdb::auto_detect_tier(exe_path);
    info!("First launch: {reason}");

    // 3. If interactive and not headless, we could prompt the user here.
    // For now, use auto-detect in all modes (future: add TTY prompt).
    if !no_gui && is_interactive() {
        // Future: show GTK4 dialog or TTY prompt
        // For now, just use auto-detect
        info!("Interactive wizard not yet implemented, using auto-detect");
    }

    WizardResult {
        entry: make_auto_entry(exe_path, hash, suggested_tier),
        source: WizardSource::AutoDetect,
    }
}

/// Check if stdin is a TTY (interactive terminal).
fn is_interactive() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) != 0 }
}

/// Create a RuleEntry for an auto-detected app.
fn make_auto_entry(exe_path: &str, hash: &str, tier: Tier) -> RuleEntry {
    let name = std::path::Path::new(exe_path)
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let network = tier >= Tier::Tier2;
    let gpu = true; // Most Windows apps need GPU

    RuleEntry {
        hash: hash.to_string(),
        name,
        tier,
        allowed_paths: vec![],
        network,
        gpu,
        trusted: false,
        dxvk: false,
        winetricks: vec![],
        env: std::collections::HashMap::new(),
        wine_variant: "system".into(),
    }
}

/// Show what the wizard decided (for logging/UI).
pub fn describe_decision(result: &WizardResult) -> String {
    let entry = &result.entry;
    let source = match result.source {
        WizardSource::AppDatabase => "app database",
        WizardSource::AutoDetect => "auto-detected",
        WizardSource::UserChoice => "user selected",
        WizardSource::UserTrusted => "user trusted",
    };

    let mut parts = vec![
        format!("App: {}", entry.name),
        format!("Source: {source}"),
        format!("Tier: {}", entry.tier),
    ];

    if entry.trusted {
        parts.push("Trusted: yes (no sandbox)".into());
    }
    if entry.network {
        parts.push("Network: enabled".into());
    }
    if entry.gpu {
        parts.push("GPU: enabled".into());
    }
    if entry.dxvk {
        parts.push("DXVK: will install".into());
    }
    if !entry.winetricks.is_empty() {
        parts.push(format!("Winetricks: {}", entry.winetricks.join(", ")));
    }

    parts.join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wizard_finds_app_in_database() {
        let db = appdb::AppDatabase {
            version: 1,
            description: "test".into(),
            profiles: vec![appdb::AppProfile {
                name: "Fusion 360".into(),
                match_names: vec!["Fusion360.exe".into()],
                tier: Tier::Tier0,
                trusted: true,
                network: true,
                gpu: true,
                dxvk: true,
                winetricks: vec!["dotnet48".into()],
                env: std::collections::HashMap::new(),
                wine_variant: "staging".into(),
                notes: "CAD app".into(),
            }],
        };

        let result = run_wizard("C:\\Fusion360.exe", &db, true, "abc123");
        assert_eq!(result.source, WizardSource::AppDatabase);
        assert!(result.entry.trusted);
    }

    #[test]
    fn wizard_falls_back_to_auto_detect() {
        let db = appdb::AppDatabase {
            version: 1,
            description: "test".into(),
            profiles: vec![],
        };

        // Installer detected
        let result = run_wizard("setup_myapp.exe", &db, true, "def456");
        assert_eq!(result.source, WizardSource::AutoDetect);
        assert_eq!(result.entry.tier, Tier::Tier2);

        // Unknown app
        let result = run_wizard("random.exe", &db, true, "ghi789");
        assert_eq!(result.source, WizardSource::AutoDetect);
        assert_eq!(result.entry.tier, Tier::Tier1);
    }

    #[test]
    fn describe_decision_shows_info() {
        let result = WizardResult {
            entry: RuleEntry {
                hash: "abc".into(),
                name: "Test".into(),
                tier: Tier::Tier2,
                allowed_paths: vec![],
                network: true,
                gpu: true,
                trusted: false,
                dxvk: true,
                winetricks: vec!["vcrun2019".into()],
                env: std::collections::HashMap::new(),
                wine_variant: "system".into(),
            },
            source: WizardSource::AppDatabase,
        };

        let desc = describe_decision(&result);
        assert!(desc.contains("Test"));
        assert!(desc.contains("app database"));
        assert!(desc.contains("DXVK"));
        assert!(desc.contains("vcrun2019"));
    }
}
