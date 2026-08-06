use crate::Args;
use anyhow::{bail, Result};
use landlock::{
    path_beneath_rules, Access, AccessFs, CompatLevel, Compatible, LandlockStatus,
    Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, ABI,
};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};
use tracing::{info, warn};

/// Tier 1: Landlock LSM sandbox.
///
/// Applies Landlock rules to restrict filesystem access.
/// Read-only: /usr, /lib, /lib64, /opt, /etc, /bin, /sbin, wine prefix.
/// Read-write: binary dir, /tmp/win-runtime-$$.
///
/// NOTE: Landlock cannot block all network access — it can only GRANT specific
/// ports. For network isolation, use Tier 2 (bwrap with --unshare-net) or Tier 3.
/// Network restriction is NOT part of Tier 1.
pub fn run(args: &Args) -> Result<ExitCode> {
    info!("Tier 1: Landlock sandbox for {}", args.exe);

    let config = crate::config::load_config(None);
    let abi = detect_landlock_abi()?;

    // Build read-only paths: system dirs + wine prefix
    let ro_paths = build_ro_paths(&config.wine_prefix, args);
    let rw_paths = build_rw_paths();

    // Create ruleset with filesystem access handling.
    // Landlock cannot deny all TCP — it only grants specific ports. So we only
    // handle filesystem access here. Network isolation requires bwrap/seccomp.
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))?
        .create()?
        // Read-only filesystem rules (system dirs, wine prefix, device nodes)
        .add_rules(path_beneath_rules(&ro_paths, AccessFs::from_read(abi)))?
        // Read-write filesystem rules (tmp dir, XDG runtime, /dev/null etc.)
        .add_rules(path_beneath_rules(&rw_paths, AccessFs::from_all(abi)))?
        .set_compatibility(CompatLevel::BestEffort)
        .restrict_self()?;

    if status.ruleset == RulesetStatus::NotEnforced {
        bail!("Landlock ruleset could not be enforced at all");
    }

    match status.landlock {
        LandlockStatus::NotEnabled => {
            warn!("Landlock is disabled in kernel config (CONFIG_LSM)");
        }
        LandlockStatus::NotImplemented => {
            warn!("Landlock is not built into this kernel");
        }
        LandlockStatus::Available { effective_abi, .. } => {
            info!("Landlock enforced with ABI v{}", effective_abi as u32);
        }
    }

    // Now exec wine — Landlock rules are inherited by child processes
    info!("Launching wine in Landlock sandbox");
    let sandbox_env = crate::env_sanitize::build_sandbox_env(&config)?;
    let mut cmd = Command::new("wine");
    cmd.arg(&args.exe).args(&args.args);
    cmd.env_clear();
    for (key, val) in &sandbox_env {
        cmd.env(key, val);
    }
    let err = cmd.exec();

    bail!("Failed to exec wine: {err}");
}

/// Detect the highest Landlock ABI version the kernel supports.
fn detect_landlock_abi() -> Result<ABI> {
    for abi in [ABI::V4, ABI::V3, ABI::V2, ABI::V1] {
        let result = Ruleset::default().handle_access(AccessFs::from_all(abi));
        if result.is_ok() {
            info!("Detected Landlock ABI v{}", abi as u32);
            return Ok(abi);
        }
    }
    bail!("Landlock is not supported on this kernel (requires 5.13+)")
}

/// Build the list of read-only filesystem paths.
fn build_ro_paths(wine_prefix: &str, args: &Args) -> Vec<String> {
    let mut paths = vec![
        "/usr".to_string(),
        "/lib".to_string(),
        "/lib64".to_string(),
        "/bin".to_string(),
        "/sbin".to_string(),
        "/opt".to_string(),
        "/etc".to_string(),
        "/dev/urandom".to_string(),
        "/dev/null".to_string(),
        "/dev/zero".to_string(),
        wine_prefix.to_string(),
    ];

    if let Some(parent) = std::path::Path::new(&args.exe).parent() {
        let parent_str = parent.to_string_lossy().to_string();
        if !paths.contains(&parent_str) && !parent_str.is_empty() && parent_str != "/" {
            paths.push(parent_str);
        }
    }

    paths
}

/// Build the list of read-write filesystem paths.
fn build_rw_paths() -> Vec<String> {
    let pid = std::process::id();
    let tmp_dir = format!("/tmp/win-runtime-{pid}");
    let _ = std::fs::create_dir_all(&tmp_dir);

    let mut paths = vec![
        "/dev/null".to_string(),
        "/dev/full".to_string(),
        "/dev/zero".to_string(),
        tmp_dir,
    ];

    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        paths.push(runtime);
    }

    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ro_paths_include_system() {
        let args = Args {
            exe: "/home/test/game.exe".into(),
            tier: None,
            rules: None,
            verbose: false,
            no_gui: false,
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
        };
        let paths = build_ro_paths("/home/test/.wine", &args);
        assert!(paths.contains(&"/usr".to_string()));
        assert!(paths.contains(&"/lib".to_string()));
        assert!(paths.contains(&"/home/test/.wine".to_string()));
        assert!(paths.contains(&"/home/test".to_string()));
    }

    #[test]
    fn ro_paths_exclude_root() {
        let args = Args {
            exe: "/game.exe".into(),
            tier: None,
            rules: None,
            verbose: false,
            no_gui: false,
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
        };
        let paths = build_ro_paths("/tmp/.wine", &args);
        // "/" must NOT be in ro_paths (would grant read to entire root)
        assert!(!paths.contains(&"/".to_string()));
    }

    #[test]
    fn rw_paths_include_tmp() {
        let paths = build_rw_paths();
        assert!(paths.iter().any(|p| p.starts_with("/tmp/win-runtime-")));
    }
}
