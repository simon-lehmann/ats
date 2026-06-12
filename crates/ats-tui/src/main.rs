//! ats-tui: Ratatui client. Left rail + two tab groups of five sessions.
//! Detaching (Alt+x) never kills agents — the daemon owns them.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use ats_core::client::Client;
use ats_core::config::Config;
use ats_core::rpc::{Event, Request, Response};
use crossterm::event::{Event as CtEvent, EventStream, KeyEventKind};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

mod app;
mod input;
mod ui;

use app::App;

fn load_config() -> Config {
    for path in [
        std::path::PathBuf::from("ats.toml"),
        ats_core::data_dir().join("ats.toml"),
    ] {
        if let Ok(raw) = std::fs::read_to_string(&path) {
            if let Ok(cfg) = Config::from_toml(&raw) {
                return cfg;
            }
        }
    }
    Config::default()
}

/// Connect to the daemon, starting one if none is running.
async fn connect_or_start(socket: &str) -> Result<Arc<Client>> {
    if let Ok(c) = Client::connect(socket).await {
        return Ok(c);
    }
    let exe = std::env::current_exe()?;
    let daemon = exe.with_file_name(if cfg!(windows) { "ats-daemon.exe" } else { "ats-daemon" });
    let log = ats_core::data_dir().join("daemon.log");
    std::fs::create_dir_all(ats_core::data_dir())?;
    let logfile = std::fs::File::create(&log)?;
    std::process::Command::new(&daemon)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(logfile.try_clone()?))
        .stderr(std::process::Stdio::from(logfile))
        .spawn()
        .with_context(|| format!("starting {}", daemon.display()))?;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(c) = Client::connect(socket).await {
            return Ok(c);
        }
    }
    anyhow::bail!("daemon did not come up on {socket} (log: {})", log.display())
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = load_config();
    let socket = config
        .daemon
        .socket_path
        .clone()
        .unwrap_or_else(ats_core::default_socket_path);
    let client = connect_or_start(&socket).await?;

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, client, &config).await;
    ratatui::restore();
    result
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    client: Arc<Client>,
    config: &Config,
) -> Result<()> {
    let mut app = App::new(client.clone(), config.ui.group_a_slots, config.ui.group_b_slots);
    app.template_colors = config.ui.template_colors.clone();
    // --group a|b: single-group client for a second monitor/window
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--group") {
        match args.get(i + 1).map(String::as_str) {
            Some("a") => {
                app.solo = true;
                app.focus = app::Focus::GroupA;
            }
            Some("b") => {
                app.solo = true;
                app.focus = app::Focus::GroupB;
            }
            other => anyhow::bail!("--group expects 'a' or 'b', got {other:?}"),
        }
    }
    let mut async_rx = app.async_rx.take().expect("fresh App has the receiver");
    app.refresh().await?;

    // cold start: nothing registered and no orchestrator yet → open the
    // orchestrator with an onboarding kickoff so first-run setup happens by
    // conversation instead of `ats register`. (Skip on a second-monitor client.)
    if !app.solo && app.templates.is_empty() && !app.sessions.iter().any(|s| s.is_orchestrator) {
        let kickoff = "No repos are registered with ATS yet. Greet the developer in one \
            line, ask which local git repo they want you to manage, then register it as a \
            template and spawn a session in it."
            .to_string();
        if let Ok(Response::Session { .. }) =
            client.request(Request::EnsureOrchestrator { kickoff: Some(kickoff) }).await
        {
            app.refresh().await?;
            app.modal = app::Modal::Orchestrator;
            app.status_line = "orchestrator ready — type to it; it'll set up your first repo".into();
        }
    }

    let mut events = client.subscribe_events();
    let mut term_events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(2000));
    let rail_width = config.ui.rail_width;

    loop {
        // draw, remembering pane sizes for attach/resize bookkeeping
        let mut areas = None;
        terminal.draw(|frame| {
            areas = Some(ui::draw(frame, &app, rail_width));
        })?;
        if let Some(areas) = areas {
            let pane_a = (areas.a_inner.width, areas.a_inner.height);
            let pane_b = (areas.b_inner.width, areas.b_inner.height);
            // attach the orchestrator session at the overlay's size while it's open
            let orch = match (areas.orch_inner, app.orchestrator_session_id()) {
                (Some(r), Some(id)) => Some((id, (r.width, r.height))),
                _ => None,
            };
            if let Err(e) = app.sync_attachments(pane_a, pane_b, orch).await {
                app.status_line = format!("attach: {e:#}");
            }
        }
        if app.should_quit {
            return Ok(());
        }

        tokio::select! {
            ev = term_events.next() => {
                match ev {
                    Some(Ok(CtEvent::Key(key))) if key.kind != KeyEventKind::Release => {
                        input::handle_key(&mut app, key).await?;
                    }
                    Some(Ok(CtEvent::Resize(_, _))) => { /* redraw on next loop */ }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(()),
                }
            }
            ev = events.recv() => {
                match ev {
                    Ok(Event::PtyOutput { session_id, bytes }) => {
                        app.feed_output(session_id, &bytes);
                        // drain any burst of output before redrawing
                        while let Ok(Event::PtyOutput { session_id, bytes }) = events.try_recv() {
                            app.feed_output(session_id, &bytes);
                        }
                    }
                    Ok(Event::SessionStateChanged { session_id, state, detail }) => {
                        app.set_session_state(session_id, state, detail);
                        if state == ats_core::state::SessionState::Dead {
                            let _ = app.refresh().await;
                        }
                    }
                    Ok(Event::WorkspaceStatusChanged { .. }) => {
                        let _ = app.refresh().await;
                    }
                    Ok(Event::DigestReady { session_id, summary }) => {
                        app.status_line = format!("digest [{session_id}]: {summary}");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        anyhow::bail!("daemon connection lost");
                    }
                }
            }
            msg = async_rx.recv() => {
                if let Some(msg) = msg {
                    app.apply_async(msg);
                }
            }
            _ = tick.tick() => {
                let _ = app.refresh().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    // Layout snapshot (plan §6): rail sections and both tab groups render.
    #[tokio::test]
    async fn layout_renders_rail_and_groups() {
        // a client that never connects isn't needed for pure rendering;
        // build App with a dummy client via an in-process socket-less path is
        // overkill — render with a default App built around an unconnected
        // client is impossible, so render the UI parts directly instead.
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        // App::new needs a Client; spin a daemonless fake via a socketpair is
        // heavy. Instead, validate via the daemon e2e test for behavior and
        // here only check ui::draw with a stub App is structurally sound.
        // Connect to a real micro-daemon over a temp socket:
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("tui-test.sock").to_string_lossy().into_owned();
        let mut config = ats_core::config::Config::default();
        config.daemon.workspaces_root = tmp.path().join("ws").to_string_lossy().into_owned();
        let store = std::sync::Arc::new(ats_daemon::store::Store::open(&tmp.path().join("db")).unwrap());
        let daemon = std::sync::Arc::new(ats_daemon::server::Daemon::new(
            config,
            store,
            tmp.path().join("data"),
        ));
        let handle = tokio::spawn({
            let daemon = daemon.clone();
            let socket = socket.clone();
            async move { ats_daemon::server::serve(daemon, &socket).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let client = Client::connect(&socket).await.unwrap();
        let mut app = App::new(client, 5, 5);
        app.refresh().await.unwrap();

        terminal
            .draw(|frame| {
                ui::draw(frame, &app, 28);
            })
            .unwrap();

        let text = format!("{:?}", terminal.backend().buffer());
        assert!(text.contains("WORKSPACES"));
        assert!(text.contains("REVIEW QUEUE"));
        assert!(text.contains("empty slot"));
        handle.abort();
    }
}
