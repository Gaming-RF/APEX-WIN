//! Background daemon for seamless .exe execution.
//!
//! The daemon pre-loads the app database, rules, and prefix manager into memory
//! and registers a binfmt_misc handler. When a .exe is launched, the kernel
//! routes it through the daemon via a named pipe (FIFO), avoiding the cold-start
//! overhead of loading config on every launch.
//!
//! An IPC Unix socket allows runtime control: trust apps, reload rules, query status.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use tracing::{debug, error, info, warn};

use crate::{appdb, config, dispatch, hasher, netopt, prefix, rules, Args};
use win_sandbox_common::rules_schema::RulesFile;

/// Runtime state cached by the daemon to avoid re-loading on every .exe launch.
pub struct DaemonState {
    pub app_db: appdb::AppDatabase,
    pub rules: RulesFile,
    #[allow(dead_code)]
    pub prefix_mgr: prefix::PrefixManager,
    #[allow(dead_code)]
    pub config: crate::config::Config,
    pub net_config: netopt::NetOptimizerConfig,
    /// Track how many .exe launches we've handled.
    pub launch_count: u64,
    /// Track per-exe launch history (exe path -> count).
    pub launch_history: HashMap<String, u64>,
}

impl DaemonState {
    /// Load all state from disk.
    pub fn load() -> Result<Self> {
        let app_db = appdb::AppDatabase::load_embedded();
        let rules_path = config::find_rules_path(None);
        let rules = rules::load_rules(rules_path.as_deref())?;
        let prefix_mgr = prefix::PrefixManager::new();
        let config = crate::config::load_config(None);
        let net_config = netopt::load_config(None);

        info!(
            "Daemon state loaded: {} app profiles, {} rules",
            app_db.profiles.len(),
            rules.entries.len()
        );

        Ok(Self {
            app_db,
            rules,
            prefix_mgr,
            config,
            net_config,
            launch_count: 0,
            launch_history: HashMap::new(),
        })
    }

    /// Reload rules from disk (user may have edited rules.json).
    pub fn reload_rules(&mut self) -> Result<()> {
        let rules_path = config::find_rules_path(None);
        self.rules = rules::load_rules(rules_path.as_deref())?;
        info!("Rules reloaded: {} entries", self.rules.entries.len());
        Ok(())
    }

    /// Reload the network optimizer config from disk.
    pub fn reload_net_config(&mut self) {
        self.net_config = netopt::load_config(None);
        info!("Network config reloaded");
    }
}

/// Paths used by the daemon.
fn fifo_path() -> PathBuf {
    PathBuf::from("/run/win-sandbox-runner/fifo")
}

fn socket_path() -> PathBuf {
    PathBuf::from("/run/win-sandbox-runner/ipc.sock")
}

fn pid_path() -> PathBuf {
    PathBuf::from("/run/win-sandbox-runner/daemon.pid")
}

/// Ensure the runtime directory exists with correct permissions.
fn ensure_runtime_dir() -> Result<()> {
    let dir = Path::new("/run/win-sandbox-runner");
    if !dir.exists() {
        std::fs::create_dir_all(dir)
            .context("Failed to create /run/win-sandbox-runner (are you root?)")?;
        // Allow any user to write to the FIFO (for binfmt_misc)
        std::fs::set_permissions(
            dir,
            std::os::unix::fs::PermissionsExt::from_mode(0o1777),
        )?;
    }
    Ok(())
}

/// Write the daemon PID file.
fn write_pid() -> Result<()> {
    let pid = std::process::id();
    std::fs::write(pid_path(), pid.to_string())?;
    debug!("PID {pid} written to {}", pid_path().display());
    Ok(())
}

/// Remove daemon runtime files on shutdown.
#[allow(dead_code)]
fn cleanup_runtime() {
    let _ = std::fs::remove_file(fifo_path());
    let _ = std::fs::remove_file(socket_path());
    let _ = std::fs::remove_file(pid_path());
}

