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

/// Runtime directory base for macOS: `$TMPDIR/win-sandbox-runner`.
///
/// `$TMPDIR` on macOS is a per-user, private directory (mode 0700, created
/// by the OS at login), which is what makes it safe to host a FIFO and an
/// IPC socket that control process launches.
///
/// There is deliberately NO fallback to `/tmp`. An earlier version of this
/// function fell back to `/tmp` when `$TMPDIR` was unset, which was wrong:
/// `/tmp` is mode 1777 and shared by every user on the machine, so the
/// daemon's FIFO (created 0666) and IPC socket would have been reachable by
/// any local user, and the containing directory would have been a symlink-
/// swap target. `$TMPDIR`'s 0700 parent is precisely the property that
/// makes the 1777 mode set in `ensure_runtime_dir` harmless; `/tmp` has no
/// such parent, so the same mode there is a real exposure rather than a
/// cosmetic one. Failing loudly is correct: launchd always sets `$TMPDIR`,
/// so an unset value means something is wrong with the environment, and
/// silently downgrading to a world-writable location is not a safe
/// interpretation of "something is wrong".
#[cfg(not(target_os = "linux"))]
fn runtime_dir_base_checked() -> Result<PathBuf> {
    let tmp = std::env::var("TMPDIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .context(
            "TMPDIR is not set. The macOS daemon stores its FIFO and IPC socket there \
             because it is private to your user (mode 0700). Refusing to fall back to \
             /tmp, which every user on this machine can write to. launchd always sets \
             TMPDIR; if you are running the daemon by hand, set it first.",
        )?;
    // TMPDIR on macOS is typically already-slash-terminated
    // (e.g. "/var/folders/.../T/"); trim so join() doesn't produce "//".
    Ok(PathBuf::from(tmp.trim_end_matches('/')).join("win-sandbox-runner"))
}

#[cfg(target_os = "linux")]
fn runtime_dir_base_checked() -> Result<PathBuf> {
    Ok(PathBuf::from(RUNTIME_DIR_BASE))
}

