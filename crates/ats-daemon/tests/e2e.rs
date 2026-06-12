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

/// Anthropic-shaped server that serves scripted response bodies in order
/// (then repeats the last one).
async fn scripted_anthropic(listener: tokio::net::TcpListener, bodies: Vec<String>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let counter = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(bodies);
    loop {
        let Ok((mut sock, _)) = listener.accept().await else { return };
        let counter = counter.clone();
        let bodies = bodies.clone();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
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
            let i = counter.fetch_add(1, Ordering::SeqCst).min(bodies.len() - 1);
            let body = &bodies[i];
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn interactive_orchestrator_spawns_and_instructs_via_tools() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("ANTHROPIC_API_KEY", "test-key");
    let api = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api.local_addr().unwrap();
    // round 1: the model calls spawn_session; round 2: it reports back
    tokio::spawn(scripted_anthropic(
        api,
        vec![
            r#"{"stop_reason":"tool_use","content":[
                {"type":"text","text":"Spawning a session for the parser work."},
                {"type":"tool_use","id":"tu_1","name":"spawn_session",
                 "input":{"template":"demo","instruction":"please build the parser"}}
            ]}"#
                .into(),
            r#"{"stop_reason":"end_turn","content":[
                {"type":"text","text":"Done: session 1 is working on the parser."}
            ]}"#
                .into(),
        ],
    ));

    let socket_str = tmp.path().join("ats-agent.sock").to_string_lossy().into_owned();
    let template_dir = tmp.path().join("template");
    make_template(&template_dir).await;

    let mut config = Config::default();
    config.daemon.workspaces_root =
        tmp.path().join("workspaces").to_string_lossy().into_owned();
    config.daemon.session_cmd = "cat".into(); // echoes the instruction
    config.daemon.idle_threshold_secs = 600;
    config.orchestrator.base_url = Some(format!("http://{api_addr}"));

    let store = Arc::new(Store::open(&tmp.path().join("ats.db")).unwrap());
    let daemon = Arc::new(server::Daemon::new(config, store, tmp.path().join("data")));
    let server_handle = tokio::spawn({
        let daemon = daemon.clone();
        let s = socket_str.clone();
        async move { server::serve(daemon, &s).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = Client::connect(&socket_str).await.unwrap();
    // the orchestrator needs the template to exist (it could also register
    // it itself — covered by the tool unit path)
    let resp = client
        .request(Request::RegisterTemplate {
            name: "demo".into(),
            path: template_dir.to_string_lossy().into_owned(),
            setup_cmd: None,
            kickoff_prompt: None,
        })
        .await
        .unwrap();
    assert!(matches!(resp, Response::Template { .. }));

    let mut events = client.subscribe_events();
    let resp = client
        .request(Request::OrchestratorChat {
            message: "spawn a session from demo and have it build the parser".into(),
        })
        .await
        .unwrap();
    let Response::Answer { text } = resp else { panic!("{resp:?}") };
    assert_eq!(text, "Done: session 1 is working on the parser.");

    // tool-call progress was pushed
    let mut saw_tool_progress = false;
    while let Ok(ev) = events.try_recv() {
        if let Event::OrchestratorProgress { text } = ev {
            if text.contains("spawn_session") {
                saw_tool_progress = true;
            }
        }
    }
    assert!(saw_tool_progress, "expected spawn_session progress event");

    // the tool really ran: one session exists…
    let resp = client.request(Request::ListSessions).await.unwrap();
    let Response::Sessions { sessions } = resp else { panic!() };
    assert_eq!(sessions.len(), 1);
    let sid = sessions[0].id;

    // …and the kickoff instruction reaches its terminal (sent ~3s in)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        assert!(tokio::time::Instant::now() < deadline, "instruction never arrived");
        let resp = client.request(Request::GetScrollback { session_id: sid }).await.unwrap();
        if let Response::Scrollback { data, .. } = resp {
            if String::from_utf8_lossy(&data).contains("please build the parser") {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // reset clears the conversation
    let resp = client.request(Request::OrchestratorReset).await.unwrap();
    assert!(matches!(resp, Response::Ok));

    let _ = client.request(Request::KillSession { session_id: sid }).await;
    server_handle.abort();
}

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
