use anyhow::Result;
use std::os::unix::io::{AsRawFd, RawFd};
use tracing::{debug, error, info};

/// Global pipe file descriptor for self-pipe trick (SIGCHLD cleanup).
/// The signal handler writes a byte here; the main loop reads it.
static mut CLEANUP_PIPE_FD: Option<(RawFd, RawFd)> = None;

/// Static mount path for atexit cleanup callback.
static mut CLEANUP_MOUNT_PATH: Option<&'static str> = None;

/// Initialize the self-pipe trick for async-signal-safe SIGCHLD handling.
///
/// Returns the read end of the pipe, which should be polled in the main loop.
pub fn init_cleanup_pipe() -> Result<RawFd> {
    use nix::unistd::pipe;
    let (read_fd, write_fd) = pipe()?;
    let read_raw = read_fd.as_raw_fd();
    let write_raw = write_fd.as_raw_fd();
    // Leak the OwnedFds so they stay open for the lifetime of the process.
    // The raw fds are stored in the static for the signal handler.
    std::mem::forget(read_fd);
    std::mem::forget(write_fd);
    unsafe {
        CLEANUP_PIPE_FD = Some((read_raw, write_raw));
    }
    debug!("Cleanup pipe initialized: read={read_raw}, write={write_raw}");
    Ok(read_raw)
}

/// Install SIGCHLD handler that writes to the self-pipe.
///
/// # Safety
/// This uses unsafe signal handling. The handler only writes a single byte
/// to the pipe (async-signal-safe operation).
pub fn install_sigchld_handler() -> Result<()> {
    use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, Signal};

    let _write_fd = unsafe { CLEANUP_PIPE_FD.expect("cleanup pipe not initialized").1 };

    extern "C" fn sigchld_handler(_sig: libc::c_int) {
        unsafe {
            if let Some((_, write_fd)) = CLEANUP_PIPE_FD {
                let byte = 0u8;
                // write() is async-signal-safe
                libc::write(write_fd, &byte as *const u8 as *const libc::c_void, 1);
            }
        }
    }

    let action = SigAction::new(
        SigHandler::Handler(sigchld_handler),
        SaFlags::SA_RESTART | SaFlags::SA_NOCLDSTOP,
        nix::sys::signal::SigSet::empty(),
    );

    unsafe {
        sigaction(Signal::SIGCHLD, &action)?;
    }
    info!("SIGCHLD handler installed");
    Ok(())
}

/// Install atexit handler and panic hook for overlay cleanup.
pub fn install_cleanup_hooks(mount_path: String) {
    // Leak the string so the static reference lives forever.
    // This is intentional — the process exits after wine finishes.
    let static_path: &'static str = Box::leak(mount_path.into_boxed_str());

    unsafe {
        CLEANUP_MOUNT_PATH = Some(static_path);
        libc::atexit(cleanup_atexit);
    }

    // Register panic hook
    let path = static_path.to_string();
    std::panic::set_hook(Box::new(move |_info| {
        error!("Panic occurred, cleaning up overlay mount");
        cleanup_overlay(&path);
    }));

    debug!("Cleanup hooks installed for: {static_path}");
}

/// atexit callback — reads mount path from static and unmounts.
extern "C" fn cleanup_atexit() {
    unsafe {
        if let Some(path) = CLEANUP_MOUNT_PATH {
            cleanup_overlay(path);
        }
    }
}

/// Attempt to unmount and remove an OverlayFS mount.
pub fn cleanup_overlay(mount_path: &str) {
    debug!("Cleaning up overlay mount: {mount_path}");

    // Try unmount
    use nix::mount::{umount2, MntFlags};
    if let Err(e) = umount2(mount_path, MntFlags::MNT_DETACH) {
        error!("Failed to unmount {mount_path}: {e}");
    }

    // Try to remove the directory tree
    let base = std::path::Path::new(mount_path);
    if let Some(parent) = base.parent() {
        if let Err(e) = std::fs::remove_dir_all(parent) {
            error!("Failed to remove overlay dir {}: {e}", parent.display());
        }
    }

    info!("Overlay cleanup complete: {mount_path}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_pipe_init() {
        let fd = init_cleanup_pipe().unwrap();
        assert!(fd >= 0);
    }
}
