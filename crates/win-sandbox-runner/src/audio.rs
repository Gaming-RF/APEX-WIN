use std::path::PathBuf;
use tracing::{debug, info};

/// Detected audio server type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioServer {
    /// PipeWire (modern, preferred).
    PipeWire { socket: PathBuf },
    /// PulseAudio (legacy, widely supported).
    PulseAudio { socket: PathBuf },
}

/// Detect the running audio server and return its socket path.
///
/// Priority: PipeWire → PulseAudio (native) → $PULSE_SERVER → None.
pub fn detect() -> Option<AudioServer> {
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        // PipeWire
        let pipewire_socket = format!("{runtime}/pipewire-0");
        if PathBuf::from(&pipewire_socket).exists() {
            info!("Audio: PipeWire detected at {pipewire_socket}");
            return Some(AudioServer::PipeWire {
                socket: PathBuf::from(pipewire_socket),
            });
        }

        // PulseAudio (native socket)
        let pulse_socket = format!("{runtime}/pulse/native");
        if PathBuf::from(&pulse_socket).exists() {
            info!("Audio: PulseAudio detected at {pulse_socket}");
            return Some(AudioServer::PulseAudio {
                socket: PathBuf::from(pulse_socket),
            });
        }
    }

    // PulseAudio via $PULSE_SERVER (could be TCP)
    if let Ok(pulse_server) = std::env::var("PULSE_SERVER") {
        debug!("Audio: PULSE_SERVER={pulse_server}");
        // If it's a Unix socket path
        if pulse_server.starts_with('/') && PathBuf::from(&pulse_server).exists() {
            return Some(AudioServer::PulseAudio {
                socket: PathBuf::from(pulse_server),
            });
        }
        // TCP connections are handled by env var, no socket to bind-mount
    }

    debug!("Audio: No audio server detected");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_does_not_panic() {
        let _server = detect();
    }
}
