//! `ats.toml` configuration. See docs/PLAN.md §8.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub daemon: DaemonConfig,
    pub orchestrator: OrchestratorConfig,
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    pub workspaces_root: String,
    pub scrollback_lines: u32,
    pub idle_threshold_secs: u64,
    /// Command launched in each session's PTY (the agent CLI).
    pub session_cmd: String,
    /// Override the local socket path/name (default: see `default_socket_path`).
    pub socket_path: Option<String>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            workspaces_root: String::new(),
            scrollback_lines: 10_000,
            idle_threshold_secs: 8,
            session_cmd: "claude".into(),
            socket_path: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OrchestratorConfig {
    pub model: String,
    /// Calm by default: digests are on-demand unless this is enabled.
    pub auto_digest: bool,
    /// API endpoint override (tests, proxies). Default: https://api.anthropic.com
    pub base_url: Option<String>,
    /// Serve the ATS tools as an MCP server (loopback) for Claude Code
    /// sessions — the orchestrator drives the daemon through this.
    pub mcp_enabled: bool,
    /// Loopback port for the MCP HTTP endpoint (`127.0.0.1:<port>/mcp`).
    pub mcp_port: u16,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            model: "claude-haiku-4-5".into(),
            auto_digest: false,
            base_url: None,
            mcp_enabled: true,
            mcp_port: 8765,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub rail_width: u16,
    pub group_a_slots: u8,
    pub group_b_slots: u8,
    /// calm per-template tab tinting: template name → color name
    /// (e.g. `demo-app = "cyan"`); unknown names fall back to the default
    pub template_colors: std::collections::HashMap<String, String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            rail_width: 28,
            group_a_slots: 5,
            group_b_slots: 5,
            template_colors: std::collections::HashMap::new(),
        }
    }
}

impl Config {
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_gives_defaults() {
        let cfg = Config::from_toml("").unwrap();
        assert_eq!(cfg.daemon.idle_threshold_secs, 8);
        assert!(!cfg.orchestrator.auto_digest);
        assert_eq!(cfg.ui.group_a_slots, 5);
    }
}