/// Infallible view of [`runtime_dir_base_checked`], for the paths that only
/// build a path string and cannot meaningfully fail (`--status`/`--stop`
/// clients, cleanup on shutdown). If `$TMPDIR` is missing on macOS this
/// yields a path under a non-existent-by-design directory, so those callers
/// simply find no daemon, which is the correct outcome: without `$TMPDIR`
/// the daemon refused to start in the first place.
fn runtime_dir_base() -> PathBuf {
    runtime_dir_base_checked().unwrap_or_else(|_| {
        // Not a usable runtime dir, and deliberately not /tmp: a path that
        // cannot exist is safer than one every local user can write to.
        PathBuf::from("/nonexistent/win-sandbox-runner")
    })
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
    // Checked variant: on macOS this refuses (rather than silently using a
    // world-writable /tmp) when $TMPDIR is missing. This is the daemon
    // startup path, so failing here correctly prevents the daemon from
    // running at all with an unsafe runtime directory.
    let dir = runtime_dir_base_checked()?;
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create {} (are you root?)", dir.display()))?;
        // Linux: the daemon runs as root while the users launching .exe
        // files do not, and they must be able to write the FIFO, so the
        // directory is 1777 (sticky, like /tmp) by necessity.
        //
        // macOS: the daemon runs as the same unprivileged user that writes
        // to the FIFO, so no cross-user access is needed at all. 0700 is
        // both sufficient and strictly safer. An earlier version shared the
        // 1777 path across both platforms "because it does no harm" — that
        // was only true while the parent was $TMPDIR (0700); it granted
        // real cross-user access the moment the path was anywhere else.
        // Granting the minimum each platform actually requires removes the
        // dependency on that assumption entirely.
        #[cfg(target_os = "linux")]
        let mode = 0o1777;
        #[cfg(not(target_os = "linux"))]
        let mode = 0o700;
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(mode))?;
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

    // Linux: the daemon is root and the users launching .exe files are not,
    // so the FIFO must be writable by them (0666); the binfmt_misc handler
    // in main.rs writes launch requests to it as the invoking user.
    //
    // macOS: there is no binfmt_misc, the daemon runs as the same
    // unprivileged user that would write to the FIFO, and nothing else ever
    // writes to it, so 0600 is sufficient. Anything wider would let any
    // local user submit launch requests to this user's daemon for no
    // benefit.
    // Kept as u32 (what PermissionsExt::from_mode takes) and narrowed only
    // at the mkfifo call. libc::mode_t is u32 on Linux but u16 on macOS, so
    // typing this as mode_t instead would make the from_mode call need a
    // widening conversion that is mandatory on macOS and a
    // clippy::useless_conversion error on Linux. Converting in one
    // direction only, at the single point that needs it, avoids that.
    #[cfg(target_os = "linux")]
    let fifo_mode: u32 = 0o666;
    #[cfg(not(target_os = "linux"))]
    let fifo_mode: u32 = 0o600;

    unsafe {
        let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        let ret = libc::mkfifo(c_path.as_ptr(), fifo_mode as libc::mode_t);
        if ret != 0 {
            return Err(anyhow::anyhow!(
                "Failed to create FIFO at {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
    }

    // mkfifo's mode is masked by the process umask, so set it explicitly
    // afterwards to get the intended bits regardless of the umask the
    // daemon inherited.
    std::fs::set_permissions(
        &path,
        std::os::unix::fs::PermissionsExt::from_mode(fifo_mode),
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

    let args = args_for_launch_request(req);

    // Lock state for dispatch (keeps app_db and rules consistent)
    let state = state.lock().unwrap();

    dispatch::execute(&args, &req.exe_path, &hash, &state.rules, &state.app_db)?;

    Ok(())
}

/// Build the `Args` used to dispatch a FIFO-originated launch request.
/// Extracted out of `handle_launch` so the fields that matter for
/// correctness on this code path -- especially `no_gui` -- are pinned by a
/// direct unit test instead of only being verifiable by reading the source.
///
/// `no_gui` is unconditionally `true`: this always runs on the daemon's
/// background FIFO-reader thread, which has no controlling terminal to
/// prompt on regardless of what `req.uid` contains (that field can
/// legitimately be `None` if the FIFO writer sent a malformed or missing
/// `UID:` line -- see `parse_launch_message` -- so it must not be relied on
/// as the signal for "is this the daemon"). This was previously hardcoded
/// to `false`, which was silently harmless only because `wizard.rs`'s
/// interactive prompt was unimplemented; now that the prompt does
/// something, leaving this `false` would hang every daemon-dispatched
/// launch of an unknown `.exe` waiting on stdin input that can never
/// arrive.
fn args_for_launch_request(req: &LaunchRequest) -> Args {
    Args {
        exe: Some(req.exe_path.clone()),
        tier: None,
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
        print_seatbelt_profile: false,
        wine_prefix: None,
        user_env: req.env.clone(),
        uid: req.uid,
    }
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

    /// The exact regression this project already made once: `no_gui` on a
    /// FIFO-dispatched request was hardcoded `false`, which is only safe
    /// because a background daemon thread has no controlling terminal. As
    /// long as `wizard.rs`'s interactive prompt was unimplemented, `false`
    /// here was silently harmless; it stopped being harmless the moment the
    /// prompt did something. Pinned here as a real assertion instead of a
    /// property only checkable by reading the source.
    #[test]
    fn launch_request_args_never_enable_gui_prompts() {
        let req = LaunchRequest {
            exe_path: "/home/alice/game.exe".to_string(),
            uid: Some(1000),
            env: HashMap::new(),
        };
        let args = args_for_launch_request(&req);
        assert!(
            args.no_gui,
            "a FIFO-dispatched launch must never enable the interactive wizard prompt, \
             which would hang the daemon's background thread waiting on stdin"
        );
    }

    /// The specific edge case that makes `req.uid` unsafe as a proxy for
    /// "this came from the daemon": it can genuinely be `None` (a malformed
    /// or missing `UID:` line in the FIFO message -- see
    /// `parse_launch_message`), yet this is still unambiguously a
    /// daemon-dispatched request and must still never prompt.
    #[test]
    fn launch_request_args_never_enable_gui_prompts_even_with_missing_uid() {
        let req = LaunchRequest {
            exe_path: "/home/alice/game.exe".to_string(),
            uid: None,
            env: HashMap::new(),
        };
        let args = args_for_launch_request(&req);
        assert!(args.no_gui);
    }

    #[test]
    fn launch_request_args_forward_exe_path_uid_and_env() {
        let mut env = HashMap::new();
        env.insert("DISPLAY".to_string(), ":0".to_string());
        let req = LaunchRequest {
            exe_path: "/home/alice/game.exe".to_string(),
            uid: Some(1000),
            env: env.clone(),
        };
        let args = args_for_launch_request(&req);
        assert_eq!(args.exe.as_deref(), Some("/home/alice/game.exe"));
        assert_eq!(args.uid, Some(1000));
        assert_eq!(args.user_env, env);
    }

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

    /// The previous version of this test asserted only that the path wasn't
    /// `/var/run`, while its own doc comment claimed "never a shared
    /// location". That gap let a `/tmp` fallback ship: `/tmp` is mode 1777
    /// and shared by every local user, exactly the thing the comment said
    /// was forbidden, and no assertion contradicted it. These cases pin the
    /// actual claim down.
    #[test]
    #[cfg(not(target_os = "linux"))]
    fn runtime_dir_base_never_resolves_to_a_world_writable_dir() {
        let dir = runtime_dir_base();
        for shared in [
            "/tmp",
            "/var/tmp",
            "/private/tmp",
            "/var/run",
            "/private/var/run",
        ] {
            assert!(
                !dir.starts_with(shared),
                "runtime dir {} is under {shared}, which every local user can write to; \
                 the FIFO and IPC socket there control process launches",
                dir.display()
            );
        }
    }

    /// With TMPDIR unset the daemon must refuse to start, not silently pick
    /// a world-writable directory. This is the check that would have caught
    /// the `/tmp` fallback directly.
    #[test]
    #[cfg(not(target_os = "linux"))]
    fn runtime_dir_base_checked_refuses_when_tmpdir_missing() {
        // Safety: single-threaded test process; restored before returning.
        let saved = std::env::var("TMPDIR").ok();

        std::env::remove_var("TMPDIR");
        let missing = runtime_dir_base_checked();

        std::env::set_var("TMPDIR", "   ");
        let blank = runtime_dir_base_checked();

        std::env::set_var("TMPDIR", "/var/folders/ab/cd/T/");
        let ok = runtime_dir_base_checked();

        match saved {
            Some(v) => std::env::set_var("TMPDIR", v),
            None => std::env::remove_var("TMPDIR"),
        }

        assert!(missing.is_err(), "unset TMPDIR must refuse, not fall back");
        assert!(blank.is_err(), "blank TMPDIR must refuse, not fall back");
        let ok = ok.expect("a real TMPDIR must be accepted");
        assert_eq!(
            ok,
            std::path::PathBuf::from("/var/folders/ab/cd/T/win-sandbox-runner"),
            "trailing slash on TMPDIR must not produce a doubled separator"
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
    ///
    /// Linux-only: this shells out to `sh scripts/register-binfmt.sh
    /// --print`, whose `echo "$DEFINITION"` behavior is not portable. Caught
    /// live on the CI macOS runner: `/bin/sh` there is bash running in POSIX
    /// mode, whose `echo` DOES interpret `\xHH` escapes, while Linux's
    /// `/bin/sh` (dash) does NOT -- so the same script prints literally
    /// different bytes (`\x4d\x5a` vs. the raw bytes 0x4d 0x5a) depending on
    /// which OS runs it. binfmt_misc itself only exists on Linux anyway, so
    /// this test asserting Linux-specific shell behavior is correctly scoped
    /// to Linux, not a bug to paper over with a portable echo elsewhere.
    #[test]
    #[cfg(target_os = "linux")]
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
