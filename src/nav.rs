//! Pure helpers that turn the cache into what the sidebar and chat show.

use std::collections::HashMap;
use std::ops::Range;

use chrono::{DateTime, TimeZone};
use cord_core::Id;
use cord_core::cache::Cache;
use cord_core::model::{Channel, ChannelKind, Message};

pub const MAX_MESSAGE_CHARS: usize = 2000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Location {
    pub guild: Option<Id>,
    pub channel: Option<Id>,
}

pub fn parse_location(path: &str) -> Option<Location> {
    let mut parts = path.trim_start_matches('/').split('/');
    if parts.next()? != "channels" {
        return None;
    }
    let guild = match parts.next()? {
        "@me" => None,
        id => Some(Id(id.parse().ok()?)),
    };
    let channel = match parts.next() {
        Some(id) => Some(Id(id.parse().ok()?)),
        None => None,
    };
    Some(Location { guild, channel })
}

pub struct ChannelGroup<'a> {
    pub category: Option<&'a Channel>,
    pub channels: Vec<&'a Channel>,
}

fn voice_rank(kind: ChannelKind) -> u8 {
    matches!(kind, ChannelKind::Voice | ChannelKind::Stage) as u8
}

/// The sidebar's groups for `guild`, leaving out what the user can't view.
pub fn channel_groups(cache: &Cache, guild: Id) -> Vec<ChannelGroup<'_>> {
    let all = cache.guild_channels(guild);
    let mut categories: Vec<&Channel> = all.iter().copied().filter(|c| c.kind == ChannelKind::Category).collect();
    categories.sort_by_key(|c| (c.position, c.id));

    let children = |parent: Option<Id>| {
        let mut list: Vec<&Channel> = all
            .iter()
            .copied()
            .filter(|c| c.kind != ChannelKind::Category && c.parent_id == parent && cache.can_view(c.id))
            .collect();
        list.sort_by_key(|c| (voice_rank(c.kind), c.position, c.id));
        list
    };

    let mut groups = Vec::new();
    let loose = children(None);
    if !loose.is_empty() {
        groups.push(ChannelGroup { category: None, channels: loose });
    }
    for category in categories {
        let channels = children(Some(category.id));
        // A category you can't see stays hidden unless it holds something you can.
        if channels.is_empty() && !cache.can_view(category.id) {
            continue;
        }
        groups.push(ChannelGroup { category: Some(category), channels });
    }
    groups
}

pub fn first_text_channel(cache: &Cache, guild: Id) -> Option<Id> {
    channel_groups(cache, guild)
        .into_iter()
        .flat_map(|g| g.channels)
        .find(|c| c.kind.is_text_like())
        .map(|c| c.id)
}

pub fn channel_label(cache: &Cache, channel: &Channel) -> String {
    if let Some(name) = channel.name.as_deref().filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    let names: Vec<&str> = channel
        .recipient_ids
        .iter()
        .filter_map(|id| cache.user(*id))
        .map(|u| cache.display_name(None, u))
        .collect();
    if names.is_empty() { "Unnamed".to_string() } else { names.join(", ") }
}

pub fn channel_path(channel: &Channel) -> String {
    match channel.guild_id {
        Some(guild) => format!("/channels/{guild}/{}", channel.id),
        None => format!("/channels/@me/{}", channel.id),
    }
}

/// The `ListState::splice` that turns the rendered ids into the current ones:
/// keep the common prefix and suffix, replace what's between. Keeping the
/// suffix matters when older messages load above: the list then keeps its
/// place instead of jumping.
pub fn list_splice(old: &[Id], new: &[Id]) -> Option<(Range<usize>, usize)> {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    if prefix == old.len() && prefix == new.len() {
        return None;
    }
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    Some((prefix..old.len() - suffix, new.len() - prefix - suffix))
}

/// How many rows left the top if the only other change is rows added at the end: the
/// oldest message making room for a new one. Rows are counted from `first`, past the
/// row that's always at the top. One change covering both ends would rebuild the
/// list, and lose the place of someone reading further up.
pub fn trimmed_from_top(old: &[Id], new: &[Id], first: usize) -> Option<usize> {
    let oldest = *new.get(first)?;
    let at = old.iter().position(|id| *id == oldest)?;
    let kept = old.len() - at;
    (at > first && new.len() > first + kept && old[at..] == new[first..first + kept]).then_some(at - first)
}

