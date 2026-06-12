//! Shared RPC client for the TUI and CLI: connect to the daemon's local
//! socket, send requests, await correlated responses, and subscribe to
//! pushed events.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use interprocess::local_socket::{
    tokio::{prelude::*, Stream},
    GenericFilePath,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, oneshot, Mutex};

use crate::rpc::{Event, Request, Response, RpcRequest, ServerMessage};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Response, String>>>>>;

pub struct Client {
    next_id: AtomicU64,
    writer: Mutex<interprocess::local_socket::tokio::SendHalf>,
    pending: Pending,
    events: broadcast::Sender<Event>,
}

impl Client {
    pub async fn connect(socket_path: &str) -> Result<Arc<Self>> {
        let name = socket_path
            .to_fs_name::<GenericFilePath>()
            .context("building socket name")?;
        let conn = Stream::connect(name)
            .await
            .with_context(|| format!("connecting to ats-daemon at {socket_path}"))?;
        let (recv, send) = conn.split();

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (events_tx, _) = broadcast::channel(4096);

        let client = Arc::new(Self {
            next_id: AtomicU64::new(1),
            writer: Mutex::new(send),
            pending: pending.clone(),
            events: events_tx.clone(),
        });

        tokio::spawn(async move {
            let mut lines = BufReader::new(recv).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<ServerMessage>(&line) {
                    Ok(ServerMessage::Response(resp)) => {
                        if let Some(tx) = pending.lock().await.remove(&resp.id) {
                            let payload = match (resp.result, resp.error) {
                                (Some(r), _) => Ok(r),
                                (None, Some(e)) => Err(e),
                                (None, None) => Err("empty response".into()),
                            };
                            let _ = tx.send(payload);
                        }
                    }
                    Ok(ServerMessage::Event(ev)) => {
                        let _ = events_tx.send(ev);
                    }
                    Err(e) => {
                        tracing::warn!("unparseable server message: {e}");
                    }
                }
            }
            // daemon gone: fail all in-flight requests
            pending.lock().await.clear();
        });

        Ok(client)
    }

    pub async fn request(&self, request: Request) -> Result<Response> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let mut line = serde_json::to_vec(&RpcRequest { id, request })?;
        line.push(b'\n');
        {
            let mut w = self.writer.lock().await;
            w.write_all(&line).await.context("daemon connection lost")?;
        }
        rx.await
            .map_err(|_| anyhow!("daemon connection closed mid-request"))?
            .map_err(|e| anyhow!(e))
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }
}
