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

use crate::{appdb, config, dispatch, hasher, netopt, rules, Args};
use win_sandbox_common::rules_schema::RulesFile;

/// Runtime state cached by the daemon to avoid re-loading on every .exe launch.
pub struct DaemonState {
    pub app_db: appdb::AppDatabase,
    pub rules: RulesFile,
    #[allow(dead_code)]
    pub config: crate::config::Config,
    pub net_config: netopt::NetOptimizerConfig,
    /// Host sandbox capabilities (Landlock ABI, bwrap version, overlay
    /// support), probed once at startup. `--status` reports this so "is
    /// Tier 3 real on this machine" is answerable without reading source or
    /// grepping the journal — the same question dispatch's fail-secure Tier 3
    /// check answers for itself on every explicit Tier 3 launch.
    pub capabilities: crate::capabilities::Capabilities,
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
        // No PrefixManager here on purpose: it must be built per-launch from the
        // FIFO-forwarded user env (PrefixManager::for_user), because the daemon
        // runs as root and its own HOME is /root or unset.
        let config = crate::config::load_config(None);
        let net_config = netopt::load_config(None);
        let capabilities = crate::capabilities::Capabilities::detect();

        info!(
            "Daemon state loaded: {} app profiles, {} rules",
            app_db.profiles.len(),
            rules.entries.len()
        );
        info!(
            "Sandbox capabilities: landlock_abi={:?} bwrap={:?} tier3_available={}",
            capabilities.landlock_abi,
            capabilities.bwrap_version,
            capabilities.unprivileged_overlay
        );

        Ok(Self {
            app_db,
            rules,
            config,
            net_config,
            capabilities,
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

/// Base directory for daemon runtime state (FIFO, IPC socket, PID file).
///
/// Linux: `/run` is the systemd/FHS convention — the unit file's
/// `RuntimeDirectory=win-sandbox-runner` (see
/// `scripts/win-sandbox-runner.service`) creates this exact path before
/// `ExecStart` runs, so this string must stay in sync with that unit file.
/// The Linux daemon runs as root (`run_daemon()` enforces this) because it
/// needs to register a binfmt_misc handler in `/proc/sys/fs/binfmt_misc`,
/// which requires root, and `/run` itself is root-owned.
///
/// macOS: there is no binfmt_misc, and therefore nothing that requires the
/// daemon to run as root — see the platform split in `run_daemon()` below.
/// It runs as a per-user `launchd` LaunchAgent instead (mirroring the
/// architecture doc's Path A: the file's owner runs Wine directly, nothing
/// executes as root). `/var/run` (root-owned, mode 0755) would therefore
/// EACCES for a plain user process, so the runtime dir lives under the
/// user's own `$TMPDIR` instead — the macOS analogue of Linux's
/// `XDG_RUNTIME_DIR`, and already private per-user (mode 0700, set by the
/// OS at login).
#[cfg(target_os = "linux")]
const RUNTIME_DIR_BASE: &str = "/run/win-sandbox-runner";

/// Runtime directory base for macOS: `$TMPDIR/win-sandbox-runner`, falling
/// back to `/tmp/win-sandbox-runner` if `$TMPDIR` is unset (should not
/// happen under launchd, which always sets it, but a bare `cargo run`
/// during development might not).
#[cfg(not(target_os = "linux"))]
fn runtime_dir_base() -> PathBuf {
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
    // TMPDIR on macOS is typically already-slash-terminated
    // (e.g. "/var/folders/.../T/"); trim so join() doesn't produce "//".
    PathBuf::from(tmp.trim_end_matches('/')).join("win-sandbox-runner")
}

#[cfg(target_os = "linux")]
fn runtime_dir_base() -> PathBuf {
    PathBuf::from(RUNTIME_DIR_BASE)
}

/// Paths used by the daemon.
///
/// `pub(crate)` (not private) so `main.rs`'s binfmt_misc detection path can
/// call this instead of hardcoding a second copy of the FIFO path — this
/// project has independently hit the same "same literal duplicated across
/// files, one copy silently drifts" bug three times before (the binfmt mask
/// constant), so a second literal path string here is worth avoiding on
/// sight rather than waiting to hit it a fourth time.
pub(crate) fn fifo_path() -> PathBuf {
    runtime_dir_base().join("fifo")
}

fn socket_path() -> PathBuf {
    runtime_dir_base().join("ipc.sock")
}

fn pid_path() -> PathBuf {
    runtime_dir_base().join("daemon.pid")
}

/// Ensure the runtime directory exists with correct permissions.
fn ensure_runtime_dir() -> Result<()> {
    let dir = runtime_dir_base();
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create {} (are you root?)", dir.display()))?;
        // Allow any user to write to the FIFO (for binfmt_misc). Only
        // meaningful on Linux, where the daemon is root and other UIDs need
        // to reach the FIFO; on macOS the daemon already runs as the same
        // user that will write to it, so world-writability isn't needed —
        // but setting it anyway does no harm and keeps this one code path
        // shared instead of adding a second cfg branch for a cosmetic mode.
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o1777))?;
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

/// The binfmt_misc registration line for the APEX-WIN handler.
///
/// This duplicates `scripts/register-binfmt.sh` on purpose: the daemon
/// registers at startup and cannot assume the script is installed. Every
/// *other* path (Makefile, install.sh, .deb postinst) must call that script
/// rather than inlining a copy.
///
/// The `\xff\xff` mask is mandatory. Without it the kernel rejects the whole
/// registration with EINVAL. That bug recurred three times while this string
/// was duplicated across five files, so `binfmt_definition_matches_script`
/// asserts this constant stays byte-identical to the script's `--print`.
const BINFMT_REGISTRATION: &str =
    ":APEX-WIN:M:0:\\x4d\\x5a:\\xff\\xff:/usr/bin/win-sandbox-runner:CF";

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
    let registration = format!("{BINFMT_REGISTRATION}\n");

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
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o666))?;

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

    let listener = UnixListener::bind(&path).context("Failed to create IPC socket")?;

    // Allow any user to connect
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o666))?;

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

