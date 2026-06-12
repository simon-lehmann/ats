//! SQLite persistence (plan §2). One connection behind a mutex — the daemon
//! is low-QPS; simplicity beats a pool here.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use ats_core::rpc::{NoteInfo, PromptInfo, SessionInfo, TemplateInfo, WorkspaceInfo};
use ats_core::state::{SessionState, WorkspaceStatus};
use rusqlite::{params, Connection, OptionalExtension};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS templates (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  path TEXT NOT NULL,
  origin_url TEXT,
  setup_cmd TEXT,
  created_at INTEGER, updated_at INTEGER
);
CREATE TABLE IF NOT EXISTS workspaces (
  id INTEGER PRIMARY KEY,
  template_id INTEGER REFERENCES templates(id),
  path TEXT NOT NULL,
  branch TEXT,
  base_commit TEXT,
  status TEXT CHECK(status IN ('spawning','ready','attached','harvesting','destroyed')),
  created_at INTEGER
);
CREATE TABLE IF NOT EXISTS sessions (
  id INTEGER PRIMARY KEY,
  workspace_id INTEGER REFERENCES workspaces(id),
  tab_slot INTEGER,
  pty_pid INTEGER,
  claude_session_id TEXT,
  transcript_path TEXT,
  state TEXT CHECK(state IN ('working','idle','finished','needs_input','error','dead')),
  state_detail TEXT,
  kickoff_note_id INTEGER,
  created_at INTEGER, last_activity_at INTEGER
);
CREATE TABLE IF NOT EXISTS notes (
  id INTEGER PRIMARY KEY,
  title TEXT, body TEXT,
  state TEXT CHECK(state IN ('draft','finalized','claimed','done')),
  pinned INTEGER DEFAULT 0,
  claimed_by_session INTEGER REFERENCES sessions(id),
  created_at INTEGER, updated_at INTEGER
);
CREATE TABLE IF NOT EXISTS prompts (
  id INTEGER PRIMARY KEY,
  label TEXT, body TEXT,
  kind TEXT CHECK(kind IN ('clipboard','reentry')),
  use_count INTEGER DEFAULT 0,
  last_used_at INTEGER
);
CREATE TABLE IF NOT EXISTS digests (
  id INTEGER PRIMARY KEY,
  session_id INTEGER REFERENCES sessions(id),
  summary TEXT,
  source TEXT CHECK(source IN ('heuristic','llm')),
  created_at INTEGER
);
"#;

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn state_to_str(s: SessionState) -> &'static str {
    match s {
        SessionState::Working => "working",
        SessionState::Idle => "idle",
        SessionState::Finished => "finished",
        SessionState::NeedsInput => "needs_input",
        SessionState::Error => "error",
        SessionState::Dead => "dead",
    }
}

fn state_from_str(s: &str) -> SessionState {
    match s {
        "working" => SessionState::Working,
        "idle" => SessionState::Idle,
        "finished" => SessionState::Finished,
        "needs_input" => SessionState::NeedsInput,
        "error" => SessionState::Error,
        _ => SessionState::Dead,
    }
}

fn ws_status_to_str(s: WorkspaceStatus) -> &'static str {
    match s {
        WorkspaceStatus::Spawning => "spawning",
        WorkspaceStatus::Ready => "ready",
        WorkspaceStatus::Attached => "attached",
        WorkspaceStatus::Harvesting => "harvesting",
        WorkspaceStatus::Destroyed => "destroyed",
    }
}

