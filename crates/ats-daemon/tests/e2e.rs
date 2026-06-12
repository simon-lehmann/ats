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