/// Build the JSON body for the `status` IPC command.
///
/// Pulled out of `handle_ipc_command` so the format is directly testable
/// without a running daemon (constructing a full `DaemonState` for a unit
/// test would need a real `AppDatabase`/`RulesFile`, which is heavier than
/// this warrants).
fn build_status_json(
    launch_count: u64,
    app_profiles: usize,
    rules_count: usize,
    caps: &crate::capabilities::Capabilities,
) -> String {
    format!(
        r#"{{"launch_count":{},"app_profiles":{},"rules":{},"uptime":"running","landlock_abi":{},"bwrap_version":{},"tier3_available":{},"seatbelt_available":{},"tier1_2_available":{}}}"#,
        launch_count,
        app_profiles,
        rules_count,
        caps.landlock_abi
            .map(|v| v.to_string())
            .unwrap_or_else(|| "null".to_string()),
        caps.bwrap_version
            .as_ref()
            .map(|v| format!("\"{v}\""))
            .unwrap_or_else(|| "null".to_string()),
        caps.unprivileged_overlay,
        caps.seatbelt_available
            .map(|v| v.to_string())
            .unwrap_or_else(|| "null".to_string()),
        caps.tier12_available(),
    )
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
        build_status_json(
            state.launch_count,
            state.app_db.profiles.len(),
            state.rules.entries.len(),
            &state.capabilities,
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

/// Configure a Command to switch to the given UID/GID in the child process.
///
/// Uses `pre_exec()` to call setgid/setuid before exec, so the Wine process
/// runs as the user who launched the .exe, not as root.
///
/// # Safety
/// Uses `pre_exec` which runs in the forked child. `libc::getpwuid`, `setgid`,
/// and `setuid` are async-signal-safe.
pub unsafe fn configure_child_uid(cmd: &mut std::process::Command, uid: u32) {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(move || {
            // Look up user's primary group
            let passwd = libc::getpwuid(uid);
            if !passwd.is_null() {
                let gid = (*passwd).pw_gid;
                if libc::setgid(gid) != 0 {
                    tracing::warn!(
                        "Failed to setgid({gid}): {}",
                        std::io::Error::last_os_error()
                    );
                }
            }
            if libc::setuid(uid) != 0 {
                tracing::warn!(
                    "Failed to setuid({uid}): {}",
                    std::io::Error::last_os_error()
                );
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
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

    // Root is required on Linux only, and only because binfmt_misc
    // registration (/proc/sys/fs/binfmt_misc/register) and the systemd
    // RuntimeDirectory (/run/win-sandbox-runner) both require it. Neither
    // applies on macOS: there is no binfmt_misc equivalent there (Path A —
    // Launch Services invoking `--exe` directly — is the only launch path,
    // and it already runs entirely as the invoking user; see the
    // architecture note on `runtime_dir_base()`), so the macOS daemon is a
    // per-user launchd LaunchAgent and must NOT require root — a LaunchAgent
    // that immediately failed with "must run as root" would never start.
    #[cfg(target_os = "linux")]
    {
        // SAFETY: geteuid() is always safe to call on Unix
        if unsafe { libc::geteuid() } != 0 {
            return Err(anyhow::anyhow!(
                "Daemon must run as root (needed for binfmt_misc registration)"
            ));
        }
    }

    // 1. Load state
    let state = Arc::new(Mutex::new(DaemonState::load()?));

    // 2. Setup runtime
    ensure_runtime_dir()?;
    write_pid()?;

    // 3. Register binfmt_misc (non-fatal — daemon can still serve IPC/FIFO)
    if let Err(e) = register_binfmt() {
        warn!("binfmt_misc registration failed: {e}");
        warn!("Daemon will still run; .exe interception via FIFO/IPC available.");
    }

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

    info!(
        "APEX-WIN daemon ready. Listening on {}",
        fifo_path().display()
    );

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

        // Use read_line() instead of lines() to correctly handle O_NONBLOCK.
        // BufReader::lines() treats WouldBlock as a fatal error, losing any
        // buffered partial data. read_line() preserves the internal buffer
        // across calls, so we can retry on WouldBlock without data loss.
        let mut reader = BufReader::new(fifo);
        let mut line_buf = String::new();
        let mut message_lines: Vec<String> = Vec::new();
        loop {
            line_buf.clear();
            match reader.read_line(&mut line_buf) {
                Ok(0) => {
                    // EOF — writer closed its end of the FIFO.
                    // Flush any remaining partial message.
                    flush_pending_message(&message_lines, &state);
                    break; // Re-open FIFO
                }
                Ok(_) => {
                    let text = line_buf.trim().to_string();
                    if text.is_empty() && !message_lines.is_empty() {
                        // Empty line = end of message
                        if let Some(req) = parse_launch_message(&message_lines) {
                            info!("Daemon: intercepted .exe launch: {}", req.exe_path);
                            let state = Arc::clone(&state);
                            thread::spawn(move || {
                                if let Err(e) = handle_launch(&req, &state) {
                                    error!("Launch failed for {}: {e:#}", req.exe_path);
                                }
                            });
                        }
                        message_lines.clear();
                    } else {
                        message_lines.push(text);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No data available yet — the writer may still be writing.
                    // BufReader's internal buffer preserves any partial line,
                    // so the next read_line() call will resume correctly.
                    thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                Err(e) => {
                    // Real I/O error — flush any partial message and re-open.
                    flush_pending_message(&message_lines, &state);
                    debug!("FIFO read error (writer closed): {e}");
                    break;
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

/// Flush a partial message collected from the FIFO before re-opening.
fn flush_pending_message(lines: &[String], state: &Arc<Mutex<DaemonState>>) {
    if lines.is_empty() {
        return;
    }
    if let Some(req) = parse_launch_message(lines) {
        info!("Daemon: intercepted .exe launch: {}", req.exe_path);
        let state = Arc::clone(state);
        thread::spawn(move || {
            if let Err(e) = handle_launch(&req, &state) {
                error!("Launch failed for {}: {e:#}", req.exe_path);
            }
        });
    }
}

/// A launch request received via FIFO, including user context.
#[allow(dead_code)] // uid will be used for per-child UID switching
struct LaunchRequest {
    exe_path: String,
    uid: Option<u32>,
    env: HashMap<String, String>,
}

/// Parse a multi-line FIFO message into a LaunchRequest.
/// Format: exe_path\nUID:1000\nENV:KEY=VAL\n...\n\n
fn parse_launch_message(lines: &[String]) -> Option<LaunchRequest> {
    if lines.is_empty() {
        return None;
    }
    let exe_path = lines[0].trim().to_string();
    if exe_path.is_empty() {
        return None;
    }
    let mut uid = None;
    let mut env = HashMap::new();
    for line in &lines[1..] {
        let line = line.trim();
        if let Some(uid_str) = line.strip_prefix("UID:") {
            uid = uid_str.parse().ok();
        } else if let Some(kv) = line.strip_prefix("ENV:") {
            if let Some((k, v)) = kv.split_once('=') {
                env.insert(k.to_string(), v.to_string());
            }
        }
    }
    Some(LaunchRequest { exe_path, uid, env })
}

/// Handle a single .exe launch request from the FIFO.
fn handle_launch(req: &LaunchRequest, state: &Arc<Mutex<DaemonState>>) -> Result<()> {
    // Update launch stats
    {
        let mut state = state.lock().unwrap();
        state.launch_count += 1;
        *state
            .launch_history
            .entry(req.exe_path.clone())
            .or_insert(0) += 1;
    }

    // Note: user env vars are passed via Args.user_env — no global set_var.
    // UID switching happens in the forked child process via pre_exec().

    // Hash the binary
    let hash = hasher::hash_file(&req.exe_path)?;

    // Build Args for the dispatch, passing user env and UID from FIFO message
    let args = Args {
        exe: Some(req.exe_path.clone()),
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
        user_env: req.env.clone(),
        uid: req.uid,
    };

    // Lock state for dispatch (keeps app_db and rules consistent)
    let state = state.lock().unwrap();

    dispatch::execute(&args, &req.exe_path, &hash, &state.rules, &state.app_db)?;

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

    let mut stream =
        UnixStream::connect(&path).context("Failed to connect to daemon IPC socket")?;

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

    let mut stream =
        UnixStream::connect(&path).context("Failed to connect to daemon IPC socket")?;

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

    fn caps(
        landlock_abi: Option<u8>,
        bwrap_version: Option<&str>,
        overlay: bool,
    ) -> crate::capabilities::Capabilities {
        caps_with_seatbelt(landlock_abi, bwrap_version, overlay, None)
    }

    fn caps_with_seatbelt(
        landlock_abi: Option<u8>,
        bwrap_version: Option<&str>,
        overlay: bool,
        seatbelt: Option<bool>,
    ) -> crate::capabilities::Capabilities {
        crate::capabilities::Capabilities {
            landlock_abi,
            bwrap_version: bwrap_version.map(String::from),
            unprivileged_overlay: overlay,
            seatbelt_available: seatbelt,
        }
    }

    /// `--status` output must stay valid JSON with the capability fields a
    /// caller (a future CI acceptance check, or a human running --status by
    /// hand) can parse, matching what was measured by hand on this session's
    /// machine: Landlock ABI v4/v8 depending on kernel, bwrap 0.9.0,
    /// tier3_available=false.
    #[test]
    fn status_json_is_valid_and_matches_capabilities() {
        let json = build_status_json(3, 35, 7, &caps(Some(4), Some("0.9.0"), false));
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("must be valid JSON");

        assert_eq!(parsed["launch_count"], 3);
        assert_eq!(parsed["app_profiles"], 35);
        assert_eq!(parsed["rules"], 7);
        assert_eq!(parsed["landlock_abi"], 4);
        assert_eq!(parsed["bwrap_version"], "0.9.0");
        assert_eq!(parsed["tier3_available"], false);
    }

    /// A host with no bwrap or unusable Landlock must serialize as JSON
    /// `null`, not a Rust `None` string or an invalid literal, or every
    /// consumer of --status has to special-case a malformed field.
    #[test]
    fn status_json_handles_missing_capabilities_as_null() {
        let json = build_status_json(0, 0, 0, &caps(None, None, false));
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("must be valid JSON");

        assert!(parsed["landlock_abi"].is_null());
        assert!(parsed["bwrap_version"].is_null());
        assert_eq!(parsed["tier3_available"], false);
    }

    #[test]
    fn status_json_reports_tier3_available_true() {
        let json = build_status_json(0, 0, 0, &caps(Some(4), Some("0.10.0"), true));
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["tier3_available"], true);
    }

    /// On Linux (`seatbelt_available: None`, since the field only applies to
    /// macOS), `--status` must still report `null`, not silently omit the
    /// key or coerce it to `false` — those are different facts (`false`
    /// would mean "macOS host, sandbox-exec missing").
    #[test]
    fn status_json_seatbelt_null_on_non_macos() {
        let json = build_status_json(0, 0, 0, &caps(None, None, false));
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["seatbelt_available"].is_null());
    }

    #[test]
    fn status_json_seatbelt_reports_macos_state() {
        let with = caps_with_seatbelt(None, None, false, Some(true));
        let without = caps_with_seatbelt(None, None, false, Some(false));

        let parsed_with: serde_json::Value =
            serde_json::from_str(&build_status_json(0, 0, 0, &with)).unwrap();
        let parsed_without: serde_json::Value =
            serde_json::from_str(&build_status_json(0, 0, 0, &without)).unwrap();

        assert_eq!(parsed_with["seatbelt_available"], true);
        assert_eq!(parsed_without["seatbelt_available"], false);
    }

    /// tier1_2_available must be true whenever ANY of the three underlying
    /// mechanisms (Landlock, bubblewrap, Seatbelt) is present, and false
    /// only when none are — this is the field a caller checks to answer
    /// "is any filesystem-restriction sandbox usable here at all" without
    /// having to know which OS it is running on.
    #[test]
    fn status_json_tier1_2_available_true_when_any_mechanism_present() {
        let via_landlock = caps(Some(4), None, false);
        let via_bwrap = caps(None, Some("0.9.0"), false);
        let via_seatbelt = caps_with_seatbelt(None, None, false, Some(true));

        for c in [via_landlock, via_bwrap, via_seatbelt] {
            let parsed: serde_json::Value =
                serde_json::from_str(&build_status_json(0, 0, 0, &c)).unwrap();
            assert_eq!(
                parsed["tier1_2_available"], true,
                "expected tier1_2_available=true for {c:?}"
            );
        }
    }

    #[test]
    fn status_json_tier1_2_available_false_when_nothing_present() {
        let nothing = caps_with_seatbelt(None, None, false, None);
        let parsed: serde_json::Value =
            serde_json::from_str(&build_status_json(0, 0, 0, &nothing)).unwrap();
        assert_eq!(parsed["tier1_2_available"], false);

        // Explicitly-false Seatbelt (macOS, sandbox-exec missing) must not
        // be confused with "not applicable" (None) — both correctly report
        // tier1_2_available=false here, but for different underlying facts.
        let macos_no_seatbelt = caps_with_seatbelt(None, None, false, Some(false));
        let parsed2: serde_json::Value =
            serde_json::from_str(&build_status_json(0, 0, 0, &macos_no_seatbelt)).unwrap();
        assert_eq!(parsed2["tier1_2_available"], false);
    }

    #[test]
    fn fifo_path_format() {
        assert_eq!(fifo_path(), runtime_dir_base().join("fifo"));
    }

    #[test]
    fn socket_path_format() {
        assert_eq!(socket_path(), runtime_dir_base().join("ipc.sock"));
    }

    #[test]
    fn pid_path_format() {
        assert_eq!(pid_path(), runtime_dir_base().join("daemon.pid"));
    }

    /// The Linux runtime dir must stay `/run/win-sandbox-runner` exactly —
    /// it is load-bearing: `scripts/win-sandbox-runner.service`'s
    /// `RuntimeDirectory=win-sandbox-runner` line creates precisely this
    /// path before `ExecStart`, so if this constant ever drifted from that
    /// unit file the daemon would silently fail to find its own runtime
    /// directory. This is the platform-specific half of the path-format
    /// tests above, which only check "does fifo_path() build on top of
    /// runtime_dir_base() correctly" — not "is the Linux value itself right".
    #[test]
    #[cfg(target_os = "linux")]
    fn runtime_dir_base_matches_systemd_unit_file() {
        assert_eq!(RUNTIME_DIR_BASE, "/run/win-sandbox-runner");
    }

    /// The macOS runtime dir must live under $TMPDIR (per-user, private,
    /// mode 0700), never a shared/root-owned location — the daemon there
    /// runs as an ordinary user (see `run_daemon()`'s platform split), so
    /// anywhere shared between users would be both wrong (permission
    /// errors) and a symlink-attack surface no `/run`-style tmpfs has.
    #[test]
    #[cfg(not(target_os = "linux"))]
    fn runtime_dir_base_is_under_tmpdir_not_shared() {
        let dir = runtime_dir_base();
        assert!(
            dir.ends_with("win-sandbox-runner"),
            "expected .../win-sandbox-runner, got {}",
            dir.display()
        );
        assert_ne!(
            dir,
            std::path::PathBuf::from("/var/run/win-sandbox-runner"),
            "must not resolve to a root-owned shared path"
        );
    }

    /// Exact wire format written by the binfmt handler in main().
    fn sample_fifo_message(home: &str) -> Vec<String> {
        vec![
            "/home/alice/game.exe".to_string(),
            "UID:1000".to_string(),
            format!("ENV:HOME={home}"),
            "ENV:DISPLAY=:0".to_string(),
            "ENV:XAUTHORITY=/run/user/1000/gdm/Xauthority".to_string(),
            "ENV:XDG_RUNTIME_DIR=/run/user/1000".to_string(),
        ]
    }

    #[test]
    fn parse_launch_message_extracts_uid_and_env() {
        let req = parse_launch_message(&sample_fifo_message("/home/alice")).unwrap();

        assert_eq!(req.exe_path, "/home/alice/game.exe");
        assert_eq!(req.uid, Some(1000));
        assert_eq!(req.env.get("HOME").unwrap(), "/home/alice");
        assert_eq!(req.env.get("DISPLAY").unwrap(), ":0");
        assert_eq!(
            req.env.get("XAUTHORITY").unwrap(),
            "/run/user/1000/gdm/Xauthority",
            "XAUTHORITY must survive FIFO transport or X11 auth fails"
        );
    }

    /// Ties the whole daemon chain together: FIFO bytes -> LaunchRequest.env
    /// -> PrefixManager::for_user. The daemon runs as root, so if the user's
    /// HOME is lost anywhere along this path the prefix lands in /root
    /// (permission denied) or /tmp (Wine refuses: "not owned by you") —
    /// which is exactly the failure seen in the journal.
    #[test]
    fn fifo_env_reaches_prefix_resolution() {
        let req = parse_launch_message(&sample_fifo_message("/home/alice")).unwrap();

        let prefix = crate::prefix::PrefixManager::for_user(&req.env).wine_prefix("abc123");

        assert!(
            prefix.starts_with("/home/alice/.local/share/win-sandbox/prefixes"),
            "prefix must resolve under the user's home, got {}",
            prefix.display()
        );
        assert!(!prefix.starts_with("/root"), "must not use root's home");
        assert!(!prefix.starts_with("/tmp"), "Wine refuses /tmp prefixes");
    }

    /// Values containing '=' (paths, D-Bus addresses) must not be truncated.
    #[test]
    fn parse_launch_message_preserves_equals_in_values() {
        let lines = vec![
            "/x/app.exe".to_string(),
            "ENV:DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus".to_string(),
        ];
        let req = parse_launch_message(&lines).unwrap();
        assert_eq!(
            req.env.get("DBUS_SESSION_BUS_ADDRESS").unwrap(),
            "unix:path=/run/user/1000/bus"
        );
    }

    #[test]
    fn parse_launch_message_rejects_empty() {
        assert!(parse_launch_message(&[]).is_none());
        assert!(parse_launch_message(&["".to_string()]).is_none());
    }

    /// The daemon and scripts/register-binfmt.sh both define the registration
    /// line, because the daemon cannot assume the script is installed. This
    /// test is what keeps that duplication honest: it runs the script's
    /// --print and asserts byte equality with the Rust constant.
    ///
    /// Without it, the two drift. The missing-\xff\xff-mask bug (kernel returns
    /// EINVAL) was found and fixed three separate times precisely because
    /// nothing compared the copies.
    #[test]
    fn binfmt_definition_matches_script() {
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/register-binfmt.sh");

        // Skip rather than fail when the script is absent (e.g. installed
        // crate, vendored build) so this cannot produce a false failure.
        if !script.exists() {
            eprintln!("skip: {} not present", script.display());
            return;
        }

        let out = std::process::Command::new("sh")
            .arg(&script)
            .arg("--print")
            .output()
            .expect("failed to run register-binfmt.sh --print");

        assert!(
            out.status.success(),
            "register-binfmt.sh --print failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let from_script = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(
            from_script, BINFMT_REGISTRATION,
            "binfmt definition drifted between daemon.rs and register-binfmt.sh"
        );
    }

    /// Guards the specific field that has broken three times.
    #[test]
    fn binfmt_registration_carries_mask() {
        let fields: Vec<&str> = BINFMT_REGISTRATION.split(':').collect();
        // :name:type:offset:magic:mask:interpreter:flags -> leading empty field
        assert_eq!(fields.len(), 8, "unexpected field count: {fields:?}");
        assert_eq!(fields[1], "APEX-WIN");
        assert_eq!(fields[2], "M", "must be a magic match");
        assert_eq!(fields[3], "0", "magic is at offset 0");
        assert_eq!(fields[4], r"\x4d\x5a", "MZ header");
        assert_eq!(
            fields[5], r"\xff\xff",
            "mask must be present or the kernel rejects with EINVAL"
        );
        assert!(fields[6].ends_with("win-sandbox-runner"));
        assert!(fields[7].contains('C'), "C = credential inheritance");
        assert!(fields[7].contains('F'), "F = fix binary");
    }
}
