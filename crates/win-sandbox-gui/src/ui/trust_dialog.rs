/// Result of the trust-level dialog choice.
#[derive(Debug, Clone)]
pub struct TrustResult {
    pub tier: u8,
    pub network: bool,
    pub gpu: bool,
    pub remember: bool,
}

/// Show a trust-level selector dialog for an untrusted path.
///
/// Presents the user with:
/// - Binary info (name, hash, path)
/// - Tier radio buttons (0-3)
/// - Network toggle
/// - GPU toggle
/// - "Remember" checkbox
pub fn show(_binary_name: &str, _hash: &str, _path: &str, _suggested_tier: u8) -> TrustResult {
    // TODO: Create GTK4 dialog window
    // TODO: Add tier radio buttons
    // TODO: Add network/GPU toggles
    // TODO: Add "Remember" checkbox
    // TODO: Wait for user response

    todo!("Trust dialog not yet implemented")
}
