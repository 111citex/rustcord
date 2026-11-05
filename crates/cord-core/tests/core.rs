use std::sync::Arc;
use std::time::Duration;

use cord_core::auth::token_store::{MemoryStore, TokenStore};
use cord_core::event::CoreEvent;
use cord_core::http::HttpError;
use cord_core::identity::{Identity, Os};
use cord_core::{Core, CoreConfig, Id};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn identity() -> Identity {
    Identity { os: Os::Linux, locale: "en-US".into(), timezone: "UTC".into(), build_number: 1 }
}

fn core(server: &MockServer, tokens: Arc<MemoryStore>) -> Arc<Core> {
    // Nothing listens on port 9: the gateway just keeps retrying in the background.
    let config = CoreConfig {
        profile_dir: None,
        ready_file: None,
        api_base: server.uri(),
        gateway_url: "ws://127.0.0.1:9".into(),
        remote_auth_url: "ws://127.0.0.1:9".into(),
    };
    Core::new(identity(), config, tokens as Arc<dyn TokenStore>, tokio::runtime::Handle::current())
}

fn message(id: &str, content: &str) -> Value {
    json!({"id": id, "channel_id": "5", "author": {"id": "2", "username": "lin"}, "content": content, "timestamp": "t"})
}

#[tokio::test(flavor = "multi_thread")]
async fn fetches_history_with_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/channels/5/messages")).and(query_param("limit", "50"))
        .and(header("authorization", "tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([message("2", "b"), message("1", "a")])))
        .expect(1).mount(&server).await;
    let c = core(&server, Arc::default());
    let _rx = c.start("tok".into());
    let msgs = c.messages(Id(5), 50).await.unwrap();
    assert_eq!(msgs.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(), ["b", "a"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn send_message_posts_content_and_nonce() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/channels/5/messages"))
        .and(body_partial_json(json!({"content": "hi", "nonce": "123", "enforce_nonce": true, "tts": false})))
        .respond_with(ResponseTemplate::new(200).set_body_json(message("9", "hi")))
        .expect(1).mount(&server).await;
    let c = core(&server, Arc::default());
    let _rx = c.start("tok".into());
    let m = c.send_message(Id(5), "hi".into(), "123".into(), None, vec![]).await.unwrap();
    assert_eq!(m.id, Id(9));
}

#[tokio::test(flavor = "multi_thread")]
async fn unauthorized_rest_emits_auth_failed() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(401)).mount(&server).await;
    let c = core(&server, Arc::default());
    let mut rx = c.start("tok".into());
    assert!(matches!(c.messages(Id(5), 50).await, Err(HttpError::Unauthorized)));
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(e) = rx.recv().await {
            if matches!(e, CoreEvent::AuthFailed) {
                return;
            }
        }
        panic!("channel closed without AuthFailed");
    })
    .await
    .expect("no AuthFailed");
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_out_forgets_token_everywhere() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_json(json!([]))).mount(&server).await;
    let tokens = Arc::new(MemoryStore::default());
    let c = core(&server, tokens.clone());
    c.save_token("tok").unwrap();
    assert_eq!(c.saved_token().as_deref(), Some("tok"));
    let _rx = c.start("tok".into());
    c.sign_out();
    assert_eq!(tokens.load(), None);
    let _ = c.messages(Id(5), 50).await;
    let last = server.received_requests().await.unwrap().pop().unwrap();
    assert!(!last.headers.contains_key("authorization"));
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_identity_uses_fresh_cache_without_network() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("build_number"), "123456").unwrap();
    let id = cord_core::startup_identity(dir.path()).await;
    assert_eq!(id.build_number, 123456);
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_identity_never_waits_when_it_has_an_old_number() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("build_number");
    std::fs::write(&file, "123456").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 3600);
    std::fs::File::options().write(true).open(&file).unwrap().set_modified(old).unwrap();
    let started = std::time::Instant::now();
    let id = cord_core::startup_identity(dir.path()).await;
    assert_eq!(id.build_number, 123456);
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn fetches_older_messages_before_an_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/channels/5/messages"))
        .and(query_param("before", "100")).and(query_param("limit", "50"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([message("99", "older")])))
        .expect(1).mount(&server).await;
    let c = core(&server, Arc::default());
    let _rx = c.start("tok".into());
    let msgs = c.messages_before(Id(5), Id(100), 50).await.unwrap();
    assert_eq!(msgs[0].content, "older");
}

#[tokio::test(flavor = "multi_thread")]
async fn reactions_put_and_delete_with_encoded_emoji() {
    use cord_core::model::ReactionEmoji;
    let server = MockServer::start().await;
    Mock::given(method("PUT")).and(path("/channels/5/messages/6/reactions/%E2%9C%85/@me"))
        .and(header("authorization", "tok"))
        .respond_with(ResponseTemplate::new(204)).expect(1).mount(&server).await;
    Mock::given(method("DELETE")).and(path("/channels/5/messages/6/reactions/pog:9/@me"))
        .respond_with(ResponseTemplate::new(204)).expect(1).mount(&server).await;
    let c = core(&server, Arc::default());
    let _rx = c.start("tok".into());
    let check = ReactionEmoji { id: None, name: Some("\u{2705}".into()), animated: false };
    c.set_reaction(Id(5), Id(6), &check, true).await.unwrap();
    let custom = ReactionEmoji { id: Some(Id(9)), name: Some("pog".into()), animated: false };
    c.set_reaction(Id(5), Id(6), &custom, false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn reaction_failure_is_reported() {
    use cord_core::model::ReactionEmoji;
    let server = MockServer::start().await;
    Mock::given(method("PUT")).respond_with(ResponseTemplate::new(403).set_body_json(json!({"message": "Missing Permissions"})))
        .mount(&server).await;
    let c = core(&server, Arc::default());
    let _rx = c.start("tok".into());
    let e = ReactionEmoji { id: None, name: Some("x".into()), animated: false };
    assert!(matches!(c.set_reaction(Id(5), Id(6), &e, true).await, Err(HttpError::Forbidden(_))));
}
