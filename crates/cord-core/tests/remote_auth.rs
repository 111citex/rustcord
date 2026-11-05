use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use cord_core::Id;
use cord_core::auth::remote::{self, PendingUser, RemoteAuthEvent};
use cord_core::http::Http;
use cord_core::identity::{Identity, Os};
use futures_util::{SinkExt, StreamExt};
use rsa::pkcs8::DecodePublicKey;
use rsa::{Oaep, RsaPrivateKey, RsaPublicKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{WebSocketStream, accept_async};
use cord_core::auth::CaptchaSolution;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const WAIT: Duration = Duration::from_secs(5);

fn http(server: &MockServer) -> Http {
    let id = Identity { os: Os::Linux, locale: "en-US".into(), timezone: "UTC".into(), build_number: 1 };
    Http::new(id, &server.uri())
}

fn small_key() -> RsaPrivateKey {
    // 1024 bits keeps the test fast; production uses 2048.
    RsaPrivateKey::new(&mut rand::thread_rng(), 1024).unwrap()
}

fn encrypt(pk: &RsaPublicKey, data: &[u8]) -> String {
    STANDARD.encode(pk.encrypt(&mut rand::thread_rng(), Oaep::new::<Sha256>(), data).unwrap())
}

async fn send(ws: &mut WebSocketStream<TcpStream>, v: Value) {
    ws.send(Message::text(v.to_string())).await.unwrap();
}

async fn recv_op(ws: &mut WebSocketStream<TcpStream>, op: &str) -> Value {
    timeout(WAIT, async {
        loop {
            if let Message::Text(t) = ws.next().await.unwrap().unwrap() {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["op"] == op {
                    return v;
                }
            }
        }
    })
    .await
    .expect("timed out")
}

/// Accepts one connection and runs the handshake up to the QR code.
async fn handshake(listener: &TcpListener) -> (WebSocketStream<TcpStream>, RsaPublicKey) {
    let (tcp, _) = timeout(WAIT, listener.accept()).await.unwrap().unwrap();
    let mut ws = accept_async(tcp).await.unwrap();
    send(&mut ws, json!({"op": "hello", "heartbeat_interval": 41250, "timeout_ms": 120000})).await;
    let init = recv_op(&mut ws, "init").await;
    let der = STANDARD.decode(init["encoded_public_key"].as_str().unwrap()).unwrap();
    let pk = RsaPublicKey::from_public_key_der(&der).unwrap();

    send(&mut ws, json!({"op": "nonce_proof", "encrypted_nonce": encrypt(&pk, b"nonce-123")})).await;
    let proof = recv_op(&mut ws, "nonce_proof").await;
    assert_eq!(proof["proof"], URL_SAFE_NO_PAD.encode(Sha256::digest(b"nonce-123")));

    send(&mut ws, json!({"op": "pending_remote_init", "fingerprint": "fp1"})).await;
    (ws, pk)
}

async fn next(auth: &mut remote::RemoteAuth) -> RemoteAuthEvent {
    timeout(WAIT, auth.events.recv()).await.expect("timed out").expect("channel closed")
}

#[tokio::test]
async fn full_qr_flow_yields_token() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let api = MockServer::start().await;
    let mut auth = remote::start_with_key(&url, http(&api), small_key());

    let (mut ws, pk) = handshake(&listener).await;
    assert!(matches!(next(&mut auth).await, RemoteAuthEvent::Qr(u) if u == "https://discord.com/ra/fp1"));

    send(&mut ws, json!({"op": "pending_ticket", "encrypted_user_payload": encrypt(&pk, b"100:0:abc:ada")})).await;
    match next(&mut auth).await {
        RemoteAuthEvent::Scanned(u) => assert_eq!(u, PendingUser { id: Id(100), username: "ada".into(), avatar: Some("abc".into()) }),
        _ => panic!("expected Scanned"),
    }

    Mock::given(method("POST")).and(path("/users/@me/remote-auth/login"))
        .and(body_json(json!({"ticket": "t1"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"encrypted_token": encrypt(&pk, b"tok-xyz")})))
        .expect(1).mount(&api).await;
    send(&mut ws, json!({"op": "pending_login", "ticket": "t1"})).await;
    assert!(matches!(next(&mut auth).await, RemoteAuthEvent::Token(t) if t == "tok-xyz"));
}

#[tokio::test]
async fn cancel_on_phone() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let api = MockServer::start().await;
    let mut auth = remote::start_with_key(&url, http(&api), small_key());
    let (mut ws, _) = handshake(&listener).await;
    next(&mut auth).await; // Qr
    send(&mut ws, json!({"op": "cancel"})).await;
    assert!(matches!(next(&mut auth).await, RemoteAuthEvent::Cancelled));
}

#[tokio::test]
async fn server_close_means_expired() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let api = MockServer::start().await;
    let mut auth = remote::start_with_key(&url, http(&api), small_key());
    let (mut ws, _) = handshake(&listener).await;
    next(&mut auth).await; // Qr
    ws.close(None).await.unwrap();
    assert!(matches!(next(&mut auth).await, RemoteAuthEvent::Expired));
}

#[tokio::test]
async fn unreachable_server_fails() {
    let api = MockServer::start().await;
    let mut auth = remote::start_with_key("ws://127.0.0.1:9", http(&api), small_key());
    assert!(matches!(next(&mut auth).await, RemoteAuthEvent::Failed(_)));
}

