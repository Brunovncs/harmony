//! The one WebSocket to the server: `hello`, requests that get a reply by `rid`, pushes, a ping
//! every 15 s, a watchdog, and reconnecting with backoff until the server says no for good
//! (close code 4401).

use super::api::Api;
use super::types::{ErrorCode, Snapshot};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, interval, sleep, timeout};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

const BACKOFF_MS: [u64; 6] = [500, 1000, 2000, 5000, 10_000, 15_000];
const PING_EVERY: Duration = Duration::from_secs(15);
const WATCHDOG: Duration = Duration::from_secs(35);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum Event {
    /// `hello-ok`, the first time and after every reconnect. `None` when its body did not
    /// parse: keep what you have.
    Up(Option<Box<Snapshot>>),
    Down,
    /// The token was refused (4401). Nothing more will happen on this connection.
    Rejected,
    /// A push from the server, as sent.
    Push(Value),
}

#[derive(Clone, Debug)]
pub struct RequestError {
    /// The reply's `error` field, or `Timeout` / `Offline`.
    pub code: ErrorCode,
    pub reply: Value,
}

impl RequestError {
    fn offline() -> RequestError {
        RequestError { code: ErrorCode::Offline, reply: Value::Null }
    }
}

impl std::fmt::Display for RequestError {
    /// The code as the server wrote it, even one this client has no name for.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.reply.get("error").and_then(Value::as_str) {
            Some(raw) => f.write_str(raw),
            None => self.code.fmt(f),
        }
    }
}

enum Command {
    Request { kind: String, payload: Value, reply: oneshot::Sender<Result<Value, RequestError>> },
    Stop,
}

#[derive(Clone)]
pub struct Realtime {
    commands: mpsc::UnboundedSender<Command>,
}

/// `http(s)://host:port/anything` → `ws(s)://host:port/ws`.
pub fn ws_url(base: &str) -> Option<String> {
    let mut url = url::Url::parse(base).ok()?;
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme).ok()?;
    url.set_path("/ws");
    url.set_query(None);
    Some(url.to_string())
}

impl Realtime {
    /// The token is read from `api` on every connect, so a new one (a changed password) is used
    /// from the next reconnect on.
    pub fn start(rt: &tokio::runtime::Handle, api: Api, events: async_channel::Sender<Event>) -> Realtime {
        let (tx, rx) = mpsc::unbounded_channel();
        let url = ws_url(&api.base()).unwrap_or_default();
        rt.spawn(run(url, api, rx, events));
        Realtime { commands: tx }
    }

    /// Queues the request now, so requests go out in the order they were made, and returns the
    /// reply to await (on the network runtime).
    pub fn request(&self, kind: &str, payload: Value) -> impl Future<Output = Result<Value, RequestError>> + use<> {
        let (reply, rx) = oneshot::channel();
        let queued = self.commands.send(Command::Request { kind: kind.into(), payload, reply }).is_ok();
        async move {
            if !queued {
                return Err(RequestError::offline());
            }
            match timeout(REQUEST_TIMEOUT, rx).await {
                Ok(Ok(r)) => r,
                Ok(Err(_)) => Err(RequestError::offline()),
                Err(_) => Err(RequestError { code: ErrorCode::Timeout, reply: Value::Null }),
            }
        }
    }

    pub fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }
}

type Pending = HashMap<String, oneshot::Sender<Result<Value, RequestError>>>;