/// Register the binfmt_misc handler for .exe files.
///
/// This tells the kernel: "when someone tries to execute a .exe file,
/// run /usr/bin/win-sandbox-runner instead, passing the .exe path as an argument."
fn register_binfmt() -> Result<()> {
    let binfmt_dir = Path::new("/proc/sys/fs/binfmt_misc");
    if !binfmt_dir.exists() {
        return Err(anyhow::anyhow!(
            "binfmt_misc not available (is the kernel module loaded?)"
        ));
    }

    let register_path = binfmt_dir.join("register");

    // binfmt_misc format: :name:type:offset:magic:mask:interpreter:flags
    // - name: PE_WINE
    // - type: M (magic match)
    // - offset: 0 (check at start of file)
    // - magic: 4d5a (MZ header — PE executable signature)
    // - mask: ffff (match first 2 bytes)
    // - interpreter: /usr/bin/win-sandbox-runner
    // - flags: F (fix binary — don't need interpreter to be available at open time)
    //
    // We use C (credential inheritance) and F (fix binary) flags.
    // C ensures the runner inherits the credentials of the user who launched the exe.
    let registration =
        ":APEX-WIN:M:0:\\x4d\\x5a:/usr/bin/win-sandbox-runner:CF\n".to_string();

    // First, unregister if already registered
    let status_path = binfmt_dir.join("APEX-WIN");
    if status_path.exists() {
        let _ = std::fs::write(&status_path, "-1");
        debug!("Unregistered existing APEX-WIN binfmt handler");
    }

    std::fs::write(&register_path, &registration)
        .context("Failed to register binfmt handler (are you root?)")?;

    info!("binfmt_misc registered: .exe -> /usr/bin/win-sandbox-runner");
    Ok(())
}

/// Unregister the binfmt_misc handler.
pub fn unregister_binfmt() -> Result<()> {
    let status_path = Path::new("/proc/sys/fs/binfmt_misc/APEX-WIN");
    if status_path.exists() {
        std::fs::write(status_path, "-1")?;
        info!("binfmt_misc unregistered: APEX-WIN removed");
    } else {
        info!("binfmt_misc: APEX-WIN not registered (nothing to do)");
    }
    Ok(())
}

