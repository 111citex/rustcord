use std::time::Duration;

use cord_core::http::{Http, HttpError};
use cord_core::identity::{Identity, Os};
use serde_json::{Value, json};
use wiremock::matchers::{header, header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn http(server: &MockServer) -> Http {
    let id = Identity { os: Os::Linux, locale: "en-US".into(), timezone: "UTC".into(), build_number: 1 };
    let h = Http::new(id, &server.uri());
    h.set_token(Some("tok".into()));
    h
}

#[tokio::test]
async fn sends_identity_and_auth_headers() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/users/@me"))
        .and(header("authorization", "tok"))
        .and(header_exists("x-super-properties"))
        .and(header("origin", "https://discord.com"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"1"})))
        .expect(1).mount(&server).await;
    let v: Value = http(&server).get("/users/@me").await.unwrap();
    assert_eq!(v["id"], "1");
}

#[tokio::test]
async fn retries_after_429() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/x"))
        .respond_with(ResponseTemplate::new(429)
            .insert_header("retry-after", "0.05")
            .set_body_json(json!({"retry_after":0.05,"global":false})))
        .up_to_n_times(1).mount(&server).await;
    Mock::given(method("GET")).and(path("/x"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok":true})))
        .mount(&server).await;
    let v: Value = http(&server).get("/x").await.unwrap();
    assert_eq!(v["ok"], true);
}

#[tokio::test]
async fn queued_sends_keep_order_after_429() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/channels/1/messages"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0.05")
            .set_body_json(json!({"retry_after":0.05})))
        .up_to_n_times(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/channels/1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server).await;

    let h = http(&server);
    let (b1, b2, b3) = (json!({"content":"1"}), json!({"content":"2"}), json!({"content":"3"}));
    let (a, b, c) = tokio::join!(
        h.post::<Value>("/channels/1/messages", &b1),
        h.post::<Value>("/channels/1/messages", &b2),
        h.post::<Value>("/channels/1/messages", &b3),
    );
    a.unwrap(); b.unwrap(); c.unwrap();

    let sent: Vec<String> = server.received_requests().await.unwrap().iter()
        .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["content"].as_str().unwrap().to_string())
        .collect();
    // First attempt of "1" was rate limited, then retried before "2" and "3".
    assert_eq!(sent, ["1", "1", "2", "3"]);
}

#[tokio::test]
async fn unauthorized_maps() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(401)).mount(&server).await;
    assert!(matches!(http(&server).get::<Value>("/users/@me").await, Err(HttpError::Unauthorized)));
}

#[tokio::test]
async fn forbidden_maps() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(403)
        .set_body_json(json!({"message":"Missing Access","code":50001}))).mount(&server).await;
    match http(&server).get::<Value>("/channels/1/messages").await {
        Err(HttpError::Forbidden(m)) => assert_eq!(m, "Missing Access"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn server_errors_retry_three_times_then_fail() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(502)).expect(4).mount(&server).await;
    assert!(matches!(http(&server).get::<Value>("/x").await, Err(HttpError::Server(502))));
}

#[tokio::test]
async fn bad_request_carries_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(400)
        .set_body_json(json!({"captcha_key":["captcha-required"]}))).mount(&server).await;
    match http(&server).post::<Value>("/auth/login", &json!({})).await {
        Err(HttpError::BadRequest(v)) => assert_eq!(v["captcha_key"][0], "captcha-required"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn slow_response_times_out_as_network_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/slow"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})).set_delay(Duration::from_millis(500)))
        .mount(&server).await;
    let id = Identity { os: Os::Linux, locale: "en-US".into(), timezone: "UTC".into(), build_number: 1 };
    let h = Http::with_timeouts(id, &server.uri(), Duration::from_millis(100), Duration::from_millis(100));
    let r: Result<Value, HttpError> = h.get("/slow").await;
    assert!(matches!(r, Err(HttpError::Network(_))), "{r:?}");
}
