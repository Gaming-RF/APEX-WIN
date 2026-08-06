use win_sandbox_common::tier::Tier;

/// Result of the user's confirmation dialog choice.
#[derive(Debug, Clone)]
pub struct ConfirmResult {
    pub tier: Tier,
    pub remember: bool,
}

/// Show a confirmation dialog for an unmapped binary.
///
/// Presents the user with:
/// - Binary name and hash
/// - "Run Sandboxed (Tier 2)" button
/// - "Run Direct (Tier 0)" button
/// - "Deny" button
/// - "Remember this choice" checkbox
pub fn show(_binary_name: &str, _hash: &str, _path: &str) -> ConfirmResult {
    // TODO: Create GTK4 dialog window
    // TODO: Add binary info display
    // TODO: Add tier selection buttons
    // TODO: Add "Remember" checkbox
    // TODO: Wait for user response

    todo!("Confirmation dialog not yet implemented")
}
