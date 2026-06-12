//! Shared types for ATS: RPC protocol, session state, config.
//!
//! This crate is the contract between the daemon, the TUI, and the CLI.
//! See docs/PLAN.md §3 for the protocol design.

pub mod b64;
pub mod client;
pub mod config;
pub mod rpc;
pub mod state;

/// Default local socket name. On Unix this is a filesystem path; on Windows
/// it is a named-pipe name (use `GenericNamespaced` there).
pub fn default_socket_path() -> String {
    if let Ok(p) = std::env::var("ATS_SOCKET") {
        return p;
    }
    #[cfg(unix)]
    {
        if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
            return format!("{dir}/ats.sock");
        }
        format!("/tmp/ats-{}.sock", std::env::var("USER").unwrap_or_else(|_| "user".into()))
    }
    #[cfg(windows)]
    {
        "ats.sock".to_string()
    }
}

/// Data directory for the daemon (SQLite db, harvest patches, logs).
pub fn data_dir() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("ATS_DATA_DIR") {
        return p.into();
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    std::path::Path::new(&home).join(".ats")
}
