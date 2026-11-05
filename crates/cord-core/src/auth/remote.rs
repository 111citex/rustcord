//! QR login through Discord's remote-auth gateway. The phone scans a code
//! holding our public key's fingerprint; the token comes back encrypted to
//! our RSA key, so it never crosses the wire in the clear.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use rsa::pkcs8::EncodePublicKey;
use rsa::{Oaep, RsaPrivateKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, Interval, interval_at, sleep_until, timeout_at};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use crate::Id;
use crate::auth::{CaptchaChallenge, CaptchaSolution, captcha_headers, parse_captcha};
use crate::http::{Http, HttpError};

pub const REMOTE_AUTH_URL: &str = "wss://remote-auth-gateway.discord.gg/?v=2";

/// Connecting and receiving `hello` must finish within this.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// Slack on top of the server's `timeout_ms` before we give up on the session.
const GRACE: Duration = Duration::from_secs(2);
/// How long a human gets to solve a captcha on the ticket exchange.
const CAPTCHA_WAIT: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq)]
pub struct PendingUser {
    pub id: Id,
    pub username: String,
    pub avatar: Option<String>,
}

pub enum RemoteAuthEvent {
    Qr(String),
    Scanned(PendingUser),
    Token(String),
    Cancelled,
    Expired,
    Failed(String),
    /// Discord wants a captcha before handing over the token. Answer through
    /// [`RemoteAuth::captcha_sender`]: `Some(solution)`, or `None` if the
    /// user closed the captcha window.
    CaptchaRequired(CaptchaChallenge),
}

pub struct RemoteAuth {
    pub events: mpsc::UnboundedReceiver<RemoteAuthEvent>,
    captcha: mpsc::UnboundedSender<Option<CaptchaSolution>>,
    task: JoinHandle<()>,
}

impl RemoteAuth {
    /// Where to send the answer to a [`RemoteAuthEvent::CaptchaRequired`].
    pub fn captcha_sender(&self) -> mpsc::UnboundedSender<Option<CaptchaSolution>> {
        self.captcha.clone()
    }
}

impl Drop for RemoteAuth {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn start(url: &str, http: Http) -> RemoteAuth {
    spawn(url.to_string(), http, None, HANDSHAKE_TIMEOUT)
}

pub fn start_with_key(url: &str, http: Http, key: RsaPrivateKey) -> RemoteAuth {
    spawn(url.to_string(), http, Some(key), HANDSHAKE_TIMEOUT)
}

#[doc(hidden)]
pub fn start_with_key_and_timeout(url: &str, http: Http, key: RsaPrivateKey, handshake: Duration) -> RemoteAuth {
    spawn(url.to_string(), http, Some(key), handshake)
}

fn spawn(url: String, http: Http, key: Option<RsaPrivateKey>, handshake: Duration) -> RemoteAuth {
    let (tx, events) = mpsc::unbounded_channel();
    let (captcha, mut solutions) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let key = match key {
            Some(k) => k,
            None => match tokio::task::spawn_blocking(|| RsaPrivateKey::new(&mut rand::thread_rng(), 2048)).await {
                Ok(Ok(k)) => k,
                _ => {
                    let _ = tx.send(RemoteAuthEvent::Failed("Couldn't create a login key.".into()));
                    return;
                }
            },
        };
        if let Err(message) = run(&url, &http, &key, &tx, &mut solutions, handshake).await {
            let _ = tx.send(RemoteAuthEvent::Failed(message));
        }
    });
    RemoteAuth { events, captcha, task }
}

pub(crate) fn parse_user_payload(s: &str) -> Option<PendingUser> {
    let mut parts = s.splitn(4, ':');
    let id = Id(parts.next()?.parse().ok()?);
    let _discriminator = parts.next()?;
    let avatar = parts.next()?;
    let username = parts.next()?;
    Some(PendingUser {
        id,
        username: username.to_string(),
        avatar: (!avatar.is_empty() && avatar != "0").then(|| avatar.to_string()),
    })
}

fn decrypt(key: &RsaPrivateKey, b64: &Value) -> Result<Vec<u8>, String> {
    let raw = STANDARD.decode(b64.as_str().ok_or("missing ciphertext")?).map_err(|e| e.to_string())?;
    key.decrypt(Oaep::new::<Sha256>(), &raw).map_err(|e| e.to_string())
}

async fn tick(beat: &mut Option<Interval>) {
    match beat {
        Some(i) => {
            i.tick().await;
        }
        None => std::future::pending().await,
    }
}

