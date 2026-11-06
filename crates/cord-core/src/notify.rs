//! Muting and notification settings for channels, and the small changes to a thread
//! its menu makes, in the shapes Discord's client sends them.

use serde_json::{Value, json};

use crate::Id;

/// Discord's mute lengths, in seconds; `None` is "until I turn it back on".
pub const MUTE_FOR: [(&str, Option<i64>); 6] = [
    ("For 15 Minutes", Some(900)),
    ("For 1 Hour", Some(3600)),
    ("For 3 Hours", Some(10_800)),
    ("For 8 Hours", Some(28_800)),
    ("For 24 Hours", Some(86_400)),
    ("Until I turn it back on", None),
];

/// Where our settings for `guild`'s channels are kept; `None` is our DMs.
pub fn settings_path(guild: Option<Id>) -> String {
    match guild {
        Some(guild) => format!("/users/@me/guilds/{guild}/settings"),
        None => "/users/@me/guilds/@me/settings".to_string(),
    }
}

/// A channel override that mutes for `seconds` from `now` (or until undone), or unmutes.
pub fn mute_body(mute: bool, seconds: Option<i64>, now: chrono::DateTime<chrono::Utc>) -> Value {
    if !mute {
        return json!({ "muted": false, "mute_config": null });
    }
    match seconds {
        Some(seconds) => {
            let end = (now + chrono::Duration::seconds(seconds)).format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
            json!({ "muted": true, "mute_config": { "selected_time_window": seconds, "end_time": end } })
        }
        None => json!({ "muted": true, "mute_config": { "selected_time_window": -1, "end_time": null } }),
    }
}

/// When a mute of `seconds` from `now_ms` ends, as the cache keeps it.
pub fn mute_ends(seconds: Option<i64>, now_ms: i64) -> Option<i64> {
    seconds.map(|s| now_ms + s * 1000)
}

/// What a thread's menu changes about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadChange {
    /// Close (archive) it, or open it again.
    Archived(bool),
    /// Lock it, so only moderators may open it again, or unlock it.
    Locked(bool),
}

impl ThreadChange {
    pub fn body(self) -> Value {
        match self {
            ThreadChange::Archived(on) => json!({ "archived": on }),
            ThreadChange::Locked(on) => json!({ "locked": on }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&chrono::Utc)
    }

    #[test]
    fn mutes_are_sent_as_discords_client_sends_them() {
        let now = at("2026-11-06T10:00:00Z");
        assert_eq!(mute_body(true, Some(3600), now), json!({ "muted": true, "mute_config": { "selected_time_window": 3600, "end_time": "2026-11-06T11:00:00.000Z" } }));
        assert_eq!(mute_body(true, None, now), json!({ "muted": true, "mute_config": { "selected_time_window": -1, "end_time": null } }));
        assert_eq!(mute_body(false, Some(3600), now), json!({ "muted": false, "mute_config": null }));
        assert_eq!(mute_ends(Some(900), 1_000), Some(901_000));
        assert_eq!(mute_ends(None, 1_000), None);
    }

    #[test]
    fn settings_are_per_server_and_dms_are_at_me() {
        assert_eq!(settings_path(Some(Id(5))), "/users/@me/guilds/5/settings");
        assert_eq!(settings_path(None), "/users/@me/guilds/@me/settings");
        assert_eq!(MUTE_FOR.len(), 6);
    }

    #[test]
    fn thread_changes_touch_one_flag() {
        assert_eq!(ThreadChange::Archived(true).body(), json!({ "archived": true }));
        assert_eq!(ThreadChange::Locked(false).body(), json!({ "locked": false }));
    }
}
