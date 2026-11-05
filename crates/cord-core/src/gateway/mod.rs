//! One gateway session: connect, IDENTIFY or RESUME, heartbeat, decode
//! dispatches, and reconnect with backoff until the handle is dropped or the
//! token is rejected. Payloads are never logged: they carry the token.

pub mod inflate;

use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::event::{ConnectionState, CoreEvent, decode_dispatch};
use crate::identity::Identity;
use inflate::Inflater;

pub const GATEWAY_URL: &str = "wss://gateway.discord.gg";
const QUERY: &str = "/?v=9&encoding=json&compress=zlib-stream";

/// The web client's IDENTIFY capabilities bitfield. It changes READY's shape,
/// so it must match what `model` decodes. Refresh from the browser (Step 7).
pub const CAPABILITIES: u64 = 161_789;

/// Connecting plus receiving HELLO must finish within this.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// After IDENTIFY or RESUME, READY or RESUMED must arrive within this.
pub const READY_TIMEOUT: Duration = Duration::from_secs(60);

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Sink = SplitSink<Ws, WsMessage>;

#[derive(Clone)]
pub struct GatewayConfig {
    pub url: String,
    pub token: String,
    pub identity: Identity,
    pub backoff_base: Duration,
    pub handshake_timeout: Duration,
    pub ready_timeout: Duration,
}

impl GatewayConfig {
    pub fn new(url: impl Into<String>, token: String, identity: Identity) -> Self {
        GatewayConfig { url: url.into(), token, identity, backoff_base: Duration::from_secs(1),
            handshake_timeout: HANDSHAKE_TIMEOUT,
            ready_timeout: READY_TIMEOUT,
        }
    }
}

pub struct GatewayHandle {
    commands: mpsc::UnboundedSender<Value>,
    task: JoinHandle<()>,
}

