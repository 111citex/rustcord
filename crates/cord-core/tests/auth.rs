use cord_core::auth::{self, AuthError, CaptchaSolution, LoginOutcome, MfaMethod};
use cord_core::http::Http;
use cord_core::identity::{Identity, Os};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn http(server: &MockServer) -> Http {
    let id = Identity { os: Os::Linux, locale: "en-US".into(), timezone: "UTC".into(), build_number: 1 };
    Http::new(id, &server.uri())
}

#[tokio::test]
async fn token_on_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/auth/login"))
        .and(body_partial_json(json!({"login": "a@b.c", "password": "pw", "undelete": false})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"user_id": "1", "token": "tok"})))
        .expect(1).mount(&server).await;
    match auth::login(&http(&server), "a@b.c", "pw", None).await {
        Ok(LoginOutcome::Token(t)) => assert_eq!(t, "tok"),
        _ => panic!("expected token"),
    }
}

#[tokio::test]
async fn mfa_challenge() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "user_id": "1", "token": null, "mfa": true, "ticket": "tkt", "totp": true, "backup": true, "sms": false
        }))).mount(&server).await;
    match auth::login(&http(&server), "a", "b", None).await {
        Ok(LoginOutcome::Mfa(m)) => {
            assert_eq!(m.ticket, "tkt");
            assert!(m.totp && m.backup && !m.sms);
        }
        _ => panic!("expected mfa"),
    }
}

#[tokio::test]
async fn captcha_challenge_then_retry_with_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/auth/login"))
        .and(header("x-captcha-key", "P1_solved"))
        .and(header("x-captcha-rqtoken", "rqt"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "tok"})))
        .mount(&server).await;
    Mock::given(method("POST")).and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "captcha_key": ["captcha-required"],
            "captcha_sitekey": "site-1",
            "captcha_service": "hcaptcha",
            "captcha_rqdata": "rqd",
            "captcha_rqtoken": "rqt",
            "captcha_session_id": "sid"
        }))).mount(&server).await;

    let h = http(&server);
    let Ok(LoginOutcome::Captcha(c)) = auth::login(&h, "a", "b", None).await else { panic!("expected captcha") };
    assert_eq!(c.sitekey, "site-1");
    assert_eq!(c.service, "hcaptcha");
    assert_eq!(c.rqdata.as_deref(), Some("rqd"));
    assert_eq!(c.session_id.as_deref(), Some("sid"));

    let solution = CaptchaSolution::for_challenge(&c, "P1_solved".into());
    let Ok(LoginOutcome::Token(t)) = auth::login(&h, "a", "b", Some(&solution)).await else { panic!("expected token") };
    assert_eq!(t, "tok");
}

#[tokio::test]
async fn invalid_login_shows_discords_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": 50035, "message": "Invalid Form Body",
            "errors": {"login": {"_errors": [{"code": "INVALID_LOGIN", "message": "Login or password is invalid."}]}}
        }))).mount(&server).await;
    match auth::login(&http(&server), "a", "b", None).await {
        Err(AuthError::Rejected(m)) => assert_eq!(m, "Login or password is invalid."),
        _ => panic!("expected rejection"),
    }
}

#[tokio::test]
async fn new_location_verification_message_is_passed_through() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": 50035,
            "errors": {"login": {"_errors": [{"code": "ACCOUNT_LOGIN_VERIFICATION_EMAIL", "message": "New login location detected, please check your e-mail."}]}}
        }))).mount(&server).await;
    match auth::login(&http(&server), "a", "b", None).await {
        Err(AuthError::Rejected(m)) => assert!(m.contains("New login location")),
        _ => panic!("expected rejection"),
    }
}

#[tokio::test]
async fn mfa_totp_returns_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/auth/mfa/totp"))
        .and(body_partial_json(json!({"code": "123456", "ticket": "tkt"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "tok"})))
        .expect(1).mount(&server).await;
    let t = auth::submit_mfa(&http(&server), "tkt", MfaMethod::Totp, "123456").await.unwrap();
    assert_eq!(t, "tok");
}

#[tokio::test]
async fn mfa_backup_uses_backup_endpoint_and_bad_code_is_rejected() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/auth/mfa/backup"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"message": "Invalid two-factor code", "code": 60008})))
        .expect(1).mount(&server).await;
    match auth::submit_mfa(&http(&server), "tkt", MfaMethod::Backup, "abcd-efgh").await {
        Err(AuthError::Rejected(m)) => assert_eq!(m, "Invalid two-factor code"),
        _ => panic!("expected rejection"),
    }
}