/// Trades the approved ticket for the encrypted token, asking the user for a
/// captcha when Discord wants one.
async fn exchange_ticket(
    http: &Http,
    ticket: &str,
    tx: &mpsc::UnboundedSender<RemoteAuthEvent>,
    solutions: &mut mpsc::UnboundedReceiver<Option<CaptchaSolution>>,
) -> Result<Value, String> {
    let body = json!({"ticket": ticket});
    let mut solution: Option<CaptchaSolution> = None;
    loop {
        match http
            .post_with_headers::<Value>("/users/@me/remote-auth/login", &body, captcha_headers(solution.as_ref()))
            .await
        {
            Ok(resp) => return Ok(resp),
            // A rejected captcha also spends the ticket (Discord answers 404
            // "Invalid remote authentication ticket" after), so don't ask again.
            Err(HttpError::BadRequest(_)) if solution.is_some() => {
                return Err("Discord didn't accept the captcha. Get a new code and try again.".into());
            }
            Err(HttpError::BadRequest(v)) => {
                let challenge = parse_captcha(&v).ok_or("Discord refused the QR login.")?;
                let _ = tx.send(RemoteAuthEvent::CaptchaRequired(challenge));
                solution = match tokio::time::timeout(CAPTCHA_WAIT, solutions.recv()).await {
                    Ok(Some(Some(s))) => Some(s),
                    Ok(_) => return Err("The captcha was closed before it was solved.".into()),
                    Err(_) => return Err("The captcha wasn't solved in time.".into()),
                };
            }
            Err(e) => return Err(format!("Discord refused the QR login: {e}")),
        }
    }
}

async fn run(
    url: &str,
    http: &Http,
    key: &RsaPrivateKey,
    tx: &mpsc::UnboundedSender<RemoteAuthEvent>,
    solutions: &mut mpsc::UnboundedReceiver<Option<CaptchaSolution>>,
    handshake: Duration,
) -> Result<(), String> {
    let mut deadline = Instant::now() + handshake;
    let mut got_hello = false;
    let mut request = url.into_client_request().map_err(|e| e.to_string())?;
    let headers = request.headers_mut();
    headers.insert("origin", HeaderValue::from_static("https://discord.com"));
    if let Ok(ua) = HeaderValue::from_str(&http.identity().user_agent()) {
        headers.insert("user-agent", ua);
    }
    let (ws, _) = timeout_at(deadline, tokio_tungstenite::connect_async(request))
        .await
        .map_err(|_| "Discord didn't answer the QR login request.".to_string())?
        .map_err(|e| format!("Couldn't reach Discord for QR login: {e}"))?;
    let (mut sink, mut stream) = ws.split();
    let public = key.to_public_key().to_public_key_der().map_err(|e| e.to_string())?;
    let mut beat: Option<Interval> = None;

    macro_rules! send {
        ($v:expr) => {
            sink.send(Message::text($v.to_string())).await.map_err(|e| e.to_string())?
        };
    }

    loop {
        tokio::select! {
            _ = sleep_until(deadline) => {
                if got_hello {
                    let _ = tx.send(RemoteAuthEvent::Expired);
                    return Ok(());
                }
                return Err("Discord didn't answer the QR login request.".into());
            }
            _ = tick(&mut beat) => send!(json!({"op": "heartbeat"})),
            frame = stream.next() => {
                let text = match frame {
                    Some(Ok(Message::Text(t))) => t.to_string(),
                    Some(Ok(Message::Close(_))) | None => {
                        let _ = tx.send(RemoteAuthEvent::Expired);
                        return Ok(());
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return Err(e.to_string()),
                };
                let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
                match v["op"].as_str().unwrap_or_default() {
                    "hello" => {
                        let period = Duration::from_millis(v["heartbeat_interval"].as_u64().unwrap_or(41_250));
                        let session = Duration::from_millis(v["timeout_ms"].as_u64().unwrap_or(120_000));
                        deadline = Instant::now() + session + GRACE;
                        got_hello = true;
                        beat = Some(interval_at(Instant::now() + period, period));
                        send!(json!({"op": "init", "encoded_public_key": STANDARD.encode(public.as_bytes())}));
                    }
                    "nonce_proof" => {
                        let nonce = decrypt(key, &v["encrypted_nonce"])?;
                        send!(json!({"op": "nonce_proof", "proof": URL_SAFE_NO_PAD.encode(Sha256::digest(&nonce))}));
                    }
                    "pending_remote_init" => {
                        let fp = v["fingerprint"].as_str().ok_or("missing fingerprint")?;
                        let _ = tx.send(RemoteAuthEvent::Qr(format!("https://discord.com/ra/{fp}")));
                    }
                    "pending_ticket" => {
                        let raw = decrypt(key, &v["encrypted_user_payload"])?;
                        let user = parse_user_payload(&String::from_utf8_lossy(&raw)).ok_or("bad user payload")?;
                        let _ = tx.send(RemoteAuthEvent::Scanned(user));
                    }
                    "pending_login" => {
                        let ticket = v["ticket"].as_str().ok_or("missing ticket")?;
                        // The phone already approved: the session deadline no longer
                        // applies (REST requests have their own timeouts).
                        let resp = exchange_ticket(http, ticket, tx, solutions).await?;
                        let token = String::from_utf8(decrypt(key, &resp["encrypted_token"])?).map_err(|e| e.to_string())?;
                        let _ = tx.send(RemoteAuthEvent::Token(token));
                        let _ = sink.close().await;
                        return Ok(());
                    }
                    "cancel" => {
                        let _ = tx.send(RemoteAuthEvent::Cancelled);
                        return Ok(());
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
    fn parses_user_payload() {
        let u = parse_user_payload("100:0:abc123:ada").unwrap();
        assert_eq!(u, PendingUser { id: Id(100), username: "ada".into(), avatar: Some("abc123".into()) });
        let u = parse_user_payload("100:0::name").unwrap();
        assert_eq!(u.avatar, None);
        assert!(parse_user_payload("garbage").is_none());
    }
}
