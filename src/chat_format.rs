//! Small pure helpers for how the chat is laid out: day dividers.

use chrono::{NaiveDate, TimeZone};
use cord_core::Id;
use cord_core::model::Message;

fn day<Tz: TimeZone>(ms: u64, tz: &Tz) -> NaiveDate {
    tz.timestamp_millis_opt(ms as i64).single().unwrap_or_else(|| tz.timestamp_millis_opt(0).unwrap()).date_naive()
}

/// Whether a day divider goes above `cur`: it is the first message loaded,
/// or the previous one was written on a different local day. Days come from
/// the snowflake ids, so unsent messages work too.
pub fn starts_new_day<Tz: TimeZone>(prev: Option<&Message>, cur: &Message, tz: &Tz) -> bool {
    match prev {
        None => true,
        Some(prev) => day(prev.id.timestamp_ms(), tz) != day(cur.id.timestamp_ms(), tz),
    }
}

/// Whether `message` pings the signed-in user: named in `mentions`, an
/// @everyone/@here, or one of their roles.
pub fn mentions_me(message: &Message, me: Id, my_roles: &[Id]) -> bool {
    message.mention_everyone
        || message.mentions.iter().any(|u| u.id == me)
        || message.mention_roles.iter().any(|r| my_roles.contains(r))
}

/// "September 30, 2026", the date a divider shows.
pub fn day_label<Tz: TimeZone>(ms: u64, tz: &Tz) -> String {
    day(ms, tz).format("%B %-d, %Y").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn at(ms: u64) -> Message {
        serde_json::from_value(serde_json::json!({
            "id": Id::from_timestamp_ms(ms).to_string(), "channel_id": "2",
            "author": {"id": "3", "username": "u"}, "timestamp": "t"
        }))
        .unwrap()
    }

    // 2026-09-30 23:00 UTC and 2026-10-01 01:00 UTC.
    const LATE: u64 = 1_790_809_200_000;
    const EARLY: u64 = LATE + 2 * 3600 * 1000;

    #[test]
    fn divider_between_days_and_at_the_top() {
        let (a, b, c) = (at(LATE), at(EARLY), at(EARLY + 1000));
        assert!(starts_new_day(None, &a, &Utc));
        assert!(starts_new_day(Some(&a), &b, &Utc));
        assert!(!starts_new_day(Some(&b), &c, &Utc));
    }

    #[test]
    fn divider_follows_the_local_day() {
        let (a, b) = (at(LATE), at(EARLY));
        let plus_two = chrono::FixedOffset::east_opt(2 * 3600).unwrap();
        // 01:00 and 03:00 local: the same day there.
        assert!(!starts_new_day(Some(&a), &b, &plus_two));
    }

    #[test]
    fn mentions_of_me() {
        let mut m = at(LATE);
        assert!(!mentions_me(&m, Id(1), &[Id(7)]));
        m.mentions.push(cord_core::model::User { id: Id(1), username: "me".into(), global_name: None, avatar: None, bot: false, extras: Default::default() });
        assert!(mentions_me(&m, Id(1), &[]));
        assert!(!mentions_me(&m, Id(2), &[]));
        m.mentions.clear();
        m.mention_everyone = true;
        assert!(mentions_me(&m, Id(2), &[]));
        m.mention_everyone = false;
        m.mention_roles = vec![Id(7), Id(8)];
        assert!(mentions_me(&m, Id(2), &[Id(8)]));
        assert!(!mentions_me(&m, Id(2), &[Id(9)]));
    }

    #[test]
    fn labels_are_long_dates() {
        assert_eq!(day_label(LATE, &Utc), "September 30, 2026");
        assert_eq!(day_label(EARLY, &Utc), "October 1, 2026");
    }
}
