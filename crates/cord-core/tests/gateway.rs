//! A fake gateway on localhost speaks zlib-stream JSON to the real session loop.

use std::time::Duration;

use cord_core::event::{ConnectionState, CoreEvent};
use cord_core::Id;
use cord_core::gateway::{self, GatewayConfig};
use cord_core::identity::{Identity, Os};
use flate2::{Compress, Compression, FlushCompress};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{WebSocketStream, accept_async};

const WAIT: Duration = Duration::from_secs(5);

struct FakeGateway {
    listener: TcpListener,
    url: String,
}

impl FakeGateway {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        FakeGateway { listener, url }
    }

    async fn accept(&self) -> Conn {
        let (tcp, _) = timeout(WAIT, self.listener.accept()).await.expect("no connection").unwrap();
        Conn { ws: accept_async(tcp).await.unwrap(), z: Compress::new(Compression::default(), true) }
    }

    async fn no_connection_within(&self, d: Duration) -> bool {
        timeout(d, self.listener.accept()).await.is_err()
    }
}

struct Conn {
    ws: WebSocketStream<TcpStream>,
    z: Compress,
}

impl Conn {
    async fn send(&mut self, v: Value) {
        let raw = v.to_string();
        let mut out = Vec::with_capacity(raw.len() * 2 + 64);
        self.z.compress_vec(raw.as_bytes(), &mut out, FlushCompress::Sync).unwrap();
        self.ws.send(Message::binary(out)).await.unwrap();
    }

    async fn hello(&mut self, interval_ms: u64) {
        self.send(json!({"op": 10, "d": {"heartbeat_interval": interval_ms}})).await;
    }

    /// Next client payload that matches `f`, skipping the rest.
    async fn recv_where(&mut self, f: impl Fn(&Value) -> bool) -> Value {
        timeout(WAIT, async {
            loop {
                if let Message::Text(t) = self.ws.next().await.unwrap().unwrap() {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    if f(&v) {
                        return v;
                    }
                }
            }
        })
        .await
        .expect("timed out waiting for client payload")
    }

    /// Next client payload that is not a heartbeat.
    async fn recv_non_heartbeat(&mut self) -> Value {
        self.recv_where(|v| v["op"] != 1).await
    }

    async fn close(mut self, code: u16) {
        let frame = CloseFrame { code: CloseCode::from(code), reason: "".into() };
        let _ = self.ws.close(Some(frame)).await;
    }
}

fn ready(seq: u64, resume_url: &str) -> Value {
    json!({"op": 0, "t": "READY", "s": seq, "d": {
        "user": {"id": "1", "username": "me"},
        "session_id": "sess-1",
        "resume_gateway_url": resume_url,
        "guilds": [], "private_channels": [], "users": []
    }})
}

fn config(url: &str) -> GatewayConfig {
    let identity = Identity { os: Os::Linux, locale: "en-US".into(), timezone: "UTC".into(), build_number: 1 };
    let mut c = GatewayConfig::new(url, "tok".into(), identity);
    c.backoff_base = Duration::from_millis(10);
    c
}

