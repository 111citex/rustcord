//! The one client profile this app presents: Chrome on the host OS.
//! Every REST call and the gateway IDENTIFY use it, so they always agree.

use base64::Engine;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};

/// Bump alongside real Chrome releases.
pub const CHROME_VERSION: &str = "146.0.0.0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Linux,
    Windows,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(windows) { Os::Windows } else { Os::Linux }
    }
}

#[derive(Clone, Debug)]
pub struct Identity {
    pub os: Os,
    pub locale: String,
    pub timezone: String,
    pub build_number: u32,
}

impl Identity {
    pub fn new(build_number: u32) -> Identity {
        Identity {
            os: Os::current(),
            locale: system_locale(),
            timezone: system_timezone(),
            build_number,
        }
    }

    pub fn user_agent(&self) -> String {
        let platform = match self.os {
            Os::Linux => "X11; Linux x86_64",
            Os::Windows => "Windows NT 10.0; Win64; x64",
        };
        format!(
            "Mozilla/5.0 ({platform}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{CHROME_VERSION} Safari/537.36"
        )
    }

    pub fn super_properties(&self) -> Value {
        let (os, os_version) = match self.os {
            Os::Linux => ("Linux", ""),
            Os::Windows => ("Windows", "10"),
        };
        json!({
            "os": os,
            "browser": "Chrome",
            "device": "",
            "system_locale": self.locale,
            "has_client_mods": false,
            "browser_user_agent": self.user_agent(),
            "browser_version": CHROME_VERSION,
            "os_version": os_version,
            "referrer": "",
            "referring_domain": "",
            "referrer_current": "",
            "referring_domain_current": "",
            "release_channel": "stable",
            "client_build_number": self.build_number,
            "client_event_source": null,
        })
    }

    pub fn super_properties_b64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.super_properties().to_string())
    }

    pub fn rest_headers(&self) -> HeaderMap {
        let mut h = HeaderMap::new();
        let mut put = |k: &'static str, v: String| {
            h.insert(k, HeaderValue::from_str(&v).expect("header value is ASCII"));
        };
        put("user-agent", self.user_agent());
        put("x-super-properties", self.super_properties_b64());
        put("x-discord-locale", self.locale.clone());
        put("x-discord-timezone", self.timezone.clone());
        put("origin", "https://discord.com".into());
        put("referer", "https://discord.com/channels/@me".into());
        h
    }
}

fn system_locale() -> String {
    std::env::var("LANG")
        .ok()
        .and_then(|l| l.split('.').next().map(|s| s.replace('_', "-")))
        .filter(|l| !l.is_empty() && l != "C" && l != "POSIX")
        .unwrap_or_else(|| "en-US".into())
}

fn system_timezone() -> String {
    if let Ok(tz) = std::env::var("TZ") {
        if !tz.is_empty() {
            return tz.trim_start_matches(':').to_string();
        }
    }
    std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|p| {
            let s = p.to_string_lossy().to_string();
            s.split("zoneinfo/").nth(1).map(str::to_string)
        })
        .unwrap_or_else(|| "UTC".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn fixed(os: Os) -> Identity {
        Identity { os, locale: "en-US".into(), timezone: "Africa/Cairo".into(), build_number: 123456 }
    }

    #[test]
    fn linux_profile_is_chrome_on_linux() {
        let id = fixed(Os::Linux);
        let ua = id.user_agent();
        assert!(ua.contains("X11; Linux x86_64"), "{ua}");
        assert!(ua.contains(&format!("Chrome/{CHROME_VERSION}")), "{ua}");
        let p = id.super_properties();
        assert_eq!(p["os"], "Linux");
        assert_eq!(p["browser"], "Chrome");
        assert_eq!(p["browser_user_agent"], ua);
        assert_eq!(p["client_build_number"], 123456);
        assert_eq!(p["release_channel"], "stable");
    }

    #[test]
    fn windows_profile_is_chrome_on_windows() {
        let id = fixed(Os::Windows);
        assert!(id.user_agent().contains("Windows NT 10.0; Win64; x64"));
        assert_eq!(id.super_properties()["os"], "Windows");
        assert_eq!(id.super_properties()["os_version"], "10");
    }

    #[test]
    fn super_properties_b64_round_trips() {
        let id = fixed(Os::Linux);
        let raw = base64::engine::general_purpose::STANDARD
            .decode(id.super_properties_b64())
            .unwrap();
        let back: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(back, id.super_properties());
    }

    #[test]
    fn rest_headers_are_complete() {
        let h = fixed(Os::Linux).rest_headers();
        for name in ["user-agent", "x-super-properties", "x-discord-locale", "x-discord-timezone", "origin", "referer"] {
            assert!(h.contains_key(name), "missing {name}");
        }
        assert_eq!(h["origin"], "https://discord.com");
        assert!(!h.contains_key("authorization"));
    }
}
