//! `client_build_number` goes in every super-properties blob. We read it from
//! the live web app, cache it on disk, and fall back to the last known value.

use std::path::Path;

use regex::Regex;

/// Last known value. Refresh with `cargo run -p cord-core --example build_number`.
pub const FALLBACK: u32 = 623968; // last refreshed 2026-09-30

pub fn parse(js: &str) -> Option<u32> {
    let re = Regex::new(r#"["']?(?:build_number|BUILD_NUMBER)["']?\s*[:,]\s*["'](\d{5,7})["']"#).unwrap();
    re.captures(js)?.get(1)?.as_str().parse().ok()
}

/// Reads the build number off the web app's page: its `GLOBAL_ENV` carries it,
/// so one request does it.
pub async fn fetch(client: &reqwest::Client) -> Option<u32> {
    let html = client.get("https://discord.com/app").send().await.ok()?.text().await.ok()?;
    parse(&html)
}

/// Live value if reachable, else the cached one, else [`FALLBACK`].
pub async fn resolve(client: &reqwest::Client, cache_file: &Path) -> u32 {
    if let Some(n) = fetch(client).await {
        let _ = std::fs::write(cache_file, n.to_string());
        return n;
    }
    std::fs::read_to_string(cache_file)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(FALLBACK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_build_number_in_bundle() {
        let js = r#"...,e.exports={releaseChannel:"stable",build_number:"431233",versionHash:"ab12"}..."#;
        assert_eq!(parse(js), Some(431233));
    }

    #[test]
    fn finds_quoted_key_form() {
        assert_eq!(parse(r#"{"build_number":"432001"}"#), Some(432001));
    }

    #[test]
    fn finds_it_in_the_page_itself() {
        assert_eq!(parse(r#"window.GLOBAL_ENV = {"NODE_ENV":"production","BUILD_NUMBER":"657850"}"#), Some(657850));
    }

    #[test]
    fn none_when_absent() {
        assert_eq!(parse("var a = 1;"), None);
    }
}