async fn next_matching(rx: &mut UnboundedReceiver<CoreEvent>, f: impl Fn(&CoreEvent) -> bool) -> CoreEvent {
    timeout(WAIT, async {
        loop {
            let e = rx.recv().await.expect("event channel closed");
            if f(&e) {
                return e;
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

#[tokio::test]
async fn identifies_and_emits_ready() {
    let gw = FakeGateway::start().await;
    let (tx, mut rx) = unbounded_channel();
    let _h = gateway::spawn(config(&gw.url), tx);

    assert!(matches!(rx.recv().await, Some(CoreEvent::Connection(ConnectionState::Connecting))));
    let mut c = gw.accept().await;
    c.hello(45_000).await;
    let identify = c.recv_non_heartbeat().await;
    assert_eq!(identify["op"], 2);
    assert_eq!(identify["d"]["token"], "tok");
    assert_eq!(identify["d"]["properties"]["os"], "Linux");
    assert_eq!(identify["d"]["capabilities"], gateway::CAPABILITIES);

    c.send(ready(1, &gw.url)).await;
    c.send(json!({"op": 0, "t": "MESSAGE_CREATE", "s": 2, "d": {
        "id": "9", "channel_id": "5", "author": {"id": "2", "username": "lin"},
        "content": "hello", "timestamp": "t"
    }})).await;

    assert!(matches!(next_matching(&mut rx, |e| matches!(e, CoreEvent::Ready(_))).await, CoreEvent::Ready(_)));
    next_matching(&mut rx, |e| matches!(e, CoreEvent::Connection(ConnectionState::Connected))).await;
    let CoreEvent::MessageCreate(m) = next_matching(&mut rx, |e| matches!(e, CoreEvent::MessageCreate(_))).await else { unreachable!() };
    assert_eq!(m.content, "hello");
}

#[tokio::test]
async fn resumes_after_drop() {
    let gw = FakeGateway::start().await;
    let (tx, mut rx) = unbounded_channel();
    let _h = gateway::spawn(config(&gw.url), tx);

    let mut c = gw.accept().await;
    c.hello(45_000).await;
    c.recv_non_heartbeat().await;
    // resume_gateway_url points back at the fake server.
    c.send(ready(1, &gw.url)).await;
    c.send(json!({"op": 0, "t": "MESSAGE_CREATE", "s": 2, "d": {
        "id": "9", "channel_id": "5", "author": {"id": "2", "username": "lin"}, "timestamp": "t"
    }})).await;
    // Wait until seq 2 has been processed before dropping the connection.
    next_matching(&mut rx, |e| matches!(e, CoreEvent::MessageCreate(_))).await;
    drop(c); // TCP drop, no close frame

    next_matching(&mut rx, |e| matches!(e, CoreEvent::Connection(ConnectionState::Reconnecting { attempt: 1 }))).await;
    let mut c = gw.accept().await;
    c.hello(45_000).await;
    let resume = c.recv_non_heartbeat().await;
    assert_eq!(resume["op"], 6, "expected RESUME, got {resume}");
    assert_eq!(resume["d"]["session_id"], "sess-1");
    assert_eq!(resume["d"]["seq"], 2);
    assert_eq!(resume["d"]["token"], "tok");

    c.send(json!({"op": 0, "t": "RESUMED", "s": 3, "d": {}})).await;
    next_matching(&mut rx, |e| matches!(e, CoreEvent::Connection(ConnectionState::Connected))).await;
}

#[tokio::test]
async fn invalid_session_reidentifies() {
    let gw = FakeGateway::start().await;
    let (tx, _rx) = unbounded_channel();
    let _h = gateway::spawn(config(&gw.url), tx);

    let mut c = gw.accept().await;
    c.hello(45_000).await;
    c.recv_non_heartbeat().await;
    c.send(ready(1, &gw.url)).await;
    c.send(json!({"op": 9, "d": false})).await;

    let mut c = gw.accept().await;
    c.hello(45_000).await;
    assert_eq!(c.recv_non_heartbeat().await["op"], 2);
}

#[tokio::test]
async fn auth_failed_stops() {
    let gw = FakeGateway::start().await;
    let (tx, mut rx) = unbounded_channel();
    let h = gateway::spawn(config(&gw.url), tx);

    let mut c = gw.accept().await;
    c.hello(45_000).await;
    c.recv_non_heartbeat().await;
    c.close(4004).await;

    next_matching(&mut rx, |e| matches!(e, CoreEvent::AuthFailed)).await;
    assert!(gw.no_connection_within(Duration::from_millis(300)).await, "must not reconnect after 4004");
    assert!(h.is_finished());
}

#[tokio::test]
async fn answers_heartbeat_request_with_last_seq() {
    let gw = FakeGateway::start().await;
    let (tx, _rx) = unbounded_channel();
    let _h = gateway::spawn(config(&gw.url), tx);

    let mut c = gw.accept().await;
    c.hello(45_000).await;
    c.recv_non_heartbeat().await;
    c.send(ready(7, &gw.url)).await;
    c.send(json!({"op": 1, "d": null})).await;
    c.recv_where(|v| v["op"] == 1 && v["d"] == 7).await;
}

#[tokio::test]
async fn reconnects_when_heartbeats_go_unacknowledged() {
    let gw = FakeGateway::start().await;
    let (tx, _rx) = unbounded_channel();
    let _h = gateway::spawn(config(&gw.url), tx);

    let mut c = gw.accept().await;
    c.hello(50).await; // never ACKed
    c.recv_non_heartbeat().await;
    let _second = gw.accept().await; // zombie detected, reconnected
    drop(c);
}

#[tokio::test]
async fn member_list_subscription_reaches_the_server_and_updates_come_back() {
    let gw = FakeGateway::start().await;
    let (tx, mut rx) = unbounded_channel();
    let h = gateway::spawn(config(&gw.url), tx);
    h.send(cord_core::member_list::subscribe_payload(Id(300), Id(311), &[[0, 99]]));

    let mut c = gw.accept().await;
    c.hello(45_000).await;
    c.recv_non_heartbeat().await;
    c.send(ready(1, &gw.url)).await;
    let sub = c.recv_non_heartbeat().await;
    assert_eq!(sub["op"], 14);
    assert_eq!(sub["d"]["guild_id"], "300");
    assert_eq!(sub["d"]["channels"]["311"], json!([[0, 99]]));

    c.send(json!({"op": 0, "t": "GUILD_MEMBER_LIST_UPDATE", "s": 2, "d": {
        "id": "everyone", "guild_id": "300", "member_count": 1, "online_count": 1,
        "groups": [{"id": "online", "count": 1}],
        "ops": [{"op": "SYNC", "range": [0, 99], "items": [
            {"group": {"id": "online", "count": 1}},
            {"member": {"user": {"id": "2", "username": "lin"}, "roles": [], "presence": {"status": "online", "activities": []}}}
        ]}]
    }})).await;
    let CoreEvent::MemberListUpdate(u) = next_matching(&mut rx, |e| matches!(e, CoreEvent::MemberListUpdate(_))).await
    else {
        unreachable!()
    };
    assert_eq!(u.guild_id, Id(300));
    assert_eq!(u.members().count(), 1);
}

#[tokio::test]
async fn commands_wait_for_ready() {
    let gw = FakeGateway::start().await;
    let (tx, _rx) = unbounded_channel();
    let h = gateway::spawn(config(&gw.url), tx);
    h.send(json!({"op": 3, "d": {"status": "idle"}}));

    let mut c = gw.accept().await;
    c.hello(45_000).await;
    assert_eq!(c.recv_non_heartbeat().await["op"], 2, "IDENTIFY must precede queued commands");
    c.send(ready(1, &gw.url)).await;
    let queued = c.recv_non_heartbeat().await;
    assert_eq!(queued["op"], 3);
    assert_eq!(queued["d"]["status"], "idle");
}

#[tokio::test]
async fn reconnects_when_no_hello() {
    let gw = FakeGateway::start().await;
    let (tx, _rx) = unbounded_channel();
    let mut cfg = config(&gw.url);
    cfg.handshake_timeout = Duration::from_millis(200);
    let _h = gateway::spawn(cfg, tx);

    let _silent = gw.accept().await; // accepted, never says HELLO
    let _second = gw.accept().await;
}

#[tokio::test]
async fn reconnects_when_no_ready() {
    let gw = FakeGateway::start().await;
    let (tx, _rx) = unbounded_channel();
    let mut cfg = config(&gw.url);
    cfg.ready_timeout = Duration::from_millis(200);
    let _h = gateway::spawn(cfg, tx);

    let mut c = gw.accept().await;
    c.hello(45_000).await;
    assert_eq!(c.recv_non_heartbeat().await["op"], 2);
    // No READY: the client gives up and reconnects.
    let _second = gw.accept().await;
    drop(c);
}