#[tokio::test]
async fn silent_server_expires() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let api = MockServer::start().await;
    let mut auth = remote::start_with_key(&url, http(&api), small_key());
    let (tcp, _) = timeout(WAIT, listener.accept()).await.unwrap().unwrap();
    let mut ws = accept_async(tcp).await.unwrap();
    send(&mut ws, json!({"op": "hello", "heartbeat_interval": 41250, "timeout_ms": 100})).await;
    // Stay silent without closing; the session deadline must fire.
    assert!(matches!(next(&mut auth).await, RemoteAuthEvent::Expired));
    drop(ws);
}

#[tokio::test]
async fn no_hello_fails() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let api = MockServer::start().await;
    let mut auth = remote::start_with_key_and_timeout(&url, http(&api), small_key(), Duration::from_millis(200));
    let (tcp, _) = timeout(WAIT, listener.accept()).await.unwrap().unwrap();
    let ws = accept_async(tcp).await.unwrap();
    assert!(matches!(next(&mut auth).await, RemoteAuthEvent::Failed(_)));
    drop(ws);
}

fn captcha_required() -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({
        "captcha_key": ["captcha-required"],
        "captcha_sitekey": "site-1",
        "captcha_service": "hcaptcha",
        "captcha_session_id": "sid",
        "captcha_rqdata": "rqd",
        "captcha_rqtoken": "rqt"
    }))
}

/// Scans and approves, up to the ticket exchange.
async fn to_pending_login(listener: &TcpListener, auth: &mut remote::RemoteAuth) -> (WebSocketStream<TcpStream>, RsaPublicKey) {
    let (mut ws, pk) = handshake(listener).await;
    next(auth).await; // Qr
    send(&mut ws, json!({"op": "pending_ticket", "encrypted_user_payload": encrypt(&pk, b"100:0:abc:ada")})).await;
    next(auth).await; // Scanned
    (ws, pk)
}

#[tokio::test]
async fn captcha_on_ticket_exchange_is_solved_and_retried() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let api = MockServer::start().await;
    let mut auth = remote::start_with_key(&url, http(&api), small_key());
    let (mut ws, pk) = to_pending_login(&listener, &mut auth).await;

    // The retry carries the solved key and the challenge's rqtoken/session id.
    Mock::given(method("POST")).and(path("/users/@me/remote-auth/login"))
        .and(header("x-captcha-key", "P1_solved"))
        .and(header("x-captcha-rqtoken", "rqt"))
        .and(header("x-captcha-session-id", "sid"))
        .and(body_json(json!({"ticket": "t1"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"encrypted_token": encrypt(&pk, b"tok-xyz")})))
        .expect(1).mount(&api).await;
    Mock::given(method("POST")).and(path("/users/@me/remote-auth/login"))
        .respond_with(captcha_required())
        .mount(&api).await;

    send(&mut ws, json!({"op": "pending_login", "ticket": "t1"})).await;
    let challenge = match next(&mut auth).await {
        RemoteAuthEvent::CaptchaRequired(c) => c,
        _ => panic!("expected CaptchaRequired"),
    };
    assert_eq!(challenge.sitekey, "site-1");
    assert_eq!(challenge.rqdata.as_deref(), Some("rqd"));

    auth.captcha_sender().send(Some(CaptchaSolution::for_challenge(&challenge, "P1_solved".into()))).unwrap();
    assert!(matches!(next(&mut auth).await, RemoteAuthEvent::Token(t) if t == "tok-xyz"));
}

#[tokio::test]
async fn closed_captcha_fails_the_qr_login() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let api = MockServer::start().await;
    let mut auth = remote::start_with_key(&url, http(&api), small_key());
    let (mut ws, _) = to_pending_login(&listener, &mut auth).await;

    Mock::given(method("POST")).and(path("/users/@me/remote-auth/login"))
        .respond_with(captcha_required())
        .mount(&api).await;
    send(&mut ws, json!({"op": "pending_login", "ticket": "t1"})).await;
    assert!(matches!(next(&mut auth).await, RemoteAuthEvent::CaptchaRequired(_)));

    auth.captcha_sender().send(None).unwrap(); // window closed
    assert!(matches!(next(&mut auth).await, RemoteAuthEvent::Failed(m) if m.contains("captcha")));
}

#[tokio::test]
async fn rejected_captcha_fails_without_asking_again() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let api = MockServer::start().await;
    let mut auth = remote::start_with_key(&url, http(&api), small_key());
    let (mut ws, _) = to_pending_login(&listener, &mut auth).await;

    // With a key: Discord rejects it (and the ticket is then spent).
    Mock::given(method("POST")).and(path("/users/@me/remote-auth/login"))
        .and(header("x-captcha-key", "P1_bad"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"captcha_key": ["invalid-response"]})))
        .mount(&api).await;
    Mock::given(method("POST")).and(path("/users/@me/remote-auth/login"))
        .respond_with(captcha_required())
        .mount(&api).await;

    send(&mut ws, json!({"op": "pending_login", "ticket": "t1"})).await;
    let RemoteAuthEvent::CaptchaRequired(challenge) = next(&mut auth).await else { panic!("expected CaptchaRequired") };
    auth.captcha_sender().send(Some(CaptchaSolution::for_challenge(&challenge, "P1_bad".into()))).unwrap();
    assert!(matches!(next(&mut auth).await, RemoteAuthEvent::Failed(m) if m.contains("didn't accept the captcha")));
}
