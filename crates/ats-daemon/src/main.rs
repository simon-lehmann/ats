//! ats-daemon: owns all agent processes (PTYs), workspaces, transcripts,
//! and state. Clients (TUI, CLI) attach over a local socket.
//!
//! Runs in the foreground — supervise with systemd/tmux, or let the TUI
//! auto-start it. Killing a client never kills the daemon or its agents.

use std::sync::Arc;

use anyhow::{Context, Result};
use ats_core::config::Config;
use ats_daemon::{server, store};

fn load_config() -> Result<Config> {
    // precedence: ./ats.toml, then <data_dir>/ats.toml, then defaults
    for path in [
        std::path::PathBuf::from("ats.toml"),
        ats_core::data_dir().join("ats.toml"),
    ] {
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let mut cfg = Config::from_toml(&raw)
                .with_context(|| format!("parsing {}", path.display()))?;
            if cfg.daemon.workspaces_root.is_empty() {
                cfg.daemon.workspaces_root =
                    ats_core::data_dir().join("workspaces").to_string_lossy().into_owned();
            }
            return Ok(cfg);
        }
    }
    let mut cfg = Config::default();
    cfg.daemon.workspaces_root =
        ats_core::data_dir().join("workspaces").to_string_lossy().into_owned();
    Ok(cfg)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = load_config()?;
    let data_dir = ats_core::data_dir();
    let store = Arc::new(store::Store::open(&data_dir.join("ats.db"))?);
    let socket = config
        .daemon
        .socket_path
        .clone()
        .unwrap_or_else(ats_core::default_socket_path);

    let daemon = Arc::new(server::Daemon::new(config, store, data_dir));
    server::serve(daemon, &socket).await
}
