//! A minimal Chrome DevTools Protocol client over a single WebSocket.
//!
//! One browser-level connection drives every page through "flattened" sessions:
//! commands carry an optional `sessionId`, responses are matched by `id`, and
//! events (no `id`) are forwarded to a channel for the controller to react to.

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// How long to wait for any CDP reply.
const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

pub struct Cdp {
    next_id: AtomicU64,
    pending: Pending,
    out: mpsc::UnboundedSender<Message>,
}

impl Cdp {
    /// Connect to a DevTools WebSocket URL. Returns the client plus a receiver
    /// of protocol events. When the socket closes, the event channel ends.
    pub async fn connect(url: &str) -> Result<(Arc<Cdp>, mpsc::UnboundedReceiver<Value>)> {
        let (ws, _) = connect_async(url).await?;
        let (mut write, mut read) = ws.split();

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<Value>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        // Writer task: drain outgoing messages to the socket.
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if write.send(msg).await.is_err() {
                    break;
                }
            }
        });

        // Reader task: route responses to waiters, events to the channel.
        let pending_reader = pending.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = read.next().await {
                if let Message::Text(text) = msg {
                    let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                        continue;
                    };
                    if let Some(id) = value.get("id").and_then(Value::as_u64) {
                        if let Some(tx) = pending_reader.lock().await.remove(&id) {
                            let _ = tx.send(value);
                        }
                    } else {
                        let _ = ev_tx.send(value);
                    }
                }
            }
        });

        let cdp = Arc::new(Cdp {
            next_id: AtomicU64::new(1),
            pending,
            out: out_tx,
        });
        Ok((cdp, ev_rx))
    }

    /// Send a command and await its result. `session` is `None` for browser-level
    /// commands, or a flattened session id for a page.
    pub async fn send(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.out
            .send(Message::Text(msg.to_string().into()))
            .map_err(|_| anyhow!("CDP connection closed"))?;

        // Never wait forever: a page caught mid-swap during Teams' startup may never answer,
        // which would stall whatever is waiting on it (e.g. an attach that should retry).
        let resp = match tokio::time::timeout(SEND_TIMEOUT, rx).await {
            Ok(r) => r.map_err(|_| anyhow!("CDP connection closed"))?,
            Err(_) => {
                self.pending.lock().await.remove(&id);
                return Err(anyhow!("CDP {method} timed out after {}s", SEND_TIMEOUT.as_secs()));
            }
        };
        if let Some(err) = resp.get("error") {
            return Err(anyhow!("CDP error for {method}: {err}"));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }
}
