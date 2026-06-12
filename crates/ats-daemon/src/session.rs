//! SessionManager: one PTY per session (plan §4.1).
//!
//! A blocking reader thread per session drains the PTY into a raw-byte
//! scrollback ring and broadcasts `PtyOutput` on the daemon event bus when at
//! least one client is attached. State for Phase 1 is heartbeat-based:
//! output flowing → working, quiet past the threshold → idle, exit → dead.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use ats_core::rpc::Event;
use ats_core::state::SessionState;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tokio::sync::broadcast;

use crate::store::{now, Store};

const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 36;

/// Raw-byte scrollback with a hard cap; oldest output is discarded.
pub struct ScrollbackRing {
    buf: Vec<u8>,
    cap: usize,
}

impl ScrollbackRing {
    pub fn new(cap: usize) -> Self {
        Self { buf: Vec::new(), cap }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        if self.buf.len() > self.cap {
            let cut = self.buf.len() - self.cap / 2;
            self.buf.drain(..cut);
        }
    }

    pub fn snapshot(&self) -> Vec<u8> {
        self.buf.clone()
    }
}

struct SessionHandle {
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send>>,
    scrollback: Arc<Mutex<ScrollbackRing>>,
    attach_count: Arc<AtomicUsize>,
    /// unix seconds of last PTY output (in-memory hot path; DB is best-effort)
    last_output_at: Arc<AtomicI64>,
    state: Mutex<SessionState>,
}

pub struct SessionManager {
    sessions: Mutex<HashMap<i64, Arc<SessionHandle>>>,
    events: broadcast::Sender<Event>,
    scrollback_bytes: usize,
}

impl SessionManager {
    pub fn new(events: broadcast::Sender<Event>, scrollback_lines: u32) -> Self {
        // rough sizing: ~200 bytes per line of ANSI-laden output
        let scrollback_bytes = (scrollback_lines as usize).saturating_mul(200).max(64 * 1024);
        Self {
            sessions: Mutex::new(HashMap::new()),
            events,
            scrollback_bytes,
        }
    }

    /// Spawn `cmd` (run through the platform shell, so quoting and args work)
    /// in a PTY at `cwd`, register under `session_id`, and start the reader
    /// thread. Returns the child pid.
    pub fn spawn(
        &self,
        session_id: i64,
        cmd: &str,
        cwd: &str,
        store: Arc<Store>,
    ) -> Result<Option<u32>> {
        if cmd.trim().is_empty() {
            return Err(anyhow!("empty session_cmd"));
        }

        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: DEFAULT_ROWS,
                cols: DEFAULT_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| anyhow!("openpty: {e}"))?;

        #[cfg(unix)]
        let mut builder = {
            let mut b = CommandBuilder::new("sh");
            b.args(["-c", cmd]);
            b
        };
        #[cfg(windows)]
        let mut builder = {
            let mut b = CommandBuilder::new("pwsh");
            b.args(["-NoProfile", "-Command", cmd]);
            b
        };
        builder.cwd(cwd);
        let mut child = pair
            .slave
            .spawn_command(builder)
            .map_err(|e| anyhow!("spawning '{cmd}' in {cwd}: {e}"))?;
        drop(pair.slave);

