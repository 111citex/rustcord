//! Per-route request queues and bucket bookkeeping. Requests on the same route
//! run one at a time, in the order they were made, so a 429 never reorders
//! messages. Buckets are tracked per route key (a simplification of Discord's
//! shared bucket hashes that errs on the side of waiting).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::header::HeaderMap;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

pub fn route_key(method: &str, path: &str) -> String {
    let path = path.split('?').next().unwrap_or(path);
    let segs: Vec<&str> = path.split('/').collect();
    let mut out = Vec::with_capacity(segs.len());
    for (i, seg) in segs.iter().enumerate() {
        let is_id = !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit());
        let major = i > 0 && matches!(segs[i - 1], "channels" | "guilds" | "webhooks");
        out.push(if is_id && !major { ":id" } else { seg });
    }
    format!("{method} {}", out.join("/"))
}

/// Converts a header/body seconds value to a wait, rejecting NaN, infinite and
/// negative values (`None`) and clamping to one hour.
pub fn secs(v: f64) -> Option<Duration> {
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(v).ok().map(|d| d.min(Duration::from_secs(3600)))
}

#[derive(Default)]
pub struct Limiter {
    queues: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    blocked_until: Mutex<HashMap<String, Instant>>,
    global_until: Mutex<Option<Instant>>,
}

impl Limiter {
    /// Waits for this route's turn and for any bucket or global reset.
    pub async fn acquire(&self, key: &str) -> OwnedMutexGuard<()> {
        let queue = {
            let mut q = self.queues.lock().unwrap();
            q.entry(key.to_string()).or_default().clone()
        };
        let guard = queue.lock_owned().await;
        loop {
            let wait = {
                let now = Instant::now();
                let bucket = self.blocked_until.lock().unwrap().get(key).copied();
                let global = *self.global_until.lock().unwrap();
                [bucket, global].into_iter().flatten().filter(|t| *t > now).max().map(|t| t - now)
            };
            match wait {
                Some(d) => tokio::time::sleep(d).await,
                None => return guard,
            }
        }
    }

    /// Records `X-RateLimit-*` headers after a response.
    pub fn update(&self, key: &str, headers: &HeaderMap) {
        let num = |name: &str| headers.get(name)?.to_str().ok()?.parse::<f64>().ok();
        if num("x-ratelimit-remaining") == Some(0.0) {
            if let Some(after) = num("x-ratelimit-reset-after").and_then(secs) {
                self.block(key, after);
            }
        }
    }

    pub fn block(&self, key: &str, d: Duration) {
        self.blocked_until.lock().unwrap().insert(key.to_string(), Instant::now() + d);
    }

    pub fn block_global(&self, d: Duration) {
        *self.global_until.lock().unwrap() = Some(Instant::now() + d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn major_params_stay_minor_ids_collapse() {
        assert_eq!(route_key("GET", "/channels/111/messages"), "GET /channels/111/messages");
        assert_eq!(route_key("DELETE", "/channels/111/messages/222"), "DELETE /channels/111/messages/:id");
        assert_eq!(route_key("GET", "/guilds/9/members/5"), "GET /guilds/9/members/:id");
        assert_eq!(route_key("GET", "/users/@me"), "GET /users/@me");
    }

    #[test]
    fn secs_rejects_bad_values_and_clamps() {
        assert_eq!(secs(f64::NAN), None);
        assert_eq!(secs(f64::INFINITY), None);
        assert_eq!(secs(-1.0), None);
        assert_eq!(secs(0.5), Some(Duration::from_millis(500)));
        assert_eq!(secs(1e12), Some(Duration::from_secs(3600)));
    }

    #[test]
    fn query_string_is_ignored() {
        assert_eq!(route_key("GET", "/channels/1/messages?limit=50"), "GET /channels/1/messages");
    }
}
