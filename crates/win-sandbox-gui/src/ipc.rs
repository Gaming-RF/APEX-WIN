use anyhow::Result;
use win_sandbox_common::message::IpcMessage;

/// IPC transport for communication between CLI runner and GUI.
#[derive(Debug)]
pub enum IpcTransport {
    /// D-Bus (primary, standard on Linux desktops).
    DBus,
    /// Unix socket fallback (for headless systems).
    UnixSocket { path: String },
}

/// Start listening for IPC messages from the CLI runner.
pub fn start_listener(transport: IpcTransport) -> Result<()> {
    match transport {
        IpcTransport::DBus => {
            // TODO: Set up D-Bus connection on org.wine.SandboxRunner
            todo!("D-Bus IPC not yet implemented")
        }
        IpcTransport::UnixSocket { path: _ } => {
            // TODO: Create Unix socket at path and listen for connections
            todo!("Unix socket IPC not yet implemented")
        }
    }
}

/// Send a message to the GUI and wait for a response.
pub fn send_message(_transport: &IpcTransport, _msg: &IpcMessage) -> Result<IpcMessage> {
    // TODO: Serialize message and send over transport
    // TODO: Wait for and deserialize response
    todo!("IPC send not yet implemented")
}
