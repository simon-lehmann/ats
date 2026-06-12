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
        // Named pipe in file-path form: every call site builds the name with
        // `GenericFilePath`, which requires a `\\.\pipe\...` path on Windows.
        r"\\.\pipe\ats".to_string()
    }
}

/// Run the Claude Code CLI with `args`, returning its captured output. On
/// Windows this goes through `cmd /C` so a `claude.cmd`/`claude.ps1` shim on
/// PATH resolves (a bare `Command::new("claude")` would not find it).
pub fn run_claude(args: &[&str]) -> std::io::Result<std::process::Output> {
    #[cfg(windows)]
    {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg("claude").args(args);
        c.output()
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("claude").args(args).output()
    }
}

/// The `claude mcp add` command that registers the ATS MCP server at user
/// scope for the given loopback port (shown to the user, run by the CLI).
pub fn mcp_register_args(port: u16) -> Vec<String> {
    vec![
        "mcp".into(),
        "add".into(),
        "--scope".into(),
        "user".into(),
        "--transport".into(),
        "http".into(),
        "ats".into(),
        format!("http://127.0.0.1:{port}/mcp"),
    ]
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