/// A number that differs for every state of the top-of-channel row, which
/// has a different height for each.
pub fn top_row_state(loading_older: bool, has_older: bool, scrollback_full: bool) -> u64 {
    u64::from(loading_older) | u64::from(has_older) << 1 | u64::from(scrollback_full) << 2
}

/// Indexes into `new` of the rows that were rendered before (in `old`, id →
/// revision) with a different revision: their content, and so their height,
/// may have changed. New rows aren't listed; the list measures them anyway.
pub fn edited_rows(old: &HashMap<Id, u64>, new: &[(Id, u64)]) -> Vec<usize> {
    new.iter()
        .enumerate()
        .filter(|(_, (id, rev))| old.get(id).is_some_and(|before| before != rev))
        .map(|(ix, _)| ix)
        .collect()
}

/// The row just after a splice. Whether a message stacks under the previous
/// one depends on that previous message, so when rows are inserted, replaced
/// or removed before it, its shape (and height) can change: measure it again.
pub fn row_after_splice(range: &Range<usize>, count: usize, new_len: usize) -> Option<usize> {
    let next = range.start + count;
    (next < new_len).then_some(next)
}

pub fn prepare_message(text: &str) -> Result<String, &'static str> {
    let text = text.trim();
    if text.is_empty() {
        return Err("");
    }
    if text.chars().count() > MAX_MESSAGE_CHARS {
        return Err("Messages can be at most 2,000 characters.");
    }
    Ok(text.to_string())
}

/// Discord stacks a message under the previous one when the same person sent
/// it less than this long after, on the same day.
const GROUP_WINDOW_MS: u64 = 7 * 60 * 1000;

fn local<Tz: TimeZone>(ms: u64, tz: &Tz) -> DateTime<Tz> {
    tz.timestamp_millis_opt(ms as i64).single().unwrap_or_else(|| tz.timestamp_millis_opt(0).unwrap())
}

/// Whether `cur` stacks under `prev` (no avatar or name of its own). Times
/// come from the snowflake ids, so unsent messages group too.
pub fn continues_group<Tz: TimeZone>(prev: Option<&Message>, cur: &Message, tz: &Tz) -> bool {
    let Some(prev) = prev else { return false };
    let (a, b) = (prev.id.timestamp_ms(), cur.id.timestamp_ms());
    prev.author.id == cur.author.id
        && b >= a
        && b - a < GROUP_WINDOW_MS
        && local(a, tz).date_naive() == local(b, tz).date_naive()
}

/// Discord's message time: "4:25 AM" for today, "Yesterday at 8:41 PM", "9/28/26, 8:41 PM" before.
pub fn format_message_time<Tz: TimeZone>(ms: u64, now: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let at = local(ms, &now.timezone());
    let time = at.format("%-I:%M %p");
    if at.date_naive() == now.date_naive() {
        time.to_string()
    } else {
        let today = now.with_timezone(&now.timezone()).date_naive();
        if today.pred_opt() == Some(at.date_naive()) {
            format!("Yesterday at {time}")
        } else {
            format!("{}, {time}", at.format("%-m/%-d/%y"))
        }
    }
}

/// Why the user can't write in `channel`, if they can't: a timeout (with its
/// end, as Discord shows it) or a missing Send Messages permission.
pub fn composer_lock(cache: &Cache, channel: &Channel) -> Option<String> {
    if let Some(until) = channel.guild_id.and_then(|g| cache.timed_out_until(g)) {
        return Some(timeout_notice(&until, &chrono::Local::now()));
    }
    (!cache.can_send(channel.id)).then(|| "You do not have permission to send messages in this channel.".to_string())
}

/// "You're timed out until Today at 21:30." for an ISO 8601 `until`.
fn timeout_notice<Tz: TimeZone>(until: &str, now: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    match DateTime::parse_from_rfc3339(until) {
        Ok(end) => format!("You're timed out until {}.", format_message_time(end.timestamp_millis().max(0) as u64, now)),
        Err(_) => "You're timed out.".to_string(),
    }
}