async fn run(url: String, api: Api, mut commands: mpsc::UnboundedReceiver<Command>, events: async_channel::Sender<Event>) {
    let mut attempt = 0usize;
    let mut next_rid = 0u64;
    loop {
        let mut pending: Pending = HashMap::new();
        match timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url.as_str())).await {
            Ok(Ok((ws, _))) => {
                let (mut sink, mut stream) = ws.split();
                next_rid += 1;
                let hello_rid = next_rid.to_string();
                let hello = json!({ "type": "hello", "token": api.token(), "rid": hello_rid }).to_string();
                if sink.send(WsMessage::Text(hello.into())).await.is_err() {
                    continue;
                }
                let mut ping = interval(PING_EVERY);
                ping.tick().await;
                let mut last_in = Instant::now();
                let mut up = false;
                let outcome = loop {
                    tokio::select! {
                        frame = stream.next() => {
                            let Some(Ok(frame)) = frame else { break Outcome::Lost };
                            last_in = Instant::now();
                            match frame {
                                WsMessage::Text(text) => {
                                    let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                                    let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
                                    let rid = v.get("rid").and_then(Value::as_str).map(str::to_string);
                                    if rid.as_deref() == Some(hello_rid.as_str()) {
                                        if kind == "hello-ok" {
                                            up = true;
                                            attempt = 0;
                                            let snap = serde_json::from_value::<Snapshot>(v)
                                                .inspect_err(|e| log::warn!("hello-ok did not parse, keeping the old state: {e}"))
                                                .ok();
                                            let _ = events.send(Event::Up(snap.map(Box::new))).await;
                                        } else {
                                            break Outcome::Rejected;
                                        }
                                        continue;
                                    }
                                    if let Some(tx) = rid.and_then(|r| pending.remove(&r)) {
                                        let failed = matches!(kind, "error" | "voice:error" | "call:error" | "hello-failed");
                                        let _ = tx.send(if failed {
                                            let code = v.get("error").and_then(Value::as_str).map(ErrorCode::parse).unwrap_or(ErrorCode::Unknown);
                                            Err(RequestError { code, reply: v })
                                        } else {
                                            Ok(v)
                                        });
                                    } else {
                                        let _ = events.send(Event::Push(v)).await;
                                    }
                                }
                                WsMessage::Close(frame) => {
                                    let code = frame.map(|f| u16::from(f.code)).unwrap_or(1005);
                                    break if code == 4401 { Outcome::Rejected } else { Outcome::Lost };
                                }
                                _ => {}
                            }
                        }
                        cmd = commands.recv() => {
                            match cmd {
                                Some(Command::Request { kind, payload, reply }) => {
                                    if !up {
                                        let _ = reply.send(Err(RequestError::offline()));
                                        continue;
                                    }
                                    next_rid += 1;
                                    let rid = next_rid.to_string();
                                    let mut body = payload;
                                    if !body.is_object() {
                                        body = json!({});
                                    }
                                    body["type"] = json!(kind);
                                    body["rid"] = json!(rid);
                                    if sink.send(WsMessage::Text(body.to_string().into())).await.is_err() {
                                        let _ = reply.send(Err(RequestError::offline()));
                                        break Outcome::Lost;
                                    }
                                    pending.insert(rid, reply);
                                }
                                Some(Command::Stop) | None => {
                                    let _ = sink.send(WsMessage::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "bye".into() }))).await;
                                    break Outcome::Stopped;
                                }
                            }
                        }
                        _ = ping.tick() => {
                            if last_in.elapsed() > WATCHDOG {
                                break Outcome::Lost;
                            }
                            if up {
                                next_rid += 1;
                                let ping = json!({ "type": "ping", "rid": next_rid.to_string() }).to_string();
                                let _ = sink.send(WsMessage::Text(ping.into())).await;
                            }
                        }
                    }
                };
                for (_, tx) in pending.drain() {
                    let _ = tx.send(Err(RequestError::offline()));
                }
                match outcome {
                    Outcome::Stopped => return,
                    Outcome::Rejected => {
                        let _ = events.send(Event::Rejected).await;
                        return;
                    }
                    Outcome::Lost => {
                        if up {
                            let _ = events.send(Event::Down).await;
                        }
                    }
                }
            }
            Ok(Err(e)) => log::debug!("realtime connect failed: {e}"),
            Err(_) => log::debug!("realtime connect timed out"),
        }
        let base = BACKOFF_MS[attempt.min(BACKOFF_MS.len() - 1)];
        attempt += 1;
        let jitter = base * (rand_percent() % 51) / 100;
        let wait = sleep(Duration::from_millis(base + jitter));
        tokio::pin!(wait);
        // Keep answering requests while waiting, so callers don't hang until the next socket.
        loop {
            tokio::select! {
                _ = &mut wait => break,
                cmd = commands.recv() => match cmd {
                    Some(Command::Request { reply, .. }) => { let _ = reply.send(Err(RequestError::offline())); }
                    Some(Command::Stop) | None => return,
                }
            }
        }
    }
}

enum Outcome {
    Lost,
    Rejected,
    Stopped,
}

fn rand_percent() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_url_replaces_the_path() {
        assert_eq!(ws_url("http://pi.local:8080").as_deref(), Some("ws://pi.local:8080/ws"));
        assert_eq!(ws_url("https://h.example.com/x?y=1").as_deref(), Some("wss://h.example.com/ws"));
    }

    #[test]
    fn requests_are_queued_in_call_order_before_anything_awaits() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let rt = Realtime { commands: tx };
        let leave = rt.request("voice:leave", json!({}));
        let join = rt.request("voice:join", json!({}));
        let kinds: Vec<String> = std::iter::from_fn(|| match rx.try_recv() {
            Ok(Command::Request { kind, .. }) => Some(kind),
            _ => None,
        })
        .collect();
        assert_eq!(kinds, ["voice:leave", "voice:join"]);
        drop((leave, join));
    }

    #[test]
    fn errors_print_the_code_the_server_sent() {
        let e = RequestError { code: ErrorCode::parse("brand_new"), reply: json!({ "error": "brand_new" }) };
        assert_eq!((e.code, e.to_string()), (ErrorCode::Unknown, "brand_new".to_string()));
        assert_eq!(RequestError::offline().to_string(), "offline");
    }
}
