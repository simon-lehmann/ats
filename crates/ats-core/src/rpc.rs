//! JSON-lines RPC protocol over a local socket (Unix socket / Windows named pipe).
//!
//! Requests flow client → daemon; Events are pushed daemon → client.
//! PTY output streams only for the currently attached session — background
//! sessions accumulate into the daemon-side scrollback ring buffer.

use serde::{Deserialize, Serialize};

use crate::state::SessionState;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    // sessions
    SpawnSession {
        template_id: i64,
        tab_slot: Option<u8>,
        kickoff_note_id: Option<i64>,
    },
    AttachSession { session_id: i64 },
    DetachSession { session_id: i64 },
    WriteStdin { session_id: i64, bytes: Vec<u8> },
    ResizeSession { session_id: i64, cols: u16, rows: u16 },
    KillSession { session_id: i64 },
    GetScrollback { session_id: i64, lines: u32 },
    // workspaces
    ListTemplates,
    RegisterTemplate {
        name: String,
        path: String,
        setup_cmd: Option<String>,
    },
    SpawnWorkspace { template_id: i64 },
    ResetWorkspace { id: i64 },
    HarvestWorkspace { id: i64 },
    DestroyWorkspace { id: i64 },
    // notes & prompts
    ListNotes,
    UpsertNote {
        id: Option<i64>,
        title: String,
        body: String,
    },
    FinalizeNote { id: i64 },
    SendNoteToSession { note_id: i64, session_id: i64 },
    ListPrompts,
    UsePrompt { id: i64, session_id: i64 },
    // orchestrator
    SummarizeSession { session_id: i64, force_llm: bool },
    AskOrchestrator {
        question: String,
        session_ids: Vec<i64>,
    },
    ListReviewQueue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "params", rename_all = "snake_case")]
pub enum Event {
    PtyOutput { session_id: i64, bytes: Vec<u8> },
    SessionStateChanged {
        session_id: i64,
        state: SessionState,
        detail: Option<String>,
    },
    DigestReady { session_id: i64, summary: String },
    WorkspaceStatusChanged {
        workspace_id: i64,
        status: crate::state::WorkspaceStatus,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let req = Request::SpawnSession {
            template_id: 1,
            tab_slot: Some(3),
            kickoff_note_id: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Request::SpawnSession { template_id: 1, .. }));
    }

    #[test]
    fn event_round_trips() {
        let ev = Event::SessionStateChanged {
            session_id: 7,
            state: SessionState::NeedsInput,
            detail: Some("keep legacy API?".into()),
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Event::SessionStateChanged { session_id: 7, .. }));
    }
}