impl GatewayHandle {
    /// Sends a raw gateway payload (e.g. a presence update). Payloads are queued
    /// until the session is established (READY or RESUMED) and then sent in order.
    pub fn send(&self, payload: Value) {
        let _ = self.commands.send(payload);
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

impl Drop for GatewayHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn spawn(config: GatewayConfig, events: mpsc::UnboundedSender<CoreEvent>) -> GatewayHandle {
    let (commands, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(run(config, events, rx));
    GatewayHandle { commands, task }
}

/// 1s, 2s, 4s … capped at 60 × `base`.
pub fn backoff(attempt: u32, base: Duration) -> Duration {
    base * 2u32.saturating_pow(attempt.saturating_sub(1)).min(60)
}

#[derive(Default)]
struct Session {
    id: Option<String>,
    resume_url: Option<String>,
    seq: Option<u64>,
}

enum End {
    AuthFailed,
    Reconnect { resumable: bool },
}

async fn run(config: GatewayConfig, events: mpsc::UnboundedSender<CoreEvent>, mut commands: mpsc::UnboundedReceiver<Value>) {
    let mut session = Session::default();
    let mut attempt = 0u32;
    loop {
        let state = if attempt == 0 { ConnectionState::Connecting } else { ConnectionState::Reconnecting { attempt } };
        if events.send(CoreEvent::Connection(state)).is_err() {
            return; // nobody is listening any more
        }
        let (end, reached_ready) = connect_once(&config, &mut session, &events, &mut commands).await;
        match end {
            End::AuthFailed => {
                let _ = events.send(CoreEvent::AuthFailed);
                return;
            }
            End::Reconnect { resumable: false } => session = Session::default(),
            End::Reconnect { resumable: true } => {}
        }
        attempt = if reached_ready { 1 } else { attempt + 1 };
        tokio::time::sleep(backoff(attempt, config.backoff_base)).await;
    }
}

async fn send(sink: &mut Sink, payload: Value) -> bool {
    sink.send(WsMessage::text(payload.to_string())).await.is_ok()
}

fn identify(config: &GatewayConfig) -> Value {
    json!({"op": 2, "d": {
        "token": config.token,
        "capabilities": CAPABILITIES,
        "properties": config.identity.super_properties(),
        "presence": {"status": "unknown", "since": 0, "activities": [], "afk": false},
        "compress": false,
        "client_state": {"guild_versions": {}},
    }})
}

fn close_outcome(code: Option<u16>) -> End {
    match code {
        Some(4004) => End::AuthFailed,
        // Invalid seq / session timed out: start a new session.
        Some(4007) | Some(4009) => End::Reconnect { resumable: false },
        _ => End::Reconnect { resumable: true },
    }
}

async fn connect_once(
    config: &GatewayConfig,
    session: &mut Session,
    events: &mpsc::UnboundedSender<CoreEvent>,
    commands: &mut mpsc::UnboundedReceiver<Value>,
) -> (End, bool) {
    let reconnect = |ready| (End::Reconnect { resumable: true }, ready);
    let base = session.resume_url.as_deref().filter(|_| session.id.is_some()).unwrap_or(&config.url);
    let url = format!("{}{QUERY}", base.trim_end_matches('/'));

    let Ok(mut request) = url.as_str().into_client_request() else {
        tracing::error!(%url, "bad gateway URL");
        return (End::Reconnect { resumable: false }, false);
    };
    let headers = request.headers_mut();
    if let Ok(ua) = HeaderValue::from_str(&config.identity.user_agent()) {
        headers.insert("user-agent", ua);
    }
    headers.insert("origin", HeaderValue::from_static("https://discord.com"));

    let started = Instant::now();
    let ws = match tokio::time::timeout(config.handshake_timeout, tokio_tungstenite::connect_async(request)).await {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(error)) => {
            tracing::warn!(%error, "gateway connect failed");
            return reconnect(false);
        }
        Err(_) => {
            tracing::warn!("gateway connect timed out");
            return reconnect(false);
        }
    };
    let (mut sink, mut stream): (Sink, SplitStream<Ws>) = ws.split();
    let mut inflater = Inflater::new();
    let mut ready = false;
    let mut heartbeat: Option<(Duration, Instant)> = None; // interval, next due
    let mut acked = true;
    // HELLO is due within the handshake timeout of starting to connect; once
    // IDENTIFY/RESUME is sent, READY/RESUMED is due within the ready timeout.
    let mut deadline: Option<Instant> = Some(started + config.handshake_timeout);

    loop {
        let due = heartbeat.map(|(_, at)| at).unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
        tokio::select! {
            _ = sleep_until(deadline.unwrap_or(due)), if deadline.is_some() => {
                tracing::warn!(ready, "gateway handshake timed out; reconnecting");
                return reconnect(ready);
            }
            _ = sleep_until(due), if heartbeat.is_some() => {
                if !acked {
                    tracing::warn!("heartbeat not acknowledged; reconnecting");
                    return reconnect(ready);
                }
                acked = false;
                if !send(&mut sink, json!({"op": 1, "d": session.seq})).await {
                    return reconnect(ready);
                }
                let (interval, _) = heartbeat.unwrap();
                heartbeat = Some((interval, Instant::now() + interval));
            }
            payload = commands.recv(), if ready => match payload {
                Some(payload) => { send(&mut sink, payload).await; }
                None => return reconnect(ready),
            },
            frame = stream.next() => {
                let text = match frame {
                    Some(Ok(WsMessage::Binary(bytes))) => match inflater.push(&bytes) {
                        Ok(Some(text)) => text,
                        Ok(None) => continue,
                        Err(error) => {
                            tracing::warn!(%error, "gateway inflate failed");
                            return reconnect(ready);
                        }
                    },
                    Some(Ok(WsMessage::Text(text))) => text.to_string(),
                    Some(Ok(WsMessage::Close(frame))) => {
                        let code = frame.map(|f| u16::from(f.code));
                        tracing::info!(?code, "gateway closed");
                        return (close_outcome(code), ready);
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(error)) => {
                        tracing::warn!(%error, "gateway read failed");
                        return reconnect(ready);
                    }
                    None => return reconnect(ready),
                };
                let Ok(mut payload) = serde_json::from_str::<Value>(&text) else { continue };
                match payload["op"].as_u64() {
                    Some(10) => {
                        let interval = Duration::from_millis(
                            payload["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250).clamp(1_000, 120_000),
                        );
                        heartbeat = Some((interval, Instant::now() + interval.mul_f64(fastrand::f64())));
                        let hello_reply = match (&session.id, session.seq) {
                            (Some(id), Some(seq)) => json!({"op": 6, "d": {"token": config.token, "session_id": id, "seq": seq}}),
                            _ => identify(config),
                        };
                        if !send(&mut sink, hello_reply).await {
                            return reconnect(ready);
                        }
                        deadline = Some(Instant::now() + config.ready_timeout);
                    }
                    Some(11) => acked = true,
                    Some(1) => {
                        if !send(&mut sink, json!({"op": 1, "d": session.seq})).await {
                            return reconnect(ready);
                        }
                    }
                    Some(7) => return reconnect(ready),
                    Some(9) => {
                        let resumable = payload["d"].as_bool().unwrap_or(false);
                        return (End::Reconnect { resumable }, ready);
                    }
                    Some(0) => {
                        if let Some(s) = payload["s"].as_u64() {
                            session.seq = Some(s);
                        }
                        let t = payload["t"].as_str().unwrap_or_default().to_string();
                        let d = payload["d"].take();
                        if t == "READY" {
                            session.id = d["session_id"].as_str().map(str::to_string);
                            session.resume_url = d["resume_gateway_url"].as_str().map(str::to_string);
                        }
                        if let Some(event) = decode_dispatch(&t, d) {
                            let _ = events.send(event);
                        }
                        if t == "READY" || t == "RESUMED" {
                            tracing::info!(dispatch = %t, "gateway session established");
                            ready = true;
                            deadline = None;
                            let _ = events.send(CoreEvent::Connection(ConnectionState::Connected));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps_at_sixty() {
        let s = Duration::from_secs(1);
        let got: Vec<u64> = (1..=9).map(|a| backoff(a, s).as_secs()).collect();
        assert_eq!(got, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    }
}
