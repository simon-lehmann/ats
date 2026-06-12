//! Orchestrator (plan §4.3): LLM digests and cross-session questions.
//!
//! Calm principle: everything here is on-demand by default; auto-digest on
//! `finished` is opt-in via config. Heuristics first — the LLM is only
//! consulted for long final reports or explicit asks.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use ats_core::config::OrchestratorConfig;
use serde_json::{json, Value};

use crate::store::Store;
use crate::transcript;

/// Final reports longer than this get an LLM digest (plan §4.2).
pub const LLM_DIGEST_THRESHOLD: usize = 400;
const DIGEST_PROMPT: &str = "Compress this agent's final report to one line, \
\u{2264}90 chars: state, blockers, what it needs from the developer. No preamble.";

pub struct Orchestrator {
    http: reqwest::Client,
    model: String,
    base_url: String,
}

impl Orchestrator {
    pub fn new(cfg: &OrchestratorConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            model: cfg.model.clone(),
            base_url: cfg
                .base_url
                .clone()
                .or_else(|| std::env::var("ANTHROPIC_BASE_URL").ok())
                .unwrap_or_else(|| "https://api.anthropic.com".into()),
        }
    }

    /// Raw /v1/messages call; returns the full response body.
    async fn messages(&self, payload: Value) -> Result<Value> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| anyhow!("ANTHROPIC_API_KEY not set — orchestrator features need it"))?;
        let resp = self
            .http
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .json(&payload)
            .send()
            .await?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        if !status.is_success() {
            bail!(
                "anthropic api {status}: {}",
                body.pointer("/error/message").and_then(Value::as_str).unwrap_or("?")
            );
        }
        Ok(body)
    }

    /// Tool-enabled call for the interactive agent loop (crate::agent).
    pub(crate) async fn messages_payload(
        &self,
        history: &[Value],
        tools: &Value,
        system: &str,
    ) -> Result<Value> {
        self.messages(json!({
            "model": self.model,
            "max_tokens": 4096,
            "system": system,
            "tools": tools,
            "messages": history,
        }))
        .await
    }

    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let body = self
            .messages(json!({
                "model": self.model,
                "max_tokens": 1024,
                "system": system,
                "messages": [{"role": "user", "content": user}],
            }))
            .await?;
        body.pointer("/content/0/text")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .ok_or_else(|| anyhow!("no text in api response"))
    }

    fn transcript_for(&self, store: &Store, session_id: i64) -> Result<PathBuf> {
        store
            .session_transcript(session_id)?
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("session {session_id} has no transcript yet"))
    }

    /// One-line digest. Heuristic (last line of the final report) unless the
    /// report is long or `force_llm`.
    pub async fn digest(
        &self,
        store: &Arc<Store>,
        session_id: i64,
        force_llm: bool,
    ) -> Result<(String, &'static str)> {
        let path = self.transcript_for(store, session_id)?;
        let final_text = tokio::task::spawn_blocking({
            let path = path.clone();
            move || transcript::final_text(&path)
        })
        .await?
        .ok_or_else(|| anyhow!("no assistant report in transcript yet"))?;

        if !force_llm && final_text.len() <= LLM_DIGEST_THRESHOLD {
            let line = final_text
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
                .to_string();
            store.insert_digest(session_id, &line, "heuristic")?;
            return Ok((line, "heuristic"));
        }
        let summary = self.complete(DIGEST_PROMPT, &final_text).await?;
        store.insert_digest(session_id, &summary, "llm")?;
        Ok((summary, "llm"))
    }

    /// Answer a question across the given sessions' transcripts.
    pub async fn ask(
        &self,
        store: &Arc<Store>,
        question: &str,
        session_ids: &[i64],
    ) -> Result<String> {
        let mut context = String::new();
        for &id in session_ids {
            let Ok(path) = self.transcript_for(store, id) else { continue };
            let info = store.get_session(id)?;
            let title = info.map(|s| s.title).unwrap_or_else(|| format!("session {id}"));
            let dialogue = tokio::task::spawn_blocking({
                let path = path.clone();
                move || transcript::recent_dialogue(&path, 6000)
            })
            .await?;
            if !dialogue.is_empty() {
                context.push_str(&format!("\n=== session {id} ({title}) ===\n{dialogue}"));
            }
        }
        if context.is_empty() {
            bail!("none of the selected sessions have transcripts");
        }
        self.complete(
            "You oversee several coding agents. Answer the developer's question \
             from the session excerpts. Be concrete and brief; cite session ids. \
             Mention changed file paths when relevant.",
            &format!("{context}\n\nQuestion: {question}"),
        )
        .await
    }

    /// Draft a catch-up note for re-entering a session; stores it as a note.
    pub async fn draft_reentry(
        &self,
        store: &Arc<Store>,
        session_id: i64,
    ) -> Result<ats_core::rpc::NoteInfo> {
        let path = self.transcript_for(store, session_id)?;
        let dialogue = tokio::task::spawn_blocking({
            let path = path.clone();
            move || transcript::recent_dialogue(&path, 8000)
        })
        .await?;
        if dialogue.is_empty() {
            bail!("transcript for session {session_id} is empty");
        }
        let body = self
            .complete(
                "Write a short re-entry briefing for a developer returning to this \
                 coding-agent session: 1) where things stand, 2) open decisions or \
                 blockers, 3) suggested next message to the agent. \u{2264}10 lines.",
                &dialogue,
            )
            .await?;
        let info = store
            .get_session(session_id)?
            .ok_or_else(|| anyhow!("no session {session_id}"))?;
        store.upsert_note(None, &format!("re-entry: {}", info.title), &body)
    }
}