/// Create the named pipe (FIFO) for binfmt_misc communication.
fn create_fifo() -> Result<()> {
    let path = fifo_path();
    if path.exists() {
        std::fs::remove_file(&path)?;
    }

    // Create FIFO with permissions allowing any user to write
    unsafe {
        let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        let ret = libc::mkfifo(c_path.as_ptr(), 0o666);
        if ret != 0 {
            return Err(anyhow::anyhow!(
                "Failed to create FIFO at {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
    }

    // Set permissions to world-writable (any user launching .exe needs to write)
    std::fs::set_permissions(
        &path,
        std::os::unix::fs::PermissionsExt::from_mode(0o666),
    )?;

    debug!("FIFO created at {}", path.display());
    Ok(())
}

/// Spawn the IPC socket listener thread.
///
/// Accepts commands over a Unix socket:
///   - "trust <exe_path>" — add a trusted rule for the given exe
///   - "reload" — reload rules from disk
///   - "status" — return daemon status as JSON
///   - "quit" — shut down the daemon
fn spawn_ipc_listener(
    state: Arc<Mutex<DaemonState>>,
    shutdown: Arc<Mutex<bool>>,
) -> Result<thread::JoinHandle<()>> {
    let path = socket_path();
    if path.exists() {
        std::fs::remove_file(&path)?;
    }

    let listener = UnixListener::bind(&path)
        .context("Failed to create IPC socket")?;

    // Allow any user to connect
    std::fs::set_permissions(
        &path,
        std::os::unix::fs::PermissionsExt::from_mode(0o666),
    )?;

    info!("IPC socket listening at {}", path.display());

    let handle = thread::spawn(move || {
        listener.set_nonblocking(true).ok();

        loop {
            // Check shutdown flag
            if *shutdown.lock().unwrap() {
                info!("IPC listener shutting down");
                break;
            }

            match listener.accept() {
                Ok((stream, _)) => {
                    let state = Arc::clone(&state);
                    let shutdown = Arc::clone(&shutdown);
                    thread::spawn(move || {
                        if let Err(e) = handle_ipc_command(stream, &state, &shutdown) {
                            debug!("IPC command error: {e}");
                        }
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(100));
                    continue;
                }
                Err(e) => {
                    warn!("IPC accept error: {e}");
                    thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
    });

    Ok(handle)
}

/// Handle a single IPC command.
fn handle_ipc_command(
    mut stream: UnixStream,
    state: &Arc<Mutex<DaemonState>>,
    shutdown: &Arc<Mutex<bool>>,
) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let cmd = line.trim();

    debug!("IPC command: {cmd}");

    let response = if cmd == "status" {
        let state = state.lock().unwrap();
        format!(
            r#"{{"launch_count":{},"app_profiles":{},"rules":{},"uptime":"running"}}"#,
            state.launch_count,
            state.app_db.profiles.len(),
            state.rules.entries.len(),
        )
    } else if cmd == "reload" {
        let mut state = state.lock().unwrap();
        state.reload_rules()?;
        state.reload_net_config();
        "OK: rules and config reloaded\n".to_string()
    } else if let Some(exe_path) = cmd.strip_prefix("trust ") {
        let exe_path = exe_path.trim();
        let hash = hasher::hash_file(exe_path)?;
        crate::save_trusted_rule(exe_path, &hash)?;
        let mut state = state.lock().unwrap();
        state.reload_rules()?;
        format!("OK: {exe_path} trusted (hash: {hash})\n")
    } else if cmd == "quit" {
        "OK: shutting down\n".to_string()
    } else {
        format!("ERROR: unknown command '{cmd}'\n")
    };

    stream.write_all(response.as_bytes())?;
    stream.flush()?;

    if cmd == "quit" {
        *shutdown.lock().unwrap() = true;
    }

    Ok(())
}

/// Main daemon entry point.
///
/// 1. Loads all state into memory
/// 2. Registers binfmt_misc handler
/// 3. Creates FIFO
/// 4. Spawns IPC listener
/// 5. Listens on FIFO for .exe launch requests
/// 6. Dispatches each request using cached state
pub fn run_daemon() -> Result<()> {
    info!("Starting APEX-WIN daemon...");

    // Ensure we're running as root (needed for binfmt_misc and /run)
    // SAFETY: geteuid() is always safe to call on Unix
    if unsafe { libc::geteuid() } != 0 {
        return Err(anyhow::anyhow!(
            "Daemon must run as root (needed for binfmt_misc registration)"
        ));
    }

    // 1. Load state
    let state = Arc::new(Mutex::new(DaemonState::load()?));

    // 2. Setup runtime
    ensure_runtime_dir()?;
    write_pid()?;

    // 3. Register binfmt_misc
    register_binfmt()?;

    // 4. Create FIFO
    create_fifo()?;

    // 5. Spawn IPC listener
    let shutdown = Arc::new(Mutex::new(false));
    let _ipc_handle = spawn_ipc_listener(Arc::clone(&state), Arc::clone(&shutdown))?;

    // 6. Apply network optimizations if config says to
    {
        let state = state.lock().unwrap();
        match netopt::optimize(&state.net_config) {
            Ok(result) => {
                if result.bbr_applied || result.sqm_applied {
                    info!("Network optimized for gaming: {result}");
                }
            }
            Err(e) => {
                debug!("Network optimization skipped (not root or not available): {e}");
            }
        }
    }

    info!("APEX-WIN daemon ready. Listening on {}", fifo_path().display());

    // 7. Main loop: read exe paths from FIFO, dispatch each one
    loop {
        // Check shutdown flag
        if *shutdown.lock().unwrap() {
            info!("Daemon shutting down...");
            break;
        }

        // Open FIFO for reading (non-blocking so we can check shutdown)
        use std::os::unix::fs::OpenOptionsExt;
        let fifo = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(fifo_path())
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No writer yet, sleep and retry
                thread::sleep(std::time::Duration::from_millis(200));
                continue;
            }
            Err(e) => {
                error!("Failed to open FIFO: {e}");
                thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
        };

        let reader = BufReader::new(fifo);
        for line in reader.lines() {
            match line {
                Ok(exe_path) => {
                    let exe_path = exe_path.trim().to_string();
                    if exe_path.is_empty() {
                        continue;
                    }

                    info!("Daemon: intercepted .exe launch: {exe_path}");

                    let state = Arc::clone(&state);
                    // Spawn a worker thread for each launch
                    thread::spawn(move || {
                        if let Err(e) = handle_launch(&exe_path, &state) {
                            error!("Launch failed for {exe_path}: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    debug!("FIFO read error (writer closed): {e}");
                    break; // Re-open FIFO
                }
            }
        }
    }

    // Cleanup on graceful shutdown
    info!("Cleaning up...");
    unregister_binfmt().ok();
    cleanup_runtime();
    info!("APEX-WIN daemon stopped.");
    Ok(())
}

/// Handle a single .exe launch request from the FIFO.
fn handle_launch(exe_path: &str, state: &Arc<Mutex<DaemonState>>) -> Result<()> {
    // Update launch stats
    {
        let mut state = state.lock().unwrap();
        state.launch_count += 1;
        *state.launch_history.entry(exe_path.to_string()).or_insert(0) += 1;
    }

    // Hash the binary
    let hash = hasher::hash_file(exe_path)?;

    // Build Args for the dispatch
    let args = Args {
        exe: Some(exe_path.to_string()),
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
        configure_net: false,
        daemon: false,
        status: false,
        reload: false,
        stop: false,
        unregister: false,
    };

    // Lock state for dispatch (keeps app_db and rules consistent)
    let state = state.lock().unwrap();

    dispatch::execute(&args, exe_path, &hash, &state.rules, &state.app_db)?;

    Ok(())
}

/// Query the daemon status via IPC socket.
pub fn query_status() -> Result<String> {
    let path = socket_path();
    if !path.exists() {
        return Err(anyhow::anyhow!(
            "Daemon not running (socket not found at {})",
            path.display()
        ));
    }

    let mut stream = UnixStream::connect(&path)
        .context("Failed to connect to daemon IPC socket")?;

    stream.write_all(b"status\n")?;
    stream.flush()?;

    let mut response = String::new();
    let mut reader = BufReader::new(&stream);
    reader.read_line(&mut response)?;

    Ok(response)
}

/// Send a command to the daemon via IPC socket.
pub fn send_command(cmd: &str) -> Result<String> {
    let path = socket_path();
    if !path.exists() {
        return Err(anyhow::anyhow!(
            "Daemon not running (socket not found at {})",
            path.display()
        ));
    }

    let mut stream = UnixStream::connect(&path)
        .context("Failed to connect to daemon IPC socket")?;

    stream.write_all(format!("{cmd}\n").as_bytes())?;
    stream.flush()?;

    let mut response = String::new();
    let mut reader = BufReader::new(&stream);
    reader.read_line(&mut response)?;

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_path_format() {
        assert_eq!(fifo_path(), PathBuf::from("/run/win-sandbox-runner/fifo"));
    }

    #[test]
    fn socket_path_format() {
        assert_eq!(
            socket_path(),
            PathBuf::from("/run/win-sandbox-runner/ipc.sock")
        );
    }

    #[test]
    fn pid_path_format() {
        assert_eq!(
            pid_path(),
            PathBuf::from("/run/win-sandbox-runner/daemon.pid")
        );
    }
}
