//! End-to-end: real daemon on a real socket, driven through the shared
//! client, running a fake agent (plan §6). Covers the full spawn flow:
//! register template → spawn workspace+session → attach → stream output →
//! observe heartbeat/death — and that the daemon outlives its clients.

use std::sync::Arc;
use std::time::Duration;

use ats_core::client::Client;
use ats_core::config::Config;
use ats_core::rpc::{Event, Request, Response};
use ats_core::state::SessionState;
use ats_daemon::{server, store::Store};
use tokio::process::Command;

async fn git(dir: &std::path::Path, args: &[&str]) {
    let st = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .await
        .unwrap();
    assert!(st.success(), "git {args:?} failed");
}

async fn make_template(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-b", "main"]).await;
    git(dir, &["config", "user.email", "t@t"]).await;
    git(dir, &["config", "user.name", "t"]).await;
    std::fs::write(dir.join("README.md"), "# demo\n").unwrap();
    git(dir, &["add", "-A"]).await;
    git(dir, &["commit", "-m", "init"]).await;
}

#[cfg_attr(windows, ignore = "Unix fake agent + filesystem socket; Windows port tracked")]
#[tokio::test(flavor = "multi_thread")]
async fn full_spawn_flow_over_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("ats-test.sock");
    let socket_str = socket.to_string_lossy().into_owned();

    let template_dir = tmp.path().join("template");
    make_template(&template_dir).await;

    let mut config = Config::default();
    config.daemon.workspaces_root =
        tmp.path().join("workspaces").to_string_lossy().into_owned();
    // fake agent: prints, waits, prints, exits
    config.daemon.session_cmd =
        "sh -c 'echo agent-booted; sleep 0.3; echo agent-finished'".into();
    config.daemon.idle_threshold_secs = 1;

    let store = Arc::new(Store::open(&tmp.path().join("ats.db")).unwrap());
    let daemon = Arc::new(server::Daemon::new(config, store, tmp.path().join("data")));
    let server_handle = tokio::spawn({
        let daemon = daemon.clone();
        let socket_str = socket_str.clone();
        async move { server::serve(daemon, &socket_str).await }
    });
    // wait for the socket to exist
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let client = Client::connect(&socket_str).await.unwrap();

    // register template
    let resp = client
        .request(Request::RegisterTemplate {
            name: "demo".into(),
            path: template_dir.to_string_lossy().into_owned(),
            setup_cmd: None,
            kickoff_prompt: None,
        })
        .await
        .unwrap();
    let template_id = match resp {
        Response::Template { template } => template.id,
        other => panic!("unexpected: {other:?}"),
    };

    // spawn session (workspace + PTY)
    let mut events = client.subscribe_events();
    let resp = client
        .request(Request::SpawnSession { template_id, tab_slot: None, kickoff_note_id: None })
        .await
        .unwrap();
    let session = match resp {
        Response::Session { session } => session,
        other => panic!("unexpected: {other:?}"),
    };
    assert_eq!(session.tab_slot, Some(1));
    assert_eq!(session.state, SessionState::Working);

    // attach and stream
    let resp = client
        .request(Request::AttachSession { session_id: session.id })
        .await
        .unwrap();
    assert!(matches!(resp, Response::Scrollback { .. }));

    let mut saw_output = false;
    let mut saw_dead = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !(saw_output && saw_dead) {
        let ev = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("timed out waiting for events")
            .expect("event stream closed");
        match ev {
            Event::PtyOutput { session_id, bytes } if session_id == session.id => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                if text.contains("agent-finished") {
                    saw_output = true;
                }
            }
            Event::SessionStateChanged { session_id, state, .. }
                if session_id == session.id && state == SessionState::Dead =>
            {
                saw_dead = true;
            }
            _ => {}
        }
    }

    // a second client sees the same state (daemon survives client churn)
    drop(client);
    let client2 = Client::connect(&socket_str).await.unwrap();
    let resp = client2.request(Request::ListSessions).await.unwrap();
    match resp {
        Response::Sessions { sessions } => {
            // scrollback for a dead session is still served
            let resp = client2
                .request(Request::GetScrollback { session_id: sessions[0].id })
                .await
                .unwrap();
            match resp {
                Response::Scrollback { data, .. } => {
                    let text = String::from_utf8_lossy(&data).into_owned();
                    assert!(text.contains("agent-booted"), "scrollback: {text}");
                    assert!(text.contains("agent-finished"), "scrollback: {text}");
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
        other => panic!("unexpected: {other:?}"),
    }

    server_handle.abort();
}

#[cfg_attr(windows, ignore = "Unix fake agent + filesystem socket; Windows port tracked")]
#[tokio::test(flavor = "multi_thread")]
async fn quiet_session_with_question_transcript_becomes_needs_input() {
    let tmp = tempfile::tempdir().unwrap();
    // fake Claude home so transcript discovery looks where we control
    std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path().join("claude-home"));
    let socket_str = tmp.path().join("ats-q.sock").to_string_lossy().into_owned();
    let template_dir = tmp.path().join("template");
    make_template(&template_dir).await;

    let mut config = Config::default();
    config.daemon.workspaces_root =
        tmp.path().join("workspaces").to_string_lossy().into_owned();
    config.daemon.session_cmd = "sh -c 'echo up; sleep 30'".into();
    config.daemon.idle_threshold_secs = 1;

    let store = Arc::new(Store::open(&tmp.path().join("ats.db")).unwrap());
    let daemon = Arc::new(server::Daemon::new(config, store, tmp.path().join("data")));
    let server_handle = tokio::spawn({
        let daemon = daemon.clone();
        let s = socket_str.clone();
        async move { server::serve(daemon, &s).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = Client::connect(&socket_str).await.unwrap();
    let resp = client
        .request(Request::RegisterTemplate {
            name: "demo".into(),
            path: template_dir.to_string_lossy().into_owned(),
            setup_cmd: None,
            kickoff_prompt: None,
        })
        .await
        .unwrap();
    let Response::Template { template } = resp else { panic!() };

    // fabricate the Claude Code transcript BEFORE spawning (the first
    // workspace path is deterministic): the agent asked a question and
    // went quiet. Writing it first avoids racing the idle sweep.
    let ws_path = tmp.path().join("workspaces").join("demo-1");
    let proj = ats_daemon::transcript::project_dir_for_cwd(
        &tmp.path().join("claude-home"),
        &ws_path.to_string_lossy(),
    );
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join("abc.jsonl"),
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Should I delete the legacy module?"}]}}
"#,
    )
    .unwrap();

    let mut events = client.subscribe_events();
    let resp = client
        .request(Request::SpawnSession {
            template_id: template.id,
            tab_slot: None,
            kickoff_note_id: None,
        })
        .await
        .unwrap();
    let Response::Session { session } = resp else { panic!() };

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let ev = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("timed out waiting for needs_input")
            .expect("event stream closed");
        if let Event::SessionStateChanged { session_id, state, detail } = ev {
            if session_id == session.id && state == SessionState::NeedsInput {
                assert_eq!(detail.as_deref(), Some("Should I delete the legacy module?"));
                break;
            }
        }
    }

    let _ = client.request(Request::KillSession { session_id: session.id }).await;
    server_handle.abort();
}

/// Minimal Anthropic-shaped HTTP server: always answers with `reply`.
async fn fake_anthropic(listener: tokio::net::TcpListener, reply: &'static str) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let Ok((mut sock, _)) = listener.accept().await else { return };
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            // read headers + declared body length
            let (mut header_end, mut content_len) = (None, 0usize);
            loop {
                let Ok(n) = sock.read(&mut tmp).await else { return };
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if header_end.is_none() {
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        header_end = Some(pos + 4);
                        let headers = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                        content_len = headers
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                    }
                }
                if let Some(he) = header_end {
                    if buf.len() >= he + content_len {
                        break;
                    }
                }
            }
            let body = format!(r#"{{"content":[{{"type":"text","text":"{reply}"}}]}}"#);
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
    }
}

#[cfg_attr(windows, ignore = "Unix fake agent + filesystem socket; Windows port tracked")]
#[tokio::test(flavor = "multi_thread")]
async fn orchestrator_digest_ask_and_reentry() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("ANTHROPIC_API_KEY", "test-key");
    let api = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api.local_addr().unwrap();
    tokio::spawn(fake_anthropic(api, "CANNED ANSWER"));

    let socket_str = tmp.path().join("ats-orch.sock").to_string_lossy().into_owned();
    let template_dir = tmp.path().join("template");
    make_template(&template_dir).await;

    let mut config = Config::default();
    config.daemon.workspaces_root =
        tmp.path().join("workspaces").to_string_lossy().into_owned();
    config.daemon.session_cmd = "sh -c 'echo up; sleep 30'".into();
    config.daemon.idle_threshold_secs = 600; // keep the sweep out of the way
    config.orchestrator.base_url = Some(format!("http://{api_addr}"));

    let store = Arc::new(Store::open(&tmp.path().join("ats.db")).unwrap());
    let daemon = Arc::new(server::Daemon::new(config, store.clone(), tmp.path().join("data")));
    let server_handle = tokio::spawn({
        let daemon = daemon.clone();
        let s = socket_str.clone();
        async move { server::serve(daemon, &s).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = Client::connect(&socket_str).await.unwrap();
    let resp = client
        .request(Request::RegisterTemplate {
            name: "demo".into(),
            path: template_dir.to_string_lossy().into_owned(),
            setup_cmd: None,
            kickoff_prompt: None,
        })
        .await
        .unwrap();
    let Response::Template { template } = resp else { panic!() };
    let resp = client
        .request(Request::SpawnSession {
            template_id: template.id,
            tab_slot: None,
            kickoff_note_id: None,
        })
        .await
        .unwrap();
    let Response::Session { session } = resp else { panic!() };

    // hand-register a transcript: short report (heuristic path) — the
    // store is shared with the daemon, so set it directly
    let tpath = tmp.path().join("t.jsonl");
    std::fs::write(
        &tpath,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Refactor done.\nAll 12 tests pass."}]}}
"#,
    )
    .unwrap();
    store
        .set_session_transcript(session.id, &tpath.to_string_lossy())
        .unwrap();

    // heuristic digest: last line, no API call
    let resp = client
        .request(Request::SummarizeSession { session_id: session.id, force_llm: false })
        .await
        .unwrap();
    let Response::Digest { summary, .. } = resp else { panic!("{resp:?}") };
    assert_eq!(summary, "All 12 tests pass.");

    // forced LLM digest: canned reply from the fake server
    let resp = client
        .request(Request::SummarizeSession { session_id: session.id, force_llm: true })
        .await
        .unwrap();
    let Response::Digest { summary, .. } = resp else { panic!("{resp:?}") };
    assert_eq!(summary, "CANNED ANSWER");

    // ask across sessions
    let resp = client
        .request(Request::AskOrchestrator {
            question: "which sessions are blocked?".into(),
            session_ids: vec![session.id],
        })
        .await
        .unwrap();
    let Response::Answer { text } = resp else { panic!("{resp:?}") };
    assert_eq!(text, "CANNED ANSWER");

    // re-entry note drafted and stored
    let resp = client
        .request(Request::DraftReentry { session_id: session.id })
        .await
        .unwrap();
    let Response::Note { note } = resp else { panic!("{resp:?}") };
    assert!(note.title.starts_with("re-entry:"), "{}", note.title);
    assert_eq!(note.body, "CANNED ANSWER");

    let _ = client.request(Request::KillSession { session_id: session.id }).await;
    server_handle.abort();
}

#[cfg_attr(windows, ignore = "Unix fake agent + filesystem socket; Windows port tracked")]
#[tokio::test(flavor = "multi_thread")]
async fn template_kickoff_prompt_reaches_the_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_str = tmp.path().join("ats-kick.sock").to_string_lossy().into_owned();
    let template_dir = tmp.path().join("template");
    make_template(&template_dir).await;

    let mut config = Config::default();
    config.daemon.workspaces_root =
        tmp.path().join("workspaces").to_string_lossy().into_owned();
    // cat echoes the kickoff back into the scrollback
    config.daemon.session_cmd = "cat".into();
    config.daemon.idle_threshold_secs = 600;

    let store = Arc::new(Store::open(&tmp.path().join("ats.db")).unwrap());
    let daemon = Arc::new(server::Daemon::new(config, store, tmp.path().join("data")));
    let server_handle = tokio::spawn({
        let daemon = daemon.clone();
        let s = socket_str.clone();
        async move { server::serve(daemon, &s).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = Client::connect(&socket_str).await.unwrap();
    let resp = client
        .request(Request::RegisterTemplate {
            name: "demo".into(),
            path: template_dir.to_string_lossy().into_owned(),
            setup_cmd: None,
            kickoff_prompt: Some("kickoff: implement the parser".into()),
        })
        .await
        .unwrap();
    let Response::Template { template } = resp else { panic!() };
    assert_eq!(template.kickoff_prompt.as_deref(), Some("kickoff: implement the parser"));

    let resp = client
        .request(Request::SpawnSession {
            template_id: template.id,
            tab_slot: None,
            kickoff_note_id: None,
        })
        .await
        .unwrap();
    let Response::Session { session } = resp else { panic!() };

    // kickoff is sent ~3s after spawn; poll the scrollback
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "kickoff never reached the agent"
        );
        let resp = client
            .request(Request::GetScrollback { session_id: session.id })
            .await
            .unwrap();
        if let Response::Scrollback { data, .. } = resp {
            if String::from_utf8_lossy(&data).contains("kickoff: implement the parser") {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    let _ = client.request(Request::KillSession { session_id: session.id }).await;
    server_handle.abort();
}

#[cfg_attr(windows, ignore = "Unix fake agent + filesystem socket; Windows port tracked")]
#[tokio::test(flavor = "multi_thread")]
async fn scratch_session_runs_without_workspace_clone() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_str = tmp.path().join("ats-scratch.sock").to_string_lossy().into_owned();

    let mut config = Config::default();
    config.daemon.workspaces_root =
        tmp.path().join("workspaces").to_string_lossy().into_owned();
    config.daemon.session_cmd = "cat".into();
    config.daemon.idle_threshold_secs = 600;

    let store = Arc::new(Store::open(&tmp.path().join("ats.db")).unwrap());
    let daemon = Arc::new(server::Daemon::new(config, store, tmp.path().join("data")));
    let server_handle = tokio::spawn({
        let daemon = daemon.clone();
        let s = socket_str.clone();
        async move { server::serve(daemon, &s).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = Client::connect(&socket_str).await.unwrap();
    // no templates registered at all — scratch sessions must still work
    let resp = client
        .request(Request::SpawnScratchSession {
            cwd: None,
            tab_slot: None,
            kickoff: Some("draft a plan for the migration".into()),
        })
        .await
        .unwrap();
    let Response::Session { session } = resp else { panic!("{resp:?}") };
    assert_eq!(session.tab_slot, Some(1));
    assert!(
        session.workspace_path.contains("scratch"),
        "cwd: {}",
        session.workspace_path
    );
    // nothing was cloned into the workspaces root
    assert!(!tmp.path().join("workspaces").exists());

    // kickoff arrives (cat echoes it)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        assert!(tokio::time::Instant::now() < deadline, "kickoff never arrived");
        let resp = client
            .request(Request::GetScrollback { session_id: session.id })
            .await
            .unwrap();
        if let Response::Scrollback { data, .. } = resp {
            if String::from_utf8_lossy(&data).contains("draft a plan for the migration") {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // an explicit cwd also works (e.g. inspecting an existing directory)
    let other = tmp.path().join("elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    let resp = client
        .request(Request::SpawnScratchSession {
            cwd: Some(other.to_string_lossy().into_owned()),
            tab_slot: None,
            kickoff: None,
        })
        .await
        .unwrap();
    let Response::Session { session: s2 } = resp else { panic!() };
    assert_eq!(s2.workspace_path, other.to_string_lossy());
    assert_eq!(s2.tab_slot, Some(2));

    let _ = client.request(Request::KillSession { session_id: session.id }).await;
    let _ = client.request(Request::KillSession { session_id: s2.id }).await;
    server_handle.abort();
}

#[cfg_attr(windows, ignore = "Unix fake agent + filesystem socket; Windows port tracked")]
#[tokio::test(flavor = "multi_thread")]
async fn idle_heartbeat_fires() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_str = tmp.path().join("ats-idle.sock").to_string_lossy().into_owned();
    let template_dir = tmp.path().join("template");
    make_template(&template_dir).await;

    let mut config = Config::default();
    config.daemon.workspaces_root =
        tmp.path().join("workspaces").to_string_lossy().into_owned();
    config.daemon.session_cmd = "sh -c 'echo up; sleep 30'".into();
    config.daemon.idle_threshold_secs = 1;

    let store = Arc::new(Store::open(&tmp.path().join("ats.db")).unwrap());
    let daemon = Arc::new(server::Daemon::new(config, store, tmp.path().join("data")));
    let server_handle = tokio::spawn({
        let daemon = daemon.clone();
        let s = socket_str.clone();
        async move { server::serve(daemon, &s).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = Client::connect(&socket_str).await.unwrap();
    let resp = client
        .request(Request::RegisterTemplate {
            name: "demo".into(),
            path: template_dir.to_string_lossy().into_owned(),
            setup_cmd: None,
            kickoff_prompt: None,
        })
        .await
        .unwrap();
    let template_id = match resp {
        Response::Template { template } => template.id,
        other => panic!("unexpected: {other:?}"),
    };

    let mut events = client.subscribe_events();
    let resp = client
        .request(Request::SpawnSession { template_id, tab_slot: None, kickoff_note_id: None })
        .await
        .unwrap();
    let session_id = match resp {
        Response::Session { session } => session.id,
        other => panic!("unexpected: {other:?}"),
    };

    // quiet agent → idle within a few sweeps
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let ev = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("timed out waiting for idle")
            .expect("event stream closed");
        if let Event::SessionStateChanged { session_id: sid, state, .. } = ev {
            if sid == session_id && state == SessionState::Idle {
                break;
            }
        }
    }

    let _ = client.request(Request::KillSession { session_id }).await;
    server_handle.abort();
}

// ---- cross-platform helpers (run on Windows too: named pipe + pwsh agent) ----

/// A local-socket name valid on the host: a named pipe on Windows, a temp
/// filesystem path on Unix.
fn test_socket(tag: &str) -> String {
    if cfg!(windows) {
        format!(r"\\.\pipe\ats-test-{tag}-{}", std::process::id())
    } else {
        std::env::temp_dir()
            .join(format!("ats-test-{tag}-{}.sock", std::process::id()))
            .to_string_lossy()
            .into_owned()
    }
}

/// A fake agent that prints `marker` then stays alive, in the syntax of the
/// shell the daemon wraps `session_cmd` in (sh on Unix, pwsh on Windows).
fn stay_alive_agent(marker: &str) -> String {
    if cfg!(windows) {
        format!("Write-Output {marker}; Start-Sleep -Seconds 20")
    } else {
        format!("echo {marker}; sleep 20")
    }
}

/// Connect with retry — a named pipe has no filesystem existence check.
async fn connect_retry(socket: &str) -> Arc<Client> {
    for _ in 0..100 {
        if let Ok(c) = Client::connect(socket).await {
            return c;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("daemon never came up on {socket}");
}

/// The orchestrator is a singleton, takes no tab slot, and its live session is
/// attachable — the exact path that regressed when the overlay attached a dead
/// orchestrator. Cross-platform (named pipe + pwsh/sh fake agent).
#[tokio::test(flavor = "multi_thread")]
async fn orchestrator_is_singleton_no_tab_and_attachable() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_str = test_socket("orch");

    let mut config = Config::default();
    config.daemon.workspaces_root =
        tmp.path().join("workspaces").to_string_lossy().into_owned();
    config.daemon.session_cmd = stay_alive_agent("orch-booted");
    config.orchestrator.mcp_enabled = false; // don't shell out to claude

    let store = Arc::new(Store::open(&tmp.path().join("ats.db")).unwrap());
    let daemon = Arc::new(server::Daemon::new(config, store, tmp.path().join("data")));
    let server_handle = tokio::spawn({
        let daemon = daemon.clone();
        let s = socket_str.clone();
        async move { server::serve(daemon, &s).await }
    });

    let client = connect_retry(&socket_str).await;

    // ensure → a live orchestrator, flagged, with no tab slot
    let Response::Session { session: s1 } = client
        .request(Request::EnsureOrchestrator { kickoff: None })
        .await
        .unwrap()
    else {
        panic!("expected a session")
    };
    assert!(s1.is_orchestrator, "must be flagged as the orchestrator");
    assert_eq!(s1.tab_slot, None, "orchestrator lives in the overlay, not a tab");

    // idempotent: a second ensure returns the same session, not a new one
    let Response::Session { session: s2 } = client
        .request(Request::EnsureOrchestrator { kickoff: None })
        .await
        .unwrap()
    else {
        panic!("expected a session")
    };
    assert_eq!(s2.id, s1.id, "EnsureOrchestrator must be idempotent");
    let Response::Sessions { sessions } =
        client.request(Request::ListSessions).await.unwrap()
    else {
        panic!()
    };
    let live_orch = sessions
        .iter()
        .filter(|s| s.is_orchestrator && s.state != SessionState::Dead)
        .count();
    assert_eq!(live_orch, 1, "exactly one live orchestrator");

    // attach the live orchestrator → scrollback eventually shows the agent's
    // output (proves it's attachable, the overlay's contract)
    let mut attached = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline {
        let resp = client.request(Request::GetScrollback { session_id: s1.id }).await.unwrap();
        if let Response::Scrollback { data, .. } = resp {
            if String::from_utf8_lossy(&data).contains("orch-booted") {
                attached = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(attached, "live orchestrator scrollback never showed agent output");

    // the /setup-repo slash command was written into the orchestrator's cwd
    let setup_cmd = tmp
        .path()
        .join("data")
        .join("orchestrator")
        .join(".claude")
        .join("commands")
        .join("setup-repo.md");
    assert!(setup_cmd.exists(), "/setup-repo command was not written");

    let _ = client.request(Request::KillSession { session_id: s1.id }).await;
    server_handle.abort();
}
