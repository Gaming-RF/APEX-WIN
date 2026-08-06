use serde::{Deserialize, Serialize};
use crate::tier::Tier;

/// Top-level rules file structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesFile {
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<RuleEntry>,
    #[serde(default)]
    pub defaults: RuleDefaults,
}

/// A single rule entry keyed by binary SHA-256 hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleEntry {
    /// SHA-256 hex string (64 chars).
    pub hash: String,
    /// Human-readable name for this binary.
    pub name: String,
    /// Sandbox tier to apply.
    pub tier: Tier,
    /// Filesystem paths the binary may access (beyond defaults).
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    /// Whether network access is permitted.
    #[serde(default)]
    pub network: bool,
    /// Whether GPU passthrough is permitted.
    #[serde(default)]
    pub gpu: bool,
}

/// Default policy for unmapped binaries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleDefaults {
    /// Tier for binaries whose hash is not in entries.
    pub unmapped_tier: Tier,
    /// Tier for binaries launched from untrusted paths (/tmp, /mnt, etc.).
    pub untrusted_path_tier: Tier,
    /// Default network permission for unmapped binaries.
    #[serde(default)]
    pub network_default: bool,
    /// Default GPU permission for unmapped binaries.
    #[serde(default)]
    pub gpu_default: bool,
}

impl Default for RuleDefaults {
    fn default() -> Self {
        Self {
            unmapped_tier: Tier::Tier0,
            untrusted_path_tier: Tier::Tier2,
            network_default: false,
            gpu_default: false,
        }
    }
}
