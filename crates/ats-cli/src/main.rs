//! ats: headless RPC client (plan §4.6). Scriptable, and usable by agents
//! themselves to talk to the daemon later.

use anyhow::{anyhow, bail, Result};
use ats_core::client::Client;
use ats_core::rpc::{Request, Response};
use ats_core::state::SessionState;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ats", about = "Agent Terminal Suite — headless client")]
struct Cli {
    /// Socket path (default: $ATS_SOCKET or the platform runtime dir)
    #[arg(long, global = true)]
    socket: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Sessions and workspaces overview
    Status,
    /// List registered templates
    Templates,
    /// Register a template repo
    Register {
        name: String,
        path: String,
        #[arg(long)]
        setup_cmd: Option<String>,
    },
    /// Spawn workspace + session from a template (name or id)
    Spawn {
        template: String,
        #[arg(long)]
        slot: Option<u8>,
    },
    /// Send text to a session's stdin (a trailing Enter is added)
    Send { session_id: i64, text: String },
    /// Print a session's scrollback to stdout
    Scrollback { session_id: i64 },
    /// Kill a session's process
    Kill { session_id: i64 },
    /// Diff a workspace against its base; writes a patch file
    Harvest { workspace_id: i64 },
    /// Reset a workspace (git reset --hard + clean)
    Reset { workspace_id: i64 },
    /// Kill sessions and delete the workspace directory
    Destroy { workspace_id: i64 },
    /// Sessions waiting on the developer (finished / needs input / error)
    Queue,
    /// One-line digest of a session's final report
    Digest {
        session_id: i64,
        /// force the LLM even for short reports
        #[arg(long)]
        llm: bool,
    },
    /// Ask the orchestrator a question across sessions (all by default)
    Ask {
        question: String,
        session_ids: Vec<i64>,
    },
    /// Draft a re-entry briefing note for a session
    Reentry { session_id: i64 },
    /// Notes / plans
    #[command(subcommand)]
    Note(NoteCmd),
    /// Prompt clipboard
    #[command(subcommand)]
    Prompt(PromptCmd),
}

#[derive(Subcommand)]
enum NoteCmd {
    /// Add a note (body from arg or stdin with '-')
    Add { title: String, body: String },
    List,
    Finalize { id: i64 },
    /// Send a note's body to a session's stdin
    Send { id: i64, session_id: i64 },
}

#[derive(Subcommand)]
enum PromptCmd {
    /// Add a prompt to the clipboard
    Add {
        label: String,
        body: String,
        #[arg(long, default_value = "clipboard")]
        kind: String,
    },
    List,
    /// Paste a prompt into a session
    Use { id: i64, session_id: i64 },
}

fn glyph(state: SessionState) -> &'static str {
    match state {
        SessionState::Working => "·",
        SessionState::Idle => "○",
        SessionState::Finished => "●",
        SessionState::NeedsInput | SessionState::Error => "!",
        SessionState::Dead => "✕",
    }
}