/// "Sunday, November 1, 2026 at 9:08 PM": when `message` was last edited.
pub fn edited_at<Tz: TimeZone>(message: &cord_core::model::Message, tz: &Tz) -> Option<String>
where
    Tz::Offset: std::fmt::Display,
{
    let at = DateTime::parse_from_rfc3339(message.edited_timestamp.as_deref()?).ok()?;
    Some(at.with_timezone(tz).format("%A, %B %-d, %Y at %-I:%M %p").to_string())
}

/// "15:31", shown beside stacked messages on hover.
pub fn short_time<Tz: TimeZone>(ms: u64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    local(ms, tz).format("%H:%M").to_string()
}

/// "5s", "3m 05s", "3h 02m" or "2d 1h", for the info panel.
pub fn format_uptime(uptime: std::time::Duration) -> String {
    let s = uptime.as_secs();
    let (d, h, m, s) = (s / 86_400, s / 3600 % 24, s / 60 % 60, s % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, _) => format!("{m}m {s:02}s"),
        (0, _, _) => format!("{h}h {m:02}m"),
        _ => format!("{d}d {h}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trim_at_the_top_with_a_new_message_is_two_changes() {
        let (top, a, b, c, d) = (Id(0), Id(1), Id(2), Id(3), Id(4));
        assert_eq!(trimmed_from_top(&[top, a, b, c], &[top, b, c, d], 1), Some(1));
        assert_eq!(trimmed_from_top(&[top, a, b, c], &[top, a, b, c, d], 1), None, "only added");
        assert_eq!(trimmed_from_top(&[top, a, b, c], &[top, b, c], 1), None, "only removed");
        assert_eq!(trimmed_from_top(&[top, a, b, c], &[top, b, d], 1), None, "something else changed too");
    }
    use cord_core::event::CoreEvent;
    use cord_core::model::{ChannelKind, Guild, Ready, User};

    fn user(id: u64, name: &str, global: Option<&str>) -> User {
        User { id: Id(id), username: name.into(), global_name: global.map(Into::into), avatar: None, bot: false, extras: Default::default() }
    }

    fn ch(id: u64, kind: ChannelKind, name: &str, position: i32, parent: Option<u64>) -> Channel {
        Channel {
            id: Id(id), kind, guild_id: Some(Id(300)), name: Some(name.into()), position,
            parent_id: parent.map(Id), last_message_id: None, recipient_ids: vec![], recipients: vec![], icon: None, permission_overwrites: vec![], topic: None,
        }
    }

    fn dm(id: u64, kind: ChannelKind, name: Option<&str>, recipients: &[u64]) -> Channel {
        Channel {
            id: Id(id), kind, guild_id: None, name: name.map(Into::into), position: 0, parent_id: None,
            last_message_id: None, recipient_ids: recipients.iter().copied().map(Id).collect(), recipients: vec![], icon: None,
            permission_overwrites: vec![], topic: None,
        }
    }

    fn cache() -> Cache {
        let mut c = Cache::default();
        c.apply(CoreEvent::Ready(Box::new(Ready {
            user: user(1, "me", None),
            session_id: "s".into(),
            resume_gateway_url: "u".into(), read_state: Default::default(), user_settings_proto: None, relationships: vec![], sessions: vec![], user_guild_settings: Default::default(),
            guilds: vec![Guild {
                id: Id(300), name: "g".into(), icon: None, unavailable: false,
                channels: vec![
                    ch(10, ChannelKind::Category, "Voice", 1, None),
                    ch(11, ChannelKind::Category, "Text", 0, None),
                    ch(20, ChannelKind::Voice, "lounge", 0, Some(11)),
                    ch(21, ChannelKind::Text, "random", 2, Some(11)),
                    ch(22, ChannelKind::Text, "general", 1, Some(11)),
                    ch(23, ChannelKind::Text, "rules", 5, None),
                    ch(24, ChannelKind::Voice, "afk", 0, Some(10)),
                ],
                roles: vec![],
                owner_id: None, voice_states: vec![], emojis: vec![], premium_tier: 0, banner: None, rules_channel: None,
            }],
            private_channels: vec![
                dm(400, ChannelKind::Dm, None, &[2]),
                dm(401, ChannelKind::GroupDm, None, &[2, 3]),
                dm(402, ChannelKind::GroupDm, Some("squad"), &[2, 3]),
            ],
            users: vec![user(2, "lin", Some("Lin")), user(3, "kai", None)],
            merged_members: vec![],
        })));
        c
    }

    #[test]
    fn parses_locations() {
        assert_eq!(parse_location("/channels/@me"), Some(Location { guild: None, channel: None }));
        assert_eq!(parse_location("/channels/@me/400"), Some(Location { guild: None, channel: Some(Id(400)) }));
        assert_eq!(parse_location("/channels/300/22"), Some(Location { guild: Some(Id(300)), channel: Some(Id(22)) }));
        assert_eq!(parse_location("/channels/300"), Some(Location { guild: Some(Id(300)), channel: None }));
        assert_eq!(parse_location("/login"), None);
        assert_eq!(parse_location("/channels/abc/1"), None);
    }

    #[test]
    fn groups_follow_discord_order() {
        let c = cache();
        let groups = channel_groups(&c, Id(300));
        let shape: Vec<(Option<&str>, Vec<&str>)> = groups
            .iter()
            .map(|g| {
                (g.category.and_then(|c| c.name.as_deref()), g.channels.iter().map(|c| c.name.as_deref().unwrap()).collect())
            })
            .collect();
        assert_eq!(
            shape,
            [
                (None, vec!["rules"]),
                (Some("Text"), vec!["general", "random", "lounge"]),
                (Some("Voice"), vec!["afk"]),
            ]
        );
        assert_eq!(first_text_channel(&c, Id(300)), Some(Id(23)));
    }

    #[test]
    fn unviewable_channels_and_empty_categories_are_hidden() {
        use cord_core::model::{Member, MemberList, OverwriteKind, PermissionOverwrite, Role};
        use cord_core::permissions;
        let hidden = |mut c: Channel| {
            c.permission_overwrites =
                vec![PermissionOverwrite { id: Id(300), kind: OverwriteKind::Role, allow: 0, deny: permissions::VIEW_CHANNEL }];
            c
        };
        let mut c = Cache::default();
        c.apply(CoreEvent::Ready(Box::new(Ready {
            user: user(1, "me", None),
            session_id: "s".into(),
            resume_gateway_url: "u".into(), read_state: Default::default(), user_settings_proto: None, relationships: vec![], sessions: vec![], user_guild_settings: Default::default(),
            guilds: vec![Guild {
                id: Id(300), name: "g".into(), icon: None, unavailable: false,
                channels: vec![
                    ch(11, ChannelKind::Category, "Text", 0, None),
                    hidden(ch(10, ChannelKind::Category, "Staff", 1, None)),
                    hidden(ch(21, ChannelKind::Text, "mods", 0, Some(10))),
                    hidden(ch(22, ChannelKind::Text, "secret", 0, Some(11))),
                    ch(23, ChannelKind::Text, "general", 1, Some(11)),
                    // Hidden category, but a child opened up to us: stays.
                    hidden(ch(12, ChannelKind::Category, "Partly", 2, None)),
                    ch(24, ChannelKind::Text, "open", 0, Some(12)),
                ],
                roles: vec![Role {
                    id: Id(300), name: String::new(), color: 0, position: 0,
                    permissions: permissions::VIEW_CHANNEL, hoist: false, icon: None, unicode_emoji: None, colors: None, mentionable: false,
                }],
                owner_id: None, voice_states: vec![], emojis: vec![], premium_tier: 0, banner: None, rules_channel: None,
            }],
            private_channels: vec![],
            users: vec![],
            merged_members: vec![MemberList(vec![Member {
                user_id: Some(Id(1)), user: None, nick: None, avatar: None, roles: vec![],
                communication_disabled_until: None, joined_at: None,
            }])],
        })));
        let shape: Vec<(Option<&str>, Vec<&str>)> = channel_groups(&c, Id(300))
            .iter()
            .map(|g| {
                (g.category.and_then(|c| c.name.as_deref()), g.channels.iter().map(|c| c.name.as_deref().unwrap()).collect())
            })
            .collect();
        assert_eq!(shape, [(Some("Text"), vec!["general"]), (Some("Partly"), vec!["open"])]);
        assert_eq!(first_text_channel(&c, Id(300)), Some(Id(23)));
    }

    #[test]
    fn labels() {
        let c = cache();
        assert_eq!(channel_label(&c, c.channel(Id(22)).unwrap()), "general");
        assert_eq!(channel_label(&c, c.channel(Id(400)).unwrap()), "Lin");
        assert_eq!(channel_label(&c, c.channel(Id(401)).unwrap()), "Lin, kai");
        assert_eq!(channel_label(&c, c.channel(Id(402)).unwrap()), "squad");
        assert_eq!(channel_path(c.channel(Id(22)).unwrap()), "/channels/300/22");
        assert_eq!(channel_path(c.channel(Id(400)).unwrap()), "/channels/@me/400");
    }

    #[test]
    fn list_splices() {
        let ids = |v: &[u64]| v.iter().copied().map(Id).collect::<Vec<_>>();
        assert_eq!(list_splice(&ids(&[1, 2]), &ids(&[1, 2])), None);
        assert_eq!(list_splice(&ids(&[1, 2]), &ids(&[1, 2, 3])), Some((2..2, 1)));
        // A pending message replaced by its echo.
        assert_eq!(list_splice(&ids(&[1, 2, 99]), &ids(&[1, 2, 3])), Some((2..3, 1)));
        // Ring buffer dropped the oldest.
        assert_eq!(list_splice(&ids(&[1, 2]), &ids(&[2, 3])), Some((0..2, 2)));
        assert_eq!(list_splice(&[], &ids(&[1])), Some((0..0, 1)));
        // Older messages loaded above: only the new ones are inserted, so the
        // position of what's on screen is kept.
        assert_eq!(list_splice(&ids(&[3, 4]), &ids(&[1, 2, 3, 4])), Some((0..0, 2)));
        // A message deleted in the middle.
        assert_eq!(list_splice(&ids(&[1, 2, 3]), &ids(&[1, 3])), Some((1..2, 0)));
    }

    #[test]
    fn edited_rows_are_the_ones_whose_revision_changed() {
        let old: HashMap<Id, u64> = [(Id(1), 10), (Id(2), 11), (Id(3), 12)].into_iter().collect();
        // Nothing changed: a presence tick or an unrelated notify touches no row.
        assert!(edited_rows(&old, &[(Id(1), 10), (Id(2), 11), (Id(3), 12)]).is_empty());
        // Row 2 edited; row 4 is new (the list measures it anyway); row 1 was dropped.
        assert_eq!(edited_rows(&old, &[(Id(2), 99), (Id(3), 12), (Id(4), 13)]), [0]);
    }

    #[test]
    fn top_row_states_differ() {
        let states: std::collections::HashSet<u64> = (0..8)
            .map(|n| top_row_state(n & 1 != 0, n & 2 != 0, n & 4 != 0))
            .collect();
        assert_eq!(states.len(), 8);
    }

    #[test]
    fn the_row_after_a_splice_is_measured_again() {
        // Older messages inserted above row 1: the old first message (now at 3)
        // may become a stacked follow-up.
        assert_eq!(row_after_splice(&(1..1), 2, 10), Some(3));
        // A pending message replaced by its echo at the very end: nothing after it.
        assert_eq!(row_after_splice(&(9..10), 1, 10), None);
        // A message deleted from the middle: the one that moved up into its place.
        assert_eq!(row_after_splice(&(4..5), 0, 8), Some(4));
    }

    #[test]
    fn prepares_messages() {
        assert_eq!(prepare_message("  hi \n"), Ok("hi".to_string()));
        assert_eq!(prepare_message("   "), Err(""));
        assert!(prepare_message(&"é".repeat(2000)).is_ok());
        assert!(prepare_message(&"é".repeat(2001)).unwrap_err().contains("2,000"));
    }
}

