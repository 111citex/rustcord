//! Email/password login, captcha retry and MFA. QR login is in `remote`.
//! Captchas are solved by the human in the app's captcha window.

pub mod remote;
pub mod token_store;

use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};

use crate::http::{Http, HttpError};

#[derive(Clone)]
pub struct CaptchaChallenge {
    pub sitekey: String,
    pub service: String,
    pub rqdata: Option<String>,
    pub rqtoken: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Clone)]
pub struct CaptchaSolution {
    pub key: String,
    pub rqtoken: Option<String>,
    pub session_id: Option<String>,
}

impl CaptchaSolution {
    pub fn for_challenge(c: &CaptchaChallenge, key: String) -> Self {
        CaptchaSolution { key, rqtoken: c.rqtoken.clone(), session_id: c.session_id.clone() }
    }
}

#[derive(Clone)]
pub struct MfaChallenge {
    pub ticket: String,
    pub totp: bool,
    pub backup: bool,
    pub sms: bool,
}

/// No `Debug`: a token must never end up in a log by accident.
pub enum LoginOutcome {
    Token(String),
    Mfa(MfaChallenge),
    Captcha(CaptchaChallenge),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MfaMethod {
    Totp,
    Backup,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("{0}")]
    Rejected(String),
    #[error(transparent)]
    Http(#[from] HttpError),
}

pub(crate) fn captcha_headers(captcha: Option<&CaptchaSolution>) -> HeaderMap {
    let mut h = HeaderMap::new();
    let Some(c) = captcha else { return h };
    let mut put = |name: &'static str, value: &str| {
        if let Ok(v) = HeaderValue::from_str(value) {
            h.insert(name, v);
        }
    };
    put("x-captcha-key", &c.key);
    if let Some(t) = &c.rqtoken {
        put("x-captcha-rqtoken", t);
    }
    if let Some(s) = &c.session_id {
        put("x-captcha-session-id", s);
    }
    h
}

pub(crate) fn parse_captcha(v: &Value) -> Option<CaptchaChallenge> {
    v.get("captcha_key")?;
    let s = |k: &str| v[k].as_str().map(str::to_string);
    Some(CaptchaChallenge {
        sitekey: s("captcha_sitekey")?,
        service: s("captcha_service").unwrap_or_else(|| "hcaptcha".into()),
        rqdata: s("captcha_rqdata"),
        rqtoken: s("captcha_rqtoken"),
        session_id: s("captcha_session_id"),
    })
}

/// Discord nests field errors as `errors.<field>._errors[].message`.
fn first_error_message(v: &Value) -> Option<String> {
    fn find(v: &Value) -> Option<String> {
        let map = v.as_object()?;
        if let Some(m) = map.get("_errors").and_then(|e| e[0]["message"].as_str()) {
            return Some(m.to_string());
        }
        map.values().find_map(find)
    }
    v.get("errors").and_then(find).or_else(|| v["message"].as_str().map(str::to_string))
}

pub async fn login(
    http: &Http,
    login: &str,
    password: &str,
    captcha: Option<&CaptchaSolution>,
) -> Result<LoginOutcome, AuthError> {
    let body = json!({
        "login": login,
        "password": password,
        "undelete": false,
        "login_source": null,
        "gift_code_sku_id": null,
    });
    match http.post_with_headers::<Value>("/auth/login", &body, captcha_headers(captcha)).await {
        Ok(v) => {
            if let Some(token) = v["token"].as_str() {
                return Ok(LoginOutcome::Token(token.to_string()));
            }
            if v["mfa"] == true {
                if let Some(ticket) = v["ticket"].as_str() {
                    return Ok(LoginOutcome::Mfa(MfaChallenge {
                        ticket: ticket.to_string(),
                        totp: v["totp"].as_bool().unwrap_or(false),
                        backup: v["backup"].as_bool().unwrap_or(true),
                        sms: v["sms"].as_bool().unwrap_or(false),
                    }));
                }
            }
            Err(AuthError::Rejected("Discord sent a login response this client doesn't understand.".into()))
        }
        Err(HttpError::BadRequest(v)) => match parse_captcha(&v) {
            Some(c) => Ok(LoginOutcome::Captcha(c)),
            None => Err(AuthError::Rejected(first_error_message(&v).unwrap_or_else(|| "Login failed.".into()))),
        },
        Err(e) => Err(e.into()),
    }
}

pub async fn submit_mfa(http: &Http, ticket: &str, method: MfaMethod, code: &str) -> Result<String, AuthError> {
    let path = match method {
        MfaMethod::Totp => "/auth/mfa/totp",
        MfaMethod::Backup => "/auth/mfa/backup",
    };
    let body = json!({"code": code, "ticket": ticket, "login_source": null, "gift_code_sku_id": null});
    match http.post::<Value>(path, &body).await {
        Ok(v) => v["token"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| AuthError::Rejected("Discord didn't return a session.".into())),
        Err(HttpError::BadRequest(v)) => {
            Err(AuthError::Rejected(first_error_message(&v).unwrap_or_else(|| "That code didn't work.".into())))
        }
        Err(e) => Err(e.into()),
    }
}