fn print_sessions(sessions: &[ats_core::rpc::SessionInfo]) {
    if sessions.is_empty() {
        println!("no sessions");
        return;
    }
    for s in sessions {
        let slot = s.tab_slot.map(|n| n.to_string()).unwrap_or_else(|| "-".into());
        let detail = s.state_detail.as_deref().unwrap_or("");
        println!(
            "{:>3}  [{slot}] {} {:<24} {:?}  {detail}",
            s.id,
            glyph(s.state),
            s.title,
            s.state
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // die quietly when piped into head & co. instead of panicking on EPIPE
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    let socket = cli.socket.unwrap_or_else(ats_core::default_socket_path);
    let client = Client::connect(&socket).await.map_err(|e| {
        anyhow!("{e:#}\nIs ats-daemon running? Start it with: ats-daemon")
    })?;

    match cli.cmd {
        Cmd::Status => {
            let resp = client.request(Request::ListSessions).await?;
            if let Response::Sessions { sessions } = resp {
                println!("SESSIONS");
                print_sessions(&sessions);
            }
            let resp = client.request(Request::ListWorkspaces).await?;
            if let Response::Workspaces { workspaces } = resp {
                println!("\nWORKSPACES");
                if workspaces.is_empty() {
                    println!("no workspaces");
                }
                for w in workspaces {
                    println!(
                        "{:>3}  {:<24} {:<12} {:?}",
                        w.id,
                        w.path,
                        w.branch.as_deref().unwrap_or("-"),
                        w.status
                    );
                }
            }
        }
        Cmd::Templates => {
            let resp = client.request(Request::ListTemplates).await?;
            if let Response::Templates { templates } = resp {
                if templates.is_empty() {
                    println!("no templates — register one: ats register <name> <path>");
                }
                for t in templates {
                    println!("{:>3}  {:<16} {}", t.id, t.name, t.path);
                }
            }
        }
        Cmd::Register { name, path, setup_cmd } => {
            let abs = std::fs::canonicalize(&path)
                .map_err(|e| anyhow!("resolving {path}: {e}"))?;
            let resp = client
                .request(Request::RegisterTemplate {
                    name,
                    path: abs.to_string_lossy().into_owned(),
                    setup_cmd,
                })
                .await?;
            if let Response::Template { template } = resp {
                println!("registered template {} (id {})", template.name, template.id);
            }
        }
        Cmd::Spawn { template, slot } => {
            let template_id = match template.parse::<i64>() {
                Ok(id) => id,
                Err(_) => {
                    let resp = client.request(Request::ListTemplates).await?;
                    let Response::Templates { templates } = resp else { bail!("bad response") };
                    templates
                        .iter()
                        .find(|t| t.name == template)
                        .map(|t| t.id)
                        .ok_or_else(|| anyhow!("no template named '{template}'"))?
                }
            };
            let resp = client
                .request(Request::SpawnSession {
                    template_id,
                    tab_slot: slot,
                    kickoff_note_id: None,
                })
                .await?;
            if let Response::Session { session } = resp {
                println!(
                    "session {} in {} (tab {})",
                    session.id,
                    session.workspace_path,
                    session.tab_slot.map(|n| n.to_string()).unwrap_or_else(|| "-".into())
                );
            }
        }
        Cmd::Send { session_id, text } => {
            let mut bytes = text.into_bytes();
            bytes.push(b'\r');
            client.request(Request::WriteStdin { session_id, bytes }).await?;
        }
        Cmd::Scrollback { session_id } => {
            let resp = client.request(Request::GetScrollback { session_id }).await?;
            if let Response::Scrollback { data, .. } = resp {
                use std::io::Write;
                std::io::stdout().write_all(&data)?;
            }
        }
        Cmd::Kill { session_id } => {
            client.request(Request::KillSession { session_id }).await?;
            println!("killed session {session_id}");
        }
        Cmd::Harvest { workspace_id } => {
            let resp = client.request(Request::HarvestWorkspace { id: workspace_id }).await?;
            if let Response::Harvest { diff_stat, patch_path, .. } = resp {
                println!("{diff_stat}\npatch: {patch_path}");
            }
        }
        Cmd::Reset { workspace_id } => {
            client.request(Request::ResetWorkspace { id: workspace_id }).await?;
            println!("workspace {workspace_id} reset");
        }
        Cmd::Destroy { workspace_id } => {
            client.request(Request::DestroyWorkspace { id: workspace_id }).await?;
            println!("workspace {workspace_id} destroyed");
        }
        Cmd::Queue => {
            let resp = client.request(Request::ListReviewQueue).await?;
            if let Response::Sessions { sessions } = resp {
                print_sessions(&sessions);
            }
        }
        Cmd::Digest { session_id, llm } => {
            let resp = client
                .request(Request::SummarizeSession { session_id, force_llm: llm })
                .await?;
            if let Response::Digest { summary, .. } = resp {
                println!("{summary}");
            }
        }
        Cmd::Ask { question, session_ids } => {
            let resp = client
                .request(Request::AskOrchestrator { question, session_ids })
                .await?;
            if let Response::Answer { text } = resp {
                println!("{text}");
            }
        }
        Cmd::Reentry { session_id } => {
            let resp = client.request(Request::DraftReentry { session_id }).await?;
            if let Response::Note { note } = resp {
                println!("note {} — {}\n\n{}", note.id, note.title, note.body);
            }
        }
        Cmd::Note(cmd) => match cmd {
            NoteCmd::Add { title, body } => {
                let body = if body == "-" {
                    use std::io::Read;
                    let mut s = String::new();
                    std::io::stdin().read_to_string(&mut s)?;
                    s
                } else {
                    body
                };
                let resp = client.request(Request::UpsertNote { id: None, title, body }).await?;
                if let Response::Note { note } = resp {
                    println!("note {} ({})", note.id, note.state);
                }
            }
            NoteCmd::List => {
                let resp = client.request(Request::ListNotes).await?;
                if let Response::Notes { notes } = resp {
                    if notes.is_empty() {
                        println!("no notes");
                    }
                    for n in notes {
                        let pin = if n.pinned { "*" } else { " " };
                        println!("{:>3} {pin} [{:<9}] {}", n.id, n.state, n.title);
                    }
                }
            }
            NoteCmd::Finalize { id } => {
                client.request(Request::FinalizeNote { id }).await?;
                println!("note {id} finalized");
            }
            NoteCmd::Send { id, session_id } => {
                client
                    .request(Request::SendNoteToSession { note_id: id, session_id })
                    .await?;
                println!("note {id} sent to session {session_id}");
            }
        },
        Cmd::Prompt(cmd) => match cmd {
            PromptCmd::Add { label, body, kind } => {
                client
                    .request(Request::UpsertPrompt { id: None, label, body, kind })
                    .await?;
                println!("prompt added");
            }
            PromptCmd::List => {
                let resp = client.request(Request::ListPrompts).await?;
                if let Response::Prompts { prompts } = resp {
                    if prompts.is_empty() {
                        println!("no prompts");
                    }
                    for p in prompts {
                        println!("{:>3}  {:<20} ({}x) {}", p.id, p.label, p.use_count, p.kind);
                    }
                }
            }
            PromptCmd::Use { id, session_id } => {
                client.request(Request::UsePrompt { id, session_id }).await?;
                println!("prompt {id} pasted into session {session_id}");
            }
        },
    }
    Ok(())
}
