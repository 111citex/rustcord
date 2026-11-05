//! Profiles and DMs over a fake REST server, and the member-list subscription
//! through the real gateway session loop.

use std::sync::Arc;
use cord_core::auth::token_store::{MemoryStore, TokenStore};
use cord_core::event::CoreEvent;
use cord_core::http::HttpError;
use cord_core::identity::{Identity, Os};
use cord_core::{Core, CoreConfig, Id};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn identity() -> Identity {
    Identity { os: Os::Linux, locale: "en-US".into(), timezone: "UTC".into(), build_number: 1 }
}

fn core(server: &MockServer) -> Arc<Core> {
    // Nothing listens on port 9: the gateway just keeps retrying in the background.
    let config = CoreConfig {
        profile_dir: None,
        ready_file: None,
        api_base: server.uri(),
        gateway_url: "ws://127.0.0.1:9".into(),
        remote_auth_url: "ws://127.0.0.1:9".into(),
    };
    let tokens: Arc<dyn TokenStore> = Arc::new(MemoryStore::default());
    Core::new(identity(), config, tokens, tokio::runtime::Handle::current())
}

#[tokio::test(flavor = "multi_thread")]
async fn profile_is_fetched_once_per_user_and_server() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/users/2/profile"))
        .and(query_param("guild_id", "300"))
        .and(query_param("with_mutual_guilds", "true"))
        .and(query_param("with_mutual_friends", "true"))
        .and(header("authorization", "tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "user": {"id": "2", "username": "lin"},
            "user_profile": {"bio": "hi"},
            "guild_member": {"nick": "Zenny", "roles": ["301"]},
        })))
        .expect(1).mount(&server).await;
    let c = core(&server);
    let _rx = c.start("tok".into());
    let first = c.user_profile(Id(2), Some(Id(300))).await.unwrap();
    assert_eq!((first.bio.as_deref(), first.nick.as_deref()), (Some("hi"), Some("Zenny")));
    let again = c.user_profile(Id(2), Some(Id(300))).await.unwrap();
    assert!(Arc::ptr_eq(&first, &again), "the second call must come from memory");
}

#[tokio::test(flavor = "multi_thread")]
async fn profile_without_a_server_sends_no_guild_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/users/2/profile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"user": {"id": "2", "username": "lin"}})))
        .expect(1).mount(&server).await;
    let c = core(&server);
    let _rx = c.start("tok".into());
    c.user_profile(Id(2), None).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert!(!requests[0].url.query().unwrap().contains("guild_id"));
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_profile_fetches_are_not_cached() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/users/2/profile"))
        .respond_with(ResponseTemplate::new(404)).expect(2).mount(&server).await;
    let c = core(&server);
    let _rx = c.start("tok".into());
    assert!(matches!(c.user_profile(Id(2), None).await, Err(HttpError::NotFound)));
    assert!(matches!(c.user_profile(Id(2), None).await, Err(HttpError::NotFound)));
}

#[tokio::test(flavor = "multi_thread")]
async fn open_dm_posts_the_recipient_and_returns_the_channel() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/users/@me/channels"))
        .and(body_partial_json(json!({"recipient_id": "2"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "402", "type": 1, "recipients": [{"id": "2", "username": "lin"}]
        })))
        .expect(1).mount(&server).await;
    let c = core(&server);
    let _rx = c.start("tok".into());
    let channel = c.open_dm(Id(2)).await.unwrap();
    assert_eq!(channel.id, Id(402));
    assert_eq!(channel.recipient_ids, vec![Id(2)]);
}

#[test]
fn decodes_member_list_update_dispatch() {
    let d = json!({
        "id": "everyone", "guild_id": "300", "member_count": 3, "online_count": 1,
        "groups": [{"id": "online", "count": 1}, {"id": "offline", "count": 2}],
        "ops": [{"op": "SYNC", "range": [0, 99], "items": [{"group": {"id": "online", "count": 1}}]}],
        "extra": true
    });
    let Some(CoreEvent::MemberListUpdate(u)) = cord_core::event::decode_dispatch("GUILD_MEMBER_LIST_UPDATE", d) else {
        panic!()
    };
    assert_eq!((u.guild_id, u.member_count, u.groups.len(), u.ops.len()), (Id(300), 3, 2, 1));
}
