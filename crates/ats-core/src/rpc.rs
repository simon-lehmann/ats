//! JSON-lines RPC protocol over a local socket (Unix socket / Windows named pipe).
//!
//! Wire format: one JSON object per line.
//! - client → daemon: `{"id": N, "method": "...", "params": {...}}`
//! - daemon → client: `{"id": N, "result": {...}}` or `{"id": N, "error": "..."}`
//!   for responses, `{"event": "...", "params": {...}}` for pushed events.
//!
//! PTY output streams only for sessions the client has attached — background
//! sessions accumulate into the daemon-side scrollback ring buffer.

use serde::{Deserialize, Serialize};

use crate::state::{SessionState, WorkspaceStatus};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    // sessions
    ListSessions,
    SpawnSession {
        template_id: i64,
        tab_slot: Option<u8>,
        kickoff_note_id: Option<i64>,
    },
    AttachSession { session_id: i64 },
    DetachSession { session_id: i64 },
    WriteStdin {
        session_id: i64,
        #[serde(with = "crate::b64")]
        bytes: Vec<u8>,
    },
    ResizeSession { session_id: i64, cols: u16, rows: u16 },
    KillSession { session_id: i64 },
    GetScrollback { session_id: i64 },
    // workspaces
    ListTemplates,
    RegisterTemplate {
        name: String,
        path: String,
        setup_cmd: Option<String>,
    },
    ListWorkspaces,
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
    UpsertPrompt {
        id: Option<i64>,
        label: String,
        body: String,
        kind: String,
    },
    UsePrompt { id: i64, session_id: i64 },
    // orchestrator
    SummarizeSession { session_id: i64, force_llm: bool },
    AskOrchestrator {
        question: String,
        session_ids: Vec<i64>,
    },
    ListReviewQueue,
}

/// A request with its correlation id, as sent on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Response>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RpcResponse {
    pub fn ok(id: u64, result: Response) -> Self {
        Self { id, result: Some(result), error: None }
    }
    pub fn err(id: u64, msg: impl Into<String>) -> Self {
        Self { id, result: None, error: Some(msg.into()) }
    }
}

/// Successful result payloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Sessions { sessions: Vec<SessionInfo> },
    Session { session: SessionInfo },
    Templates { templates: Vec<TemplateInfo> },
    Template { template: TemplateInfo },
    Workspaces { workspaces: Vec<WorkspaceInfo> },
    Workspace { workspace: WorkspaceInfo },
    Notes { notes: Vec<NoteInfo> },
    Note { note: NoteInfo },
    Prompts { prompts: Vec<PromptInfo> },
    /// Raw accumulated PTY output; feed it to a fresh vt100 parser to
    /// reconstruct the screen, then apply live `PtyOutput` events.
    Scrollback {
        session_id: i64,
        #[serde(with = "crate::b64")]
        data: Vec<u8>,
    },
    Harvest {
        workspace_id: i64,
        diff_stat: String,
        patch_path: String,
    },
    Digest { session_id: i64, summary: String },
}

/// Events pushed daemon → client (no correlation id).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "params", rename_all = "snake_case")]
pub enum Event {
    PtyOutput {
        session_id: i64,
        #[serde(with = "crate::b64")]
        bytes: Vec<u8>,
    },
    SessionStateChanged {
        session_id: i64,
        state: SessionState,
        detail: Option<String>,
    },
    DigestReady { session_id: i64, summary: String },
    WorkspaceStatusChanged {
        workspace_id: i64,
        status: WorkspaceStatus,
    },
}

/// Anything the daemon writes to a client: a response or a pushed event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerMessage {
    Event(Event),
    Response(RpcResponse),
}

// ---- info structs ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: i64,
    pub workspace_id: i64,
    pub tab_slot: Option<u8>,
    pub pid: Option<u32>,
    pub title: String,
    pub state: SessionState,
    pub state_detail: Option<String>,
    pub workspace_path: String,
    pub created_at: i64,
    pub last_activity_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateInfo {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub origin_url: Option<String>,
    pub setup_cmd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: i64,
    pub template_id: i64,
    pub template_name: String,
    pub path: String,
    pub branch: Option<String>,
    pub status: WorkspaceStatus,
    /// live `git status` figures; None when git info is unavailable
    #[serde(default)]
    pub dirty: Option<u32>,
    #[serde(default)]
    pub ahead: Option<u32>,
    #[serde(default)]
    pub behind: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteInfo {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub state: String,
    pub pinned: bool,
    pub claimed_by_session: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptInfo {
    pub id: i64,
    pub label: String,
    pub body: String,
    pub kind: String,
    pub use_count: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let req = RpcRequest {
            id: 42,
            request: Request::SpawnSession {
                template_id: 1,
                tab_slot: Some(3),
                kickoff_note_id: None,
            },
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: RpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, 42);
        assert!(matches!(back.request, Request::SpawnSession { template_id: 1, .. }));
    }

    #[test]
    fn stdin_bytes_are_base64_on_the_wire() {
        let req = RpcRequest {
            id: 1,
            request: Request::WriteStdin { session_id: 2, bytes: b"hi\r".to_vec() },
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("aGkN"), "expected base64 payload in {json}");
        let back: RpcRequest = serde_json::from_str(&json).unwrap();
        match back.request {
            Request::WriteStdin { bytes, .. } => assert_eq!(bytes, b"hi\r"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn server_message_distinguishes_event_and_response() {
        let ev = ServerMessage::Event(Event::SessionStateChanged {
            session_id: 7,
            state: SessionState::NeedsInput,
            detail: Some("keep legacy API?".into()),
        });
        let json = serde_json::to_string(&ev).unwrap();
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ServerMessage::Event(_)));

        let resp = ServerMessage::Response(RpcResponse::ok(5, Response::Ok));
        let json = serde_json::to_string(&resp).unwrap();
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        match back {
            ServerMessage::Response(r) => assert_eq!(r.id, 5),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn error_response_round_trips() {
        let resp = RpcResponse::err(9, "no such session");
        let json = serde_json::to_string(&resp).unwrap();
        let back: RpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.error.as_deref(), Some("no such session"));
        assert!(back.result.is_none());
    }
}
