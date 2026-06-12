use serde::{Deserialize, Serialize};

/// State of a Claude Code session, shown as one dim glyph in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// `·` — output flowing
    Working,
    /// `○` — no recent output, no clear terminal condition
    Idle,
    /// `●` — agent produced a final report
    Finished,
    /// `!` — agent asked a question or requested permission
    NeedsInput,
    /// `!` — something went wrong
    Error,
    /// process exited
    Dead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStatus {
    Spawning,
    Ready,
    Attached,
    Harvesting,
    Destroyed,
}
