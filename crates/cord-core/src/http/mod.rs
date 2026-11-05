//! Authenticated REST client. Every request carries the client identity,
//! waits its turn on the route queue, and retries 429 and 5xx.

pub mod ratelimit;

use std::sync::{Arc, RwLock};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::identity::Identity;
use ratelimit::{Limiter, route_key, secs};

/// A file to send with a message.
#[derive(Clone, Debug)]
pub struct Upload {
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
    /// How many bytes have gone out so far.
    pub sent: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("not found")]
    NotFound,
    #[error("bad request")]
    BadRequest(Value),
    #[error("server error {0}")]
    Server(u16),
    #[error("network: {0}")]
    Network(String),
    #[error("decode: {0}")]
    Decode(String),
}

const MAX_SERVER_RETRIES: u32 = 3;

#[derive(Clone)]
pub struct Http(Arc<Inner>);

struct Inner {
    client: reqwest::Client,
    base: String,
    identity: Identity,
    token: RwLock<Option<String>>,
    limiter: Limiter,
}

impl Http {
    pub fn new(identity: Identity, base: &str) -> Http {
        Http::with_timeouts(identity, base, Duration::from_secs(10), Duration::from_secs(30))
    }

    #[doc(hidden)]
    pub fn with_timeouts(identity: Identity, base: &str, connect: Duration, total: Duration) -> Http {
        let client = reqwest::Client::builder()
            .default_headers(identity.rest_headers())
            .connect_timeout(connect)
            .timeout(total)
            .build()
            .expect("reqwest client");
        Http(Arc::new(Inner {
            client,
            base: base.trim_end_matches('/').to_string(),
            identity,
            token: RwLock::new(None),
            limiter: Limiter::default(),
        }))
    }

    pub fn identity(&self) -> &Identity {
        &self.0.identity
    }

    pub fn set_token(&self, token: Option<String>) {
        *self.0.token.write().unwrap() = token;
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, HttpError> {
        self.send(Method::GET, path, None, HeaderMap::new()).await
    }

    pub async fn post<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T, HttpError> {
        self.send(Method::POST, path, Some(body), HeaderMap::new()).await
    }

    pub async fn put<T: DeserializeOwned>(&self, path: &str) -> Result<T, HttpError> {
        self.send(Method::PUT, path, None, HeaderMap::new()).await
    }

    pub async fn put_json<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T, HttpError> {
        self.send(Method::PUT, path, Some(body), HeaderMap::new()).await
    }

    pub async fn patch<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T, HttpError> {
        self.send(Method::PATCH, path, Some(body), HeaderMap::new()).await
    }

    pub async fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T, HttpError> {
        self.send(Method::DELETE, path, None, HeaderMap::new()).await
    }

    pub async fn post_with_headers<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
        extra: HeaderMap,
    ) -> Result<T, HttpError> {
        self.send(Method::POST, path, Some(body), extra).await
    }

    /// POSTs `body` with files alongside it, as Discord's multipart form.
    pub async fn post_files<T: DeserializeOwned>(&self, path: &str, body: &Value, files: &[Upload]) -> Result<T, HttpError> {
        self.send_with(Method::POST, path, Some(body), HeaderMap::new(), files).await
    }

    async fn send<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        extra: HeaderMap,
    ) -> Result<T, HttpError> {
        self.send_with(method, path, body, extra, &[]).await
    }

    async fn send_with<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        extra: HeaderMap,
        files: &[Upload],
    ) -> Result<T, HttpError> {
        let key = route_key(method.as_str(), path);
        let _turn = self.0.limiter.acquire(&key).await;
        let mut server_failures = 0;

        loop {
            let mut req = self
                .0
                .client
                .request(method.clone(), format!("{}{}", self.0.base, path))
                .headers(extra.clone());
            if let Some(token) = self.0.token.read().unwrap().clone() {
                let mut value = HeaderValue::from_str(&token).map_err(|e| HttpError::Network(e.to_string()))?;
                value.set_sensitive(true);
                req = req.header("authorization", value);
            }
            if files.is_empty() {
                if let Some(body) = body {
                    req = req.json(body);
                }
            } else {
                let mut payload = body.cloned().unwrap_or_else(|| Value::Object(Default::default()));
                payload["attachments"] = files
                    .iter()
                    .enumerate()
                    .map(|(i, f)| serde_json::json!({"id": i, "filename": f.name}))
                    .collect();
                let mut form = reqwest::multipart::Form::new().text("payload_json", payload.to_string());
                for (i, file) in files.iter().enumerate() {
                    // In pieces, counting them as they go.
                    const PIECE: usize = 64 * 1024;
                    let data: std::sync::Arc<[u8]> = file.bytes.as_slice().into();
                    let (length, sent) = (data.len(), file.sent.clone());
                    sent.store(0, std::sync::atomic::Ordering::Relaxed);
                    let pieces = futures_util::stream::iter((0..length).step_by(PIECE).map(move |at| {
                        let end = (at + PIECE).min(length);
                        sent.store(end as u64, std::sync::atomic::Ordering::Relaxed);
                        Ok::<Vec<u8>, std::io::Error>(data[at..end].to_vec())
                    }));
                    let part = reqwest::multipart::Part::stream_with_length(reqwest::Body::wrap_stream(pieces), length as u64)
                        .file_name(file.name.clone())
                        .mime_str(&file.mime)
                        .map_err(|e| HttpError::Network(e.to_string()))?;
                    form = form.part(format!("files[{i}]"), part);
                }
                req = req.multipart(form);
            }

            let resp = req.send().await.map_err(|e| HttpError::Network(e.to_string()))?;
            let status = resp.status();
            self.0.limiter.update(&key, resp.headers());

            if status == StatusCode::TOO_MANY_REQUESTS {
                let header_secs = resp.headers().get("retry-after")
                    .and_then(|v| secs(v.to_str().ok()?.parse::<f64>().ok()?));
                let global = resp.headers().contains_key("x-ratelimit-global");
                let body: Value = resp.json().await.unwrap_or(Value::Null);
                let wait = header_secs
                    .or_else(|| body["retry_after"].as_f64().and_then(secs))
                    .unwrap_or(Duration::from_secs(1));
                tracing::warn!(route = %key, secs = wait.as_secs_f64(), global, "rate limited");
                if global || body["global"] == true {
                    self.0.limiter.block_global(wait);
                }
                tokio::time::sleep(wait).await;
                continue;
            }

            if status.is_server_error() {
                server_failures += 1;
                if server_failures > MAX_SERVER_RETRIES {
                    return Err(HttpError::Server(status.as_u16()));
                }
                tokio::time::sleep(Duration::from_millis(250 * 2u64.pow(server_failures - 1))).await;
                continue;
            }

            return match status {
                s if s.is_success() => {
                    let bytes = resp.bytes().await.map_err(|e| HttpError::Network(e.to_string()))?;
                    // 204 No Content decodes as null.
                    let slice: &[u8] = if bytes.is_empty() { b"null" } else { &bytes };
                    serde_json::from_slice(slice).map_err(|e| HttpError::Decode(e.to_string()))
                }
                StatusCode::UNAUTHORIZED => Err(HttpError::Unauthorized),
                StatusCode::FORBIDDEN => {
                    let v: Value = resp.json().await.unwrap_or(Value::Null);
                    Err(HttpError::Forbidden(v["message"].as_str().unwrap_or("Forbidden").to_string()))
                }
                StatusCode::NOT_FOUND => Err(HttpError::NotFound),
                _ => Err(HttpError::BadRequest(resp.json().await.unwrap_or(Value::Null))),
            };
        }
    }
}
