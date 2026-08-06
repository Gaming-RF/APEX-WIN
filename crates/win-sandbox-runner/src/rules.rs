use anyhow::{Context, Result};
use std::path::Path;
use tracing::debug;
use win_sandbox_common::rules_schema::RulesFile;

/// Load and validate a rules file from the given path.
/// If no path is provided, returns the default rules.
pub fn load_rules(path: Option<&Path>) -> Result<RulesFile> {
    match path {
        Some(p) => {
            debug!("Loading rules from: {}", p.display());
            let data = std::fs::read_to_string(p)
                .with_context(|| format!("Failed to read rules file: {}", p.display()))?;
            let rules: RulesFile = serde_json::from_str(&data)
                .with_context(|| format!("Failed to parse rules file: {}", p.display()))?;
            validate_rules(&rules)?;
            Ok(rules)
        }
        None => {
            debug!("No rules file specified, using defaults");
            Ok(default_rules())
        }
    }
}

/// Return default rules (no entries, permissive defaults).
fn default_rules() -> RulesFile {
    RulesFile {
        version: 1,
        entries: Vec::new(),
        defaults: Default::default(),
    }
}

/// Validate rules file structure.
fn validate_rules(rules: &RulesFile) -> Result<()> {
    if rules.version != 1 {
        anyhow::bail!("Unsupported rules version: {}", rules.version);
    }
    for entry in &rules.entries {
        if entry.hash.len() != 64 || !entry.hash.chars().all(|c| c.is_ascii_hexdigit()) {
            anyhow::bail!(
                "Invalid hash format for entry '{}': expected 64 hex chars, got '{}'",
                entry.name,
                entry.hash
            );
        }
    }
    Ok(())
}

/// Look up a rule entry by binary hash.
pub fn lookup_by_hash<'a>(rules: &'a RulesFile, hash: &str) -> Option<&'a win_sandbox_common::rules_schema::RuleEntry> {
    rules.entries.iter().find(|e| e.hash == hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_rules_are_valid() {
        let rules = default_rules();
        validate_rules(&rules).unwrap();
    }

    #[test]
    fn invalid_version_rejected() {
        let rules = RulesFile {
            version: 99,
            entries: vec![],
            defaults: Default::default(),
        };
        assert!(validate_rules(&rules).is_err());
    }

    #[test]
    fn invalid_hash_rejected() {
        let rules = RulesFile {
            version: 1,
            entries: vec![win_sandbox_common::rules_schema::RuleEntry {
                hash: "not-a-valid-hash".into(),
                name: "test".into(),
                tier: win_sandbox_common::tier::Tier::Tier0,
                allowed_paths: vec![],
                network: false,
                gpu: false,
            }],
            defaults: Default::default(),
        };
        assert!(validate_rules(&rules).is_err());
    }

    #[test]
    fn load_nonexistent_returns_default() {
        let rules = load_rules(None).unwrap();
        assert_eq!(rules.version, 1);
        assert!(rules.entries.is_empty());
    }
}