        let pid = child.process_id();
        let killer = child.clone_killer();
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow!("clone pty reader: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| anyhow!("take pty writer: {e}"))?;

        let scrollback = Arc::new(Mutex::new(ScrollbackRing::new(self.scrollback_bytes)));
        let attach_count = Arc::new(AtomicUsize::new(0));
        let last_output_at = Arc::new(AtomicI64::new(now()));

        let handle = Arc::new(SessionHandle {
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            killer: Mutex::new(killer),
            scrollback: scrollback.clone(),
            attach_count: attach_count.clone(),
            last_output_at: last_output_at.clone(),
            state: Mutex::new(SessionState::Working),
        });
        self.sessions.lock().unwrap().insert(session_id, handle.clone());

        let events = self.events.clone();
        std::thread::Builder::new()
            .name(format!("pty-reader-{session_id}"))
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = &buf[..n];
                            scrollback.lock().unwrap().push(chunk);
                            last_output_at.store(now(), Ordering::Relaxed);
                            {
                                let mut st = handle.state.lock().unwrap();
                                if *st != SessionState::Working && *st != SessionState::Dead {
                                    *st = SessionState::Working;
                                    let _ = store.set_session_state(
                                        session_id,
                                        SessionState::Working,
                                        None,
                                    );
                                    let _ = events.send(Event::SessionStateChanged {
                                        session_id,
                                        state: SessionState::Working,
                                        detail: None,
                                    });
                                }
                            }
                            if attach_count.load(Ordering::Relaxed) > 0 {
                                let _ = events.send(Event::PtyOutput {
                                    session_id,
                                    bytes: chunk.to_vec(),
                                });
                            }
                        }
                    }
                }
                // PTY EOF: reap the child, mark dead
                let _ = child.wait();
                *handle.state.lock().unwrap() = SessionState::Dead;
                let _ = store.set_session_state(session_id, SessionState::Dead, None);
                let _ = store.clear_dead_tab_slot(session_id);
                let _ = events.send(Event::SessionStateChanged {
                    session_id,
                    state: SessionState::Dead,
                    detail: None,
                });
            })
            .context("spawning pty reader thread")?;

        Ok(pid)
    }

    fn handle(&self, session_id: i64) -> Result<Arc<SessionHandle>> {
        self.sessions
            .lock()
            .unwrap()
            .get(&session_id)
            .cloned()
            .ok_or_else(|| anyhow!("no live session {session_id} in this daemon"))
    }

    pub fn write_stdin(&self, session_id: i64, bytes: &[u8]) -> Result<()> {
        let h = self.handle(session_id)?;
        let mut w = h.writer.lock().unwrap();
        w.write_all(bytes)?;
        w.flush()?;
        Ok(())
    }

    pub fn resize(&self, session_id: i64, cols: u16, rows: u16) -> Result<()> {
        let h = self.handle(session_id)?;
        let res = h
            .master
            .lock()
            .unwrap()
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        res.map_err(|e| anyhow!("resize: {e}"))
    }

    pub fn kill(&self, session_id: i64) -> Result<()> {
        let h = self.handle(session_id)?;
        let res = h.killer.lock().unwrap().kill();
        res.map_err(|e| anyhow!("kill: {e}"))
    }

    pub fn attach(&self, session_id: i64) -> Result<Vec<u8>> {
        let h = self.handle(session_id)?;
        h.attach_count.fetch_add(1, Ordering::Relaxed);
        let snap = h.scrollback.lock().unwrap().snapshot();
        Ok(snap)
    }

    pub fn detach(&self, session_id: i64) {
        if let Ok(h) = self.handle(session_id) {
            let _ = h.attach_count.fetch_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |c| c.checked_sub(1),
            );
        }
    }

    pub fn scrollback(&self, session_id: i64) -> Result<Vec<u8>> {
        let h = self.handle(session_id)?;
        let snap = h.scrollback.lock().unwrap().snapshot();
        Ok(snap)
    }

    pub fn is_live(&self, session_id: i64) -> bool {
        self.sessions.lock().unwrap().contains_key(&session_id)
    }

    /// Heartbeat (plan §4.1): live sessions still marked working whose PTY
    /// has been quiet past the threshold — candidates for classification.
    pub fn quiet_working(&self, idle_threshold_secs: i64) -> Vec<i64> {
        let sessions = self.sessions.lock().unwrap();
        let cutoff = now() - idle_threshold_secs;
        sessions
            .iter()
            .filter(|(_, h)| {
                *h.state.lock().unwrap() == SessionState::Working
                    && h.last_output_at.load(Ordering::Relaxed) < cutoff
            })
            .map(|(&id, _)| id)
            .collect()
    }

    /// Set a session's state (handle + store) and broadcast the change.
    pub fn set_state(
        &self,
        session_id: i64,
        state: SessionState,
        detail: Option<String>,
        store: &Store,
    ) {
        if let Ok(h) = self.handle(session_id) {
            let mut st = h.state.lock().unwrap();
            if *st == SessionState::Dead || *st == state {
                return;
            }
            *st = state;
        }
        let _ = store.set_session_state(session_id, state, detail.as_deref());
        let _ = self.events.send(Event::SessionStateChanged { session_id, state, detail });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bus() -> broadcast::Sender<Event> {
        broadcast::channel(1024).0
    }

    #[test]
    fn scrollback_ring_caps() {
        let mut ring = ScrollbackRing::new(100);
        for _ in 0..50 {
            ring.push(b"0123456789");
        }
        assert!(ring.snapshot().len() <= 100);
        assert!(ring.snapshot().ends_with(b"0123456789"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_echo_collect_output_then_dead() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let t = store.insert_template("t", "/tmp", None, None, None).unwrap();
        let ws = store
            .insert_workspace(t.id, "/tmp", ats_core::state::WorkspaceStatus::Ready)
            .unwrap();
        let sid = store.insert_session(ws, None, None, None).unwrap();

        let tx = bus();
        let mut rx = tx.subscribe();
        let mgr = SessionManager::new(tx, 1000);
        mgr.spawn(sid, "sh -c 'echo hello-ats; sleep 0.1'", "/tmp", store.clone())
            .unwrap();
        // attach so output is broadcast
        mgr.attach(sid).unwrap();

        let mut got_output = false;
        let mut got_dead = false;
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
        while !(got_output && got_dead) {
            let ev = tokio::time::timeout_at(deadline, rx.recv())
                .await
                .expect("timed out waiting for events")
                .expect("event bus closed");
            match ev {
                Event::PtyOutput { session_id, bytes } if session_id == sid => {
                    if String::from_utf8_lossy(&bytes).contains("hello-ats") {
                        got_output = true;
                    }
                }
                Event::SessionStateChanged { session_id, state, .. }
                    if session_id == sid && state == SessionState::Dead =>
                {
                    got_dead = true;
                }
                _ => {}
            }
        }
        // scrollback retained the output too
        let sb = mgr.scrollback(sid).unwrap();
        assert!(String::from_utf8_lossy(&sb).contains("hello-ats"));
        // store reflects dead state
        assert_eq!(
            store.get_session(sid).unwrap().unwrap().state,
            SessionState::Dead
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stdin_reaches_process() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let t = store.insert_template("t", "/tmp", None, None, None).unwrap();
        let ws = store
            .insert_workspace(t.id, "/tmp", ats_core::state::WorkspaceStatus::Ready)
            .unwrap();
        let sid = store.insert_session(ws, None, None, None).unwrap();

        let tx = bus();
        let mut rx = tx.subscribe();
        let mgr = SessionManager::new(tx, 1000);
        mgr.spawn(sid, "cat", "/tmp", store.clone()).unwrap();
        mgr.attach(sid).unwrap();
        mgr.write_stdin(sid, b"ping-pong\n").unwrap();

        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
        let mut echoed = false;
        while !echoed {
            let ev = tokio::time::timeout_at(deadline, rx.recv())
                .await
                .expect("timed out")
                .expect("bus closed");
            if let Event::PtyOutput { session_id, bytes } = ev {
                if session_id == sid && String::from_utf8_lossy(&bytes).contains("ping-pong") {
                    echoed = true;
                }
            }
        }
        mgr.kill(sid).unwrap();
    }
}
