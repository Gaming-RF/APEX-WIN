#![allow(dead_code)]

mod ipc;
mod ui;

use anyhow::Result;
use tracing::info;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .init();

    info!("win-sandbox-gui starting");

    // TODO: Initialize GTK4 application
    // TODO: Set up D-Bus listener (org.wine.SandboxRunner)
    // TODO: Set up Unix socket fallback
    // TODO: Enter GTK main loop

    todo!("GUI not yet implemented")
}