fn ws_status_from_str(s: &str) -> WorkspaceStatus {
    match s {
        "spawning" => WorkspaceStatus::Spawning,
        "ready" => WorkspaceStatus::Ready,
        "attached" => WorkspaceStatus::Attached,
        "harvesting" => WorkspaceStatus::Harvesting,
        _ => WorkspaceStatus::Destroyed,
    }
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening sqlite db at {}", path.display()))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    // ---- templates ----

    pub fn insert_template(
        &self,
        name: &str,
        path: &str,
        origin_url: Option<&str>,
        setup_cmd: Option<&str>,
    ) -> Result<TemplateInfo> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO templates (name, path, origin_url, setup_cmd, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![name, path, origin_url, setup_cmd, now()],
        )?;
        let id = conn.last_insert_rowid();
        Ok(TemplateInfo {
            id,
            name: name.into(),
            path: path.into(),
            origin_url: origin_url.map(Into::into),
            setup_cmd: setup_cmd.map(Into::into),
        })
    }

    pub fn get_template(&self, id: i64) -> Result<Option<TemplateInfo>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, name, path, origin_url, setup_cmd FROM templates WHERE id = ?1",
            params![id],
            |r| {
                Ok(TemplateInfo {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    path: r.get(2)?,
                    origin_url: r.get(3)?,
                    setup_cmd: r.get(4)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_templates(&self) -> Result<Vec<TemplateInfo>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id, name, path, origin_url, setup_cmd FROM templates ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok(TemplateInfo {
                id: r.get(0)?,
                name: r.get(1)?,
                path: r.get(2)?,
                origin_url: r.get(3)?,
                setup_cmd: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- workspaces ----

    pub fn insert_workspace(
        &self,
        template_id: i64,
        path: &str,
        status: WorkspaceStatus,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO workspaces (template_id, path, status, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![template_id, path, ws_status_to_str(status), now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn update_workspace(
        &self,
        id: i64,
        branch: Option<&str>,
        base_commit: Option<&str>,
        status: WorkspaceStatus,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE workspaces SET branch = COALESCE(?2, branch),
             base_commit = COALESCE(?3, base_commit), status = ?4 WHERE id = ?1",
            params![id, branch, base_commit, ws_status_to_str(status)],
        )?;
        Ok(())
    }

    pub fn get_workspace(&self, id: i64) -> Result<Option<WorkspaceInfo>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT w.id, w.template_id, t.name, w.path, w.branch, w.status
             FROM workspaces w JOIN templates t ON t.id = w.template_id WHERE w.id = ?1",
            params![id],
            |r| {
                Ok(WorkspaceInfo {
                    id: r.get(0)?,
                    template_id: r.get(1)?,
                    template_name: r.get(2)?,
                    path: r.get(3)?,
                    branch: r.get(4)?,
                    status: ws_status_from_str(&r.get::<_, String>(5)?),
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn workspace_base_commit(&self, id: i64) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT base_commit FROM workspaces WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT w.id, w.template_id, t.name, w.path, w.branch, w.status
             FROM workspaces w JOIN templates t ON t.id = w.template_id
             WHERE w.status != 'destroyed' ORDER BY w.id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(WorkspaceInfo {
                id: r.get(0)?,
                template_id: r.get(1)?,
                template_name: r.get(2)?,
                path: r.get(3)?,
                branch: r.get(4)?,
                status: ws_status_from_str(&r.get::<_, String>(5)?),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn count_workspaces_for_template(&self, template_id: i64) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM workspaces WHERE template_id = ?1",
            params![template_id],
            |r| r.get(0),
        )?)
    }

    // ---- sessions ----

    pub fn insert_session(
        &self,
        workspace_id: i64,
        tab_slot: Option<u8>,
        pty_pid: Option<u32>,
        kickoff_note_id: Option<i64>,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (workspace_id, tab_slot, pty_pid, state, kickoff_note_id,
             created_at, last_activity_at) VALUES (?1, ?2, ?3, 'working', ?4, ?5, ?5)",
            params![workspace_id, tab_slot, pty_pid, kickoff_note_id, now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn set_session_state(
        &self,
        id: i64,
        state: SessionState,
        detail: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET state = ?2, state_detail = ?3 WHERE id = ?1",
            params![id, state_to_str(state), detail],
        )?;
        Ok(())
    }

    pub fn set_session_pid(&self, id: i64, pid: u32) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET pty_pid = ?2 WHERE id = ?1",
            params![id, pid],
        )?;
        Ok(())
    }

    pub fn touch_session(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET last_activity_at = ?2 WHERE id = ?1",
            params![id, now()],
        )?;
        Ok(())
    }

    pub fn get_session(&self, id: i64) -> Result<Option<SessionInfo>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(&format!("{SESSION_SELECT} WHERE s.id = ?1"), params![id], session_row)
            .optional()
            .map_err(Into::into)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!("{SESSION_SELECT} ORDER BY s.id"))?;
        let rows = stmt.query_map([], session_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn next_free_tab_slot(&self, max_slots: u8) -> Result<Option<u8>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT tab_slot FROM sessions WHERE tab_slot IS NOT NULL AND state != 'dead'",
        )?;
        let used: Vec<u8> = stmt
            .query_map([], |r| r.get::<_, u8>(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok((1..=max_slots).find(|s| !used.contains(s)))
    }

    pub fn clear_dead_tab_slot(&self, session_id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET tab_slot = NULL WHERE id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    // ---- notes ----

    pub fn upsert_note(&self, id: Option<i64>, title: &str, body: &str) -> Result<NoteInfo> {
        let conn = self.conn.lock().unwrap();
        let id = match id {
            Some(id) => {
                conn.execute(
                    "UPDATE notes SET title = ?2, body = ?3, updated_at = ?4 WHERE id = ?1",
                    params![id, title, body, now()],
                )?;
                id
            }
            None => {
                conn.execute(
                    "INSERT INTO notes (title, body, state, created_at, updated_at)
                     VALUES (?1, ?2, 'draft', ?3, ?3)",
                    params![title, body, now()],
                )?;
                conn.last_insert_rowid()
            }
        };
        conn.query_row(
            "SELECT id, title, body, state, pinned, claimed_by_session FROM notes WHERE id = ?1",
            params![id],
            note_row,
        )
        .map_err(Into::into)
    }

    pub fn list_notes(&self) -> Result<Vec<NoteInfo>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, title, body, state, pinned, claimed_by_session FROM notes
             WHERE state != 'done' ORDER BY pinned DESC, updated_at DESC",
        )?;
        let rows = stmt.query_map([], note_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_note_state(&self, id: i64, state: &str, claimed_by: Option<i64>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE notes SET state = ?2, claimed_by_session = COALESCE(?3, claimed_by_session),
             updated_at = ?4 WHERE id = ?1",
            params![id, state, claimed_by, now()],
        )?;
        Ok(())
    }

    pub fn get_note(&self, id: i64) -> Result<Option<NoteInfo>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, title, body, state, pinned, claimed_by_session FROM notes WHERE id = ?1",
            params![id],
            note_row,
        )
        .optional()
        .map_err(Into::into)
    }

    // ---- prompts ----

    pub fn list_prompts(&self) -> Result<Vec<PromptInfo>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, label, body, kind, use_count FROM prompts
             ORDER BY use_count DESC, last_used_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PromptInfo {
                id: r.get(0)?,
                label: r.get(1)?,
                body: r.get(2)?,
                kind: r.get(3)?,
                use_count: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get_prompt(&self, id: i64) -> Result<Option<PromptInfo>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, label, body, kind, use_count FROM prompts WHERE id = ?1",
            params![id],
            |r| {
                Ok(PromptInfo {
                    id: r.get(0)?,
                    label: r.get(1)?,
                    body: r.get(2)?,
                    kind: r.get(3)?,
                    use_count: r.get(4)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn bump_prompt(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE prompts SET use_count = use_count + 1, last_used_at = ?2 WHERE id = ?1",
            params![id, now()],
        )?;
        Ok(())
    }
}

const SESSION_SELECT: &str = "SELECT s.id, s.workspace_id, s.tab_slot, s.pty_pid, s.state,
    s.state_detail, w.path, t.name, s.created_at, s.last_activity_at
    FROM sessions s
    JOIN workspaces w ON w.id = s.workspace_id
    JOIN templates t ON t.id = w.template_id";

fn session_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionInfo> {
    let template_name: String = r.get(7)?;
    let path: String = r.get(6)?;
    let title = std::path::Path::new(&path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or(template_name);
    Ok(SessionInfo {
        id: r.get(0)?,
        workspace_id: r.get(1)?,
        tab_slot: r.get(2)?,
        pid: r.get(3)?,
        state: state_from_str(&r.get::<_, String>(4)?),
        state_detail: r.get(5)?,
        workspace_path: path,
        title,
        created_at: r.get(8)?,
        last_activity_at: r.get(9)?,
    })
}

fn note_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<NoteInfo> {
    Ok(NoteInfo {
        id: r.get(0)?,
        title: r.get(1)?,
        body: r.get(2)?,
        state: r.get(3)?,
        pinned: r.get::<_, i64>(4)? != 0,
        claimed_by_session: r.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_workspace_session_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let t = store.insert_template("api-core", "/tmp/api-core", None, None).unwrap();
        let ws = store.insert_workspace(t.id, "/tmp/ws/api-core-1", WorkspaceStatus::Spawning).unwrap();
        store.update_workspace(ws, Some("agent/1"), Some("abc123"), WorkspaceStatus::Ready).unwrap();

        let info = store.get_workspace(ws).unwrap().unwrap();
        assert_eq!(info.template_name, "api-core");
        assert_eq!(info.branch.as_deref(), Some("agent/1"));
        assert_eq!(info.status, WorkspaceStatus::Ready);
        assert_eq!(store.workspace_base_commit(ws).unwrap().as_deref(), Some("abc123"));

        let sid = store.insert_session(ws, Some(1), Some(4242), None).unwrap();
        let s = store.get_session(sid).unwrap().unwrap();
        assert_eq!(s.state, SessionState::Working);
        assert_eq!(s.title, "api-core-1");
        assert_eq!(s.tab_slot, Some(1));

        store.set_session_state(sid, SessionState::Dead, None).unwrap();
        assert_eq!(store.get_session(sid).unwrap().unwrap().state, SessionState::Dead);
    }

    #[test]
    fn tab_slot_allocation_skips_used() {
        let store = Store::open_in_memory().unwrap();
        let t = store.insert_template("t", "/tmp/t", None, None).unwrap();
        let ws = store.insert_workspace(t.id, "/tmp/ws1", WorkspaceStatus::Ready).unwrap();
        store.insert_session(ws, Some(1), None, None).unwrap();
        store.insert_session(ws, Some(2), None, None).unwrap();
        assert_eq!(store.next_free_tab_slot(10).unwrap(), Some(3));

        // dead sessions free their slot
        let s2 = store.insert_session(ws, Some(3), None, None).unwrap();
        store.set_session_state(s2, SessionState::Dead, None).unwrap();
        assert_eq!(store.next_free_tab_slot(10).unwrap(), Some(3));
    }

    #[test]
    fn notes_lifecycle() {
        let store = Store::open_in_memory().unwrap();
        let n = store.upsert_note(None, "plan", "do the thing").unwrap();
        assert_eq!(n.state, "draft");
        store.set_note_state(n.id, "finalized", None).unwrap();
        let n2 = store.upsert_note(Some(n.id), "plan", "do the thing v2").unwrap();
        assert_eq!(n2.body, "do the thing v2");
        let all = store.list_notes().unwrap();
        assert_eq!(all.len(), 1);
    }
}