#[cfg(test)]
mod chat_tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone};
    use cord_core::model::{Message, User};

    const MIN: u64 = 60_000;
    /// 2026-10-01 12:00:00 UTC.
    const NOON: u64 = 1_790_856_000_000;

    #[test]
    fn edited_time_reads_in_full_and_in_our_time_zone() {
        let mut message = msg(1, NOON);
        assert_eq!(edited_at(&message, &FixedOffset::east_opt(0).unwrap()), None);
        message.edited_timestamp = Some("2026-11-01T19:08:00.123000+00:00".into());
        let cairo = FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(edited_at(&message, &cairo).as_deref(), Some("Sunday, November 1, 2026 at 9:08 PM"));
    }

    fn msg(author: u64, ms: u64) -> Message {
        Message {
            pinned: false,
            id: Id::from_timestamp_ms(ms),
            channel_id: Id(1),
            guild_id: None,
            author: User { id: Id(author), username: format!("u{author}"), global_name: None, avatar: None, bot: false, extras: Default::default() },
            content: String::new(),
            timestamp: String::new(),
            edited_timestamp: None, poll: None, interaction: None, interaction_metadata: None, message_snapshots: Vec::new(),
            nonce: None,
            attachments: vec![],
            member: None,
            kind: 0,
            flags: 0,
            embeds: vec![],
            components: vec![],
            message_reference: None,
            referenced_message: None,
            sticker_items: vec![],
            reactions: vec![], mention_everyone: false, mention_roles: vec![],
            mentions: vec![],
        }
    }

    #[test]
    fn same_author_within_seven_minutes_stacks() {
        let tz = FixedOffset::east_opt(0).unwrap();
        let first = msg(1, NOON);
        assert!(!continues_group(None, &first, &tz));
        assert!(continues_group(Some(&first), &msg(1, NOON + 6 * MIN), &tz));
        assert!(!continues_group(Some(&first), &msg(1, NOON + 7 * MIN), &tz));
        assert!(!continues_group(Some(&first), &msg(2, NOON + MIN), &tz));
    }

    #[test]
    fn a_new_day_starts_a_new_group() {
        // 23:58 and 00:01 local time (UTC+3) are on different days.
        let tz = FixedOffset::east_opt(3 * 3600).unwrap();
        let late = NOON + (8 * 60 + 58) * MIN; // 20:58 UTC = 23:58 local
        assert!(!continues_group(Some(&msg(1, late)), &msg(1, late + 3 * MIN), &tz));
    }

    #[test]
    fn time_labels() {
        let tz = FixedOffset::east_opt(0).unwrap();
        let now = tz.timestamp_millis_opt((NOON + 60 * MIN) as i64).unwrap();
        assert_eq!(format_message_time(NOON, &now), "12:00 PM");
        assert_eq!(format_message_time(NOON - 15 * 60 * MIN, &now), "Yesterday at 9:00 PM");
        assert_eq!(format_message_time(NOON - 3 * 24 * 60 * MIN, &now), "9/28/26, 12:00 PM");
        assert_eq!(format_message_time(NOON - 11 * 60 * MIN - 35 * MIN, &now), "12:25 AM");
        assert_eq!(short_time(NOON + 5 * MIN, &tz), "12:05");
    }

    #[test]
    fn timeout_notices() {
        let tz = FixedOffset::east_opt(0).unwrap();
        let now = tz.timestamp_millis_opt((NOON + 60 * MIN) as i64).unwrap();
        // NOON is 2026-10-01 12:00 UTC.
        assert_eq!(timeout_notice("2026-10-01T21:30:00.000000+00:00", &now), "You're timed out until 9:30 PM.");
        assert_eq!(timeout_notice("2026-10-02T09:00:00+00:00", &now), "You're timed out until 10/2/26, 9:00 AM.");
        assert_eq!(timeout_notice("garbage", &now), "You're timed out.");
    }

    #[test]
    fn uptime_labels() {
        use std::time::Duration;
        assert_eq!(format_uptime(Duration::from_secs(5)), "5s");
        assert_eq!(format_uptime(Duration::from_secs(185)), "3m 05s");
        assert_eq!(format_uptime(Duration::from_secs(3 * 3600 + 120)), "3h 02m");
        assert_eq!(format_uptime(Duration::from_secs(2 * 86400 + 3600)), "2d 1h");
    }

}
