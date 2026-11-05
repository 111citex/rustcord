//! What `cord-core` tells the app. Gateway dispatches are decoded here.

use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::Id;
use crate::member_list::MemberListUpdate;
use crate::model::{Channel, Guild, Member, Message, Ready, ReactionEmoji, Role, User, VoiceState, lenient_vec};
use crate::rich::{Attachment, Component, Embed};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Reconnecting { attempt: u32 },
}

#[derive(Deserialize)]
struct RecipientEvent {
    channel_id: Id,
    user: User,
}

/// Where a call is. Its token never gets printed: this is what opens the call.
#[derive(Clone, Deserialize)]
pub struct VoiceServer {
    pub token: String,
    /// `None` while the server's being changed: there's another one on the way.
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub guild_id: Option<Id>,
    #[serde(default)]
    pub channel_id: Option<Id>,
}

impl std::fmt::Debug for VoiceServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceServer").field("endpoint", &self.endpoint).field("guild_id", &self.guild_id).finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub enum CoreEvent {
    Ready(Box<Ready>),
    MessageCreate(Message),
    /// Only the parts that changed are present: `embeds` when Discord unfurled
    /// a link, `components`/`attachments`/`flags` when an edit changed them.
    MessageUpdate(Box<MessageEdit>),
    MessageDelete { id: Id, channel_id: Id },
    /// A purge: many messages gone at once.
    MessageDeleteBulk { ids: Vec<Id>, channel_id: Id },
    /// Someone reacted (`burst`: a super reaction).
    ReactionAdd { channel_id: Id, message_id: Id, user_id: Id, emoji: ReactionEmoji, burst: bool },
    ReactionRemove { channel_id: Id, message_id: Id, user_id: Id, emoji: ReactionEmoji, burst: bool },
    ReactionRemoveAll { channel_id: Id, message_id: Id },
    /// A server's emoji list changed.
    GuildEmojis { guild_id: Id, emojis: Vec<crate::model::GuildEmoji> },
    /// Someone voted on a poll, or (`add` false) took a vote back.
    PollVote { channel_id: Id, message_id: Id, user_id: Id, answer_id: u32, add: bool },
    ReactionRemoveEmoji { channel_id: Id, message_id: Id, emoji: ReactionEmoji },
    ChannelUpsert(Channel),
    ChannelDelete { id: Id },
    GuildCreate(Guild),
    GuildDelete { id: Id },
    RoleUpsert { guild_id: Id, role: Role },
    RoleDelete { guild_id: Id, role_id: Id },
    /// Someone's nickname, server avatar or roles changed (or they joined).
    MemberUpsert { guild_id: Id, member: Member },
    /// Members sent back for a gateway member search (op 8).
    MembersChunk { guild_id: Id, members: Vec<Member>, presences: Vec<(Id, crate::member_list::UserPresence)> },
    /// A change to the member list the app subscribed to.
    MemberListUpdate(Box<MemberListUpdate>),
    /// Someone joined, left, moved or muted in a voice channel of a server.
    /// `member` (when sent) names them without waiting for the member list.
    VoiceStateUpdate { guild_id: Id, state: VoiceState, member: Option<Member> },
    /// Someone joined or left a group chat.
    RecipientAdd { channel_id: Id, user: User },
    RecipientRemove { channel_id: Id, user_id: Id },
    /// A message was pinned or unpinned in this channel.
    PinsChanged { channel_id: Id },
    /// A call in a DM or group started, or changed who it's ringing.
    CallUpdate(crate::call::Call),
    /// The last person left a DM or group call.
    CallDelete { channel_id: Id },
    /// Someone joined, left or changed in a DM or group call.
    PrivateVoiceState(VoiceState),
    /// Which voice server to connect to, for the call we asked to join.
    VoiceServer(VoiceServer),
    /// Someone started typing (it lasts ten seconds, or until they send).
    TypingStart { channel_id: Id, guild_id: Option<Id>, user_id: Id, member: Option<Member> },
    /// READY_SUPPLEMENTAL: who sits in voice in each server, sent right after READY.
    VoiceSnapshot(Vec<(Id, Vec<VoiceState>)>, Vec<(Id, crate::member_list::UserPresence)>),
    /// A presence changed; `guild` when it came for a server member.
    PresenceUpdate { user: Id, guild: Option<Id>, presence: crate::member_list::UserPresence },
    /// A server's voice channels: (channel, status, started at, ms since 1970).
    ChannelInfo(Vec<(Id, Option<String>, Option<u64>)>),
    VoiceChannelStatus { id: Id, status: Option<String> },
    VoiceStartTime { id: Id, started: Option<u64> },
    /// Our own sessions changed; the status they agree on is ours.
    OwnSessions(Vec<Value>),
    /// A friend's nickname was set or cleared, or they stopped being a friend.
    FriendNickname { user: Id, nickname: Option<String> },
    /// The account read `channel_id` up to `message_id` (on any device).
    MessageAck { channel_id: Id, message_id: Id },
    Connection(ConnectionState),
    /// The token was rejected. The session is over; the app returns to login.
    AuthFailed,
}

/// Like `lenient_vec`, but a missing or null list stays `None`: an update
/// without `embeds` must not wipe the ones we have.
fn optional_lenient_vec<'de, D, T>(d: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = Option::<Value>::deserialize(d)?;
    value.map(|v| lenient_vec(v).map_err(serde::de::Error::custom)).transpose()
}

/// MESSAGE_UPDATE: a partial message. Absent fields mean "unchanged".
#[derive(Clone, Debug, Default, Deserialize)]
pub struct MessageEdit {
    pub id: Id,
    pub channel_id: Id,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    #[serde(default)]
    pub pinned: Option<bool>,
    #[serde(default, deserialize_with = "optional_lenient_vec")]
    pub embeds: Option<Vec<Embed>>,
    #[serde(default, deserialize_with = "optional_lenient_vec")]
    pub components: Option<Vec<Component>>,
    #[serde(default, deserialize_with = "optional_lenient_vec")]
    pub attachments: Option<Vec<Attachment>>,
    #[serde(default)]
    pub flags: Option<u64>,
    #[serde(default, deserialize_with = "optional_lenient_vec")]
    pub mentions: Option<Vec<User>>,
    #[serde(default)]
    pub mention_roles: Option<Vec<Id>>,
    #[serde(default)]
    pub mention_everyone: Option<bool>,
    #[serde(default, deserialize_with = "crate::model::lenient_poll")]
    pub poll: Option<Box<crate::poll::Poll>>,
}

#[derive(Deserialize)]
struct GuildEmojisEvent {
    guild_id: Id,
    #[serde(default)]
    emojis: Vec<crate::model::GuildEmoji>,
}

#[derive(Deserialize)]
struct PollVoteEvent {
    user_id: Id,
    channel_id: Id,
    message_id: Id,
    answer_id: u32,
}

#[derive(Deserialize)]
struct RoleEvent {
    guild_id: Id,
    #[serde(default)]
    role: Option<Role>,
    #[serde(default)]
    role_id: Option<Id>,
}

#[derive(Deserialize)]
struct MemberEvent {
    guild_id: Id,
    #[serde(flatten)]
    member: Member,
}

#[derive(Deserialize)]
struct MembersChunk {
    guild_id: Id,
    #[serde(default)]
    members: Vec<Member>,
    #[serde(default)]
    presences: Vec<ChunkPresence>,
}

#[derive(Deserialize)]
struct ChunkPresence {
    user: PartialUserId,
    #[serde(default)]
    status: crate::member_list::Status,
    #[serde(default)]
    activities: Vec<serde_json::Value>,
    #[serde(default)]
    client_status: serde_json::Value,
}

#[derive(Deserialize)]
struct PartialUserId {
    id: Id,
}

#[derive(Deserialize)]
struct VoiceEvent {
    #[serde(default)]
    guild_id: Option<Id>,
    #[serde(default)]
    member: Option<Member>,
    #[serde(flatten)]
    state: VoiceState,
}

#[derive(Deserialize)]
struct ReactionEvent {
    channel_id: Id,
    message_id: Id,
    #[serde(default)]
    user_id: Option<Id>,
    #[serde(default)]
    emoji: Option<ReactionEmoji>,
    #[serde(default)]
    burst: bool,
}

impl ReactionEvent {
    fn emoji(&self) -> Result<ReactionEmoji, serde_json::Error> {
        self.emoji.clone().ok_or_else(|| serde::de::Error::missing_field("emoji"))
    }
}

#[derive(Deserialize)]
/// Anything that only names a channel.
struct ChannelOnly {
    channel_id: Id,
}

#[derive(Deserialize)]
struct ById {
    id: Id,
    #[serde(default)]
    channel_id: Option<Id>,
    #[serde(default)]
    unavailable: bool,
}

#[derive(Deserialize)]
struct Typing {
    channel_id: Id,
    #[serde(default)]
    guild_id: Option<Id>,
    user_id: Id,
    #[serde(default)]
    member: Option<Member>,
}

#[derive(Deserialize)]
struct Supplemental {
    #[serde(default, deserialize_with = "crate::model::lenient_vec")]
    guilds: Vec<SupplementalGuild>,
    #[serde(default)]
    merged_presences: MergedPresences,
}

#[derive(Deserialize, Default)]
struct MergedPresences {
    #[serde(default, deserialize_with = "crate::model::lenient_vec")]
    friends: Vec<FriendPresence>,
}

#[derive(Deserialize)]
struct FriendPresence {
    user_id: Id,
    #[serde(default)]
    status: crate::member_list::Status,
    #[serde(default)]
    activities: Vec<Value>,
    #[serde(default)]
    client_status: Value,
}

#[derive(Deserialize)]
struct PresenceEvent {
    user: PresenceUser,
    #[serde(default)]
    guild_id: Option<Id>,
    #[serde(default)]
    status: crate::member_list::Status,
    #[serde(default)]
    activities: Vec<Value>,
    #[serde(default)]
    client_status: Value,
}

#[derive(Deserialize)]
struct PresenceUser {
    id: Id,
}

#[derive(Deserialize)]
struct ChannelInfoEvent {
    #[serde(default, deserialize_with = "crate::model::lenient_vec")]
    channels: Vec<ChannelInfoEntry>,
}

#[derive(Deserialize)]
struct ChannelInfoEntry {
    id: Id,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    voice_start_time: Value,
}

/// A start time as sent: unix seconds (or ms) as a number or digits.
fn start_ms(value: &Value) -> Option<u64> {
    let n = value.as_u64().or_else(|| value.as_f64().map(|f| f as u64)).or_else(|| value.as_str()?.parse().ok())?;
    Some(if n < 100_000_000_000 { n * 1000 } else { n })
}

#[derive(Deserialize)]
struct BulkDelete {
    ids: Vec<Id>,
    channel_id: Id,
}

#[derive(Deserialize)]
struct RelationshipEvent {
    id: Id,
    #[serde(default)]
    nickname: Option<String>,
}

#[derive(Deserialize)]
struct SupplementalGuild {
    id: Id,
    #[serde(default, deserialize_with = "crate::model::lenient_vec")]
    voice_states: Vec<VoiceState>,
}

#[derive(Deserialize)]
struct AckEvent {
    channel_id: Id,
    message_id: Id,
}

/// READY's keys worth keeping on disk: names, channels, roles, folders and
/// unread marks, and no message content. See `ready_snapshot`.
const SNAPSHOT_KEYS: [&str; 10] = [
    "guilds",
    "private_channels",
    "users",
    "merged_members",
    "read_state",
    "user_settings_proto",
    "user_guild_settings",
    "relationships",
    "sessions",
    "user",
];
/// Our own account fields left out of the snapshot.
const PRIVATE_USER_KEYS: [&str; 6] = ["email", "phone", "mfa_enabled", "verified", "premium_usage_flags", "purchased_flags"];

/// What to keep of READY between runs. Session ids and the like are dropped:
/// it's for showing the app at once, never for talking to Discord.
pub fn ready_snapshot(ready: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for key in SNAPSHOT_KEYS {
        if let Some(value) = ready.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    if let Some(user) = out.get_mut("user").and_then(Value::as_object_mut) {
        for key in PRIVATE_USER_KEYS {
            user.remove(key);
        }
    }
    // Each client's session id goes too; "all" is a label, and how our own
    // presence gets found.
    if let Some(sessions) = out.get_mut("sessions").and_then(Value::as_array_mut) {
        for session in sessions.iter_mut().filter_map(Value::as_object_mut) {
            if session.get("session_id").and_then(Value::as_str) != Some("all") {
                session.remove("session_id");
            }
        }
    }
    out.insert("session_id".into(), Value::String(String::new()));
    out.insert("resume_gateway_url".into(), Value::String(String::new()));
    Value::Object(out)
}

/// Where READY gets kept, once `Core` has said.
pub static READY_FILE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Writes the snapshot off to the side, so a big READY doesn't hold up the gateway.
fn keep_ready(ready: &Value) {
    let Some(path) = READY_FILE.get() else { return };
    let snapshot = ready_snapshot(ready);
    std::thread::spawn(move || {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, snapshot.to_string()).is_ok() {
            // Names of people and DMs: for this account's eyes.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
            }
            let _ = std::fs::rename(&tmp, path);
        }
    });
}

pub fn decode_dispatch(t: &str, d: Value) -> Option<CoreEvent> {
    use serde_json::from_value;
    let result = match t {
        "READY" => {
            keep_ready(&d);
            from_value::<Ready>(d).map(|r| CoreEvent::Ready(Box::new(r)))
        }
        "MESSAGE_CREATE" => from_value(d).map(CoreEvent::MessageCreate),
        "GUILD_EMOJIS_UPDATE" => from_value::<GuildEmojisEvent>(d).map(|e| CoreEvent::GuildEmojis { guild_id: e.guild_id, emojis: e.emojis }),
        "MESSAGE_POLL_VOTE_ADD" | "MESSAGE_POLL_VOTE_REMOVE" => from_value::<PollVoteEvent>(d).map(|v| CoreEvent::PollVote {
            channel_id: v.channel_id,
            message_id: v.message_id,
            user_id: v.user_id,
            answer_id: v.answer_id,
            add: t == "MESSAGE_POLL_VOTE_ADD",
        }),
        "MESSAGE_UPDATE" => from_value::<MessageEdit>(d).map(|u| CoreEvent::MessageUpdate(Box::new(u))),
        "MESSAGE_DELETE" => from_value::<ById>(d).and_then(|b| match b.channel_id {
            Some(channel_id) => Ok(CoreEvent::MessageDelete { id: b.id, channel_id }),
            None => Err(serde::de::Error::missing_field("channel_id")),
        }),
        "MESSAGE_DELETE_BULK" => from_value::<BulkDelete>(d).map(|b| CoreEvent::MessageDeleteBulk { ids: b.ids, channel_id: b.channel_id }),
        "MESSAGE_REACTION_ADD" | "MESSAGE_REACTION_REMOVE" => from_value::<ReactionEvent>(d).and_then(|r| {
            let emoji = r.emoji()?;
            let user_id = r.user_id.ok_or_else(|| serde::de::Error::missing_field("user_id"))?;
            let (channel_id, message_id, burst) = (r.channel_id, r.message_id, r.burst);
            Ok(if t == "MESSAGE_REACTION_ADD" {
                CoreEvent::ReactionAdd { channel_id, message_id, user_id, emoji, burst }
            } else {
                CoreEvent::ReactionRemove { channel_id, message_id, user_id, emoji, burst }
            })
        }),
        "MESSAGE_REACTION_REMOVE_ALL" => from_value::<ReactionEvent>(d)
            .map(|r| CoreEvent::ReactionRemoveAll { channel_id: r.channel_id, message_id: r.message_id }),
        "MESSAGE_REACTION_REMOVE_EMOJI" => from_value::<ReactionEvent>(d).and_then(|r| {
            Ok(CoreEvent::ReactionRemoveEmoji { channel_id: r.channel_id, message_id: r.message_id, emoji: r.emoji()? })
        }),
        "TYPING_START" => from_value::<Typing>(d).map(|t| CoreEvent::TypingStart {
            channel_id: t.channel_id,
            guild_id: t.guild_id,
            user_id: t.user_id,
            member: t.member,
        }),
        "READY_SUPPLEMENTAL" => from_value::<Supplemental>(d)
            .map(|s| {
                let presences = s
                    .merged_presences
                    .friends
                    .into_iter()
                    .map(|p| (p.user_id, crate::member_list::UserPresence::new(p.status, &p.activities, &p.client_status)))
                    .collect();
                CoreEvent::VoiceSnapshot(s.guilds.into_iter().map(|g| (g.id, g.voice_states)).collect(), presences)
            }),
        "PRESENCE_UPDATE" => from_value::<PresenceEvent>(d).map(|p| CoreEvent::PresenceUpdate {
            user: p.user.id,
            guild: p.guild_id,
            presence: crate::member_list::UserPresence::new(p.status, &p.activities, &p.client_status),
        }),
        "RELATIONSHIP_ADD" | "RELATIONSHIP_UPDATE" => {
            from_value::<RelationshipEvent>(d).map(|r| CoreEvent::FriendNickname { user: r.id, nickname: r.nickname })
        }
        "CHANNEL_INFO" => from_value::<ChannelInfoEvent>(d).map(|e| {
            CoreEvent::ChannelInfo(e.channels.into_iter().map(|c| (c.id, c.status.filter(|s| !s.is_empty()), start_ms(&c.voice_start_time))).collect())
        }),
        "VOICE_CHANNEL_STATUS_UPDATE" => from_value::<ChannelInfoEntry>(d).map(|c| CoreEvent::VoiceChannelStatus { id: c.id, status: c.status.filter(|s| !s.is_empty()) }),
        "VOICE_CHANNEL_START_TIME_UPDATE" => from_value::<ChannelInfoEntry>(d).map(|c| CoreEvent::VoiceStartTime { id: c.id, started: start_ms(&c.voice_start_time) }),
        "SESSIONS_REPLACE" => from_value::<Vec<Value>>(d).map(CoreEvent::OwnSessions),
        "RELATIONSHIP_REMOVE" => from_value::<RelationshipEvent>(d).map(|r| CoreEvent::FriendNickname { user: r.id, nickname: None }),
        "MESSAGE_ACK" => from_value::<AckEvent>(d)
            .map(|a| CoreEvent::MessageAck { channel_id: a.channel_id, message_id: a.message_id }),
        "CHANNEL_CREATE" | "CHANNEL_UPDATE" => from_value(d).map(CoreEvent::ChannelUpsert),
        "CHANNEL_DELETE" => from_value::<ById>(d).map(|b| CoreEvent::ChannelDelete { id: b.id }),
        "GUILD_CREATE" => from_value(d).map(CoreEvent::GuildCreate),
        "GUILD_DELETE" => match from_value::<ById>(d) {
            Ok(b) if b.unavailable => return None,
            other => other.map(|b| CoreEvent::GuildDelete { id: b.id }),
        },
        "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE" => from_value::<RoleEvent>(d).and_then(|r| match r.role {
            Some(role) => Ok(CoreEvent::RoleUpsert { guild_id: r.guild_id, role }),
            None => Err(serde::de::Error::missing_field("role")),
        }),
        "GUILD_ROLE_DELETE" => from_value::<RoleEvent>(d).and_then(|r| match r.role_id {
            Some(role_id) => Ok(CoreEvent::RoleDelete { guild_id: r.guild_id, role_id }),
            None => Err(serde::de::Error::missing_field("role_id")),
        }),
        "GUILD_MEMBER_ADD" | "GUILD_MEMBER_UPDATE" => {
            from_value::<MemberEvent>(d).map(|m| CoreEvent::MemberUpsert { guild_id: m.guild_id, member: m.member })
        }
        "GUILD_MEMBERS_CHUNK" => from_value::<MembersChunk>(d).map(|c| CoreEvent::MembersChunk {
            guild_id: c.guild_id,
            members: c.members,
            presences: c.presences.into_iter().map(|p| (p.user.id, crate::member_list::UserPresence::new(p.status, &p.activities, &p.client_status))).collect(),
        }),
        "GUILD_MEMBER_LIST_UPDATE" => from_value(d).map(|u| CoreEvent::MemberListUpdate(Box::new(u))),
        "CHANNEL_RECIPIENT_ADD" => from_value::<RecipientEvent>(d).map(|e| CoreEvent::RecipientAdd { channel_id: e.channel_id, user: e.user }),
        "CHANNEL_RECIPIENT_REMOVE" => from_value::<RecipientEvent>(d).map(|e| CoreEvent::RecipientRemove { channel_id: e.channel_id, user_id: e.user.id }),
        "CHANNEL_PINS_UPDATE" => from_value::<ChannelOnly>(d).map(|c| CoreEvent::PinsChanged { channel_id: c.channel_id }),
        "CALL_CREATE" | "CALL_UPDATE" => from_value(d).map(CoreEvent::CallUpdate),
        "CALL_DELETE" => from_value::<ChannelOnly>(d).map(|c| CoreEvent::CallDelete { channel_id: c.channel_id }),
        "VOICE_SERVER_UPDATE" => from_value(d).map(CoreEvent::VoiceServer),
        "VOICE_STATE_UPDATE" => match from_value::<VoiceEvent>(d) {
            Ok(v) => match v.guild_id.or(v.state.guild_id) {
                Some(guild_id) => Ok(CoreEvent::VoiceStateUpdate { guild_id, state: v.state, member: v.member }),
                None => Ok(CoreEvent::PrivateVoiceState(v.state)),
            },
            Err(error) => Err(error),
        },
        _ => return None,
    };
    match result {
        Ok(event) => Some(event),
        Err(error) => {
            // Never log the payload: it can contain message content.
            tracing::warn!(dispatch = t, %error, "undecodable dispatch");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ready_snapshot_keeps_the_app_and_drops_the_secrets() {
        let kept = ready_snapshot(&serde_json::json!({
            "user": {"id": "1", "username": "me", "email": "a@b.c", "phone": "123"},
            "session_id": "secret",
            "resume_gateway_url": "wss://x",
            "analytics_token": "secret",
            "guilds": [],
            "sessions": [{"session_id": "secret", "status": "dnd"}, {"session_id": "all", "status": "idle"}],
        }));
        assert_eq!(kept["user"]["username"], "me");
        assert_eq!(kept["sessions"][0]["status"], "dnd");
        assert_eq!(kept["sessions"][1]["session_id"], "all");
        assert!(kept["user"].get("email").is_none() && kept["user"].get("phone").is_none());
        assert_eq!(kept["session_id"], "");
        assert!(kept.get("analytics_token").is_none());
        assert!(kept.to_string().find("secret").is_none());
    }
    use serde_json::json;

    #[test]
    fn decodes_private_calls() {
        let create = decode_dispatch("CALL_CREATE", json!({"channel_id": "9", "ringing": ["3"], "voice_states": []}));
        assert!(matches!(create, Some(CoreEvent::CallUpdate(c)) if c.ringing == vec![Id(3)]));
        assert!(matches!(decode_dispatch("CALL_DELETE", json!({"channel_id": "9"})), Some(CoreEvent::CallDelete { channel_id: Id(9) })));
        let joined = decode_dispatch("VOICE_STATE_UPDATE", json!({"user_id": "2", "channel_id": "9", "guild_id": null}));
        assert!(matches!(joined, Some(CoreEvent::PrivateVoiceState(v)) if v.channel_id == Some(Id(9))));
    }

    #[test]
    fn decodes_poll_votes() {
        let body = json!({"user_id": "7", "channel_id": "5", "message_id": "6", "guild_id": "1", "answer_id": 2});
        let add = decode_dispatch("MESSAGE_POLL_VOTE_ADD", body.clone());
        assert!(matches!(add, Some(CoreEvent::PollVote { user_id: Id(7), answer_id: 2, add: true, .. })));
        let remove = decode_dispatch("MESSAGE_POLL_VOTE_REMOVE", body);
        assert!(matches!(remove, Some(CoreEvent::PollVote { add: false, .. })));
    }

    #[test]
    fn decodes_reaction_dispatches() {
        let add = decode_dispatch(
            "MESSAGE_REACTION_ADD",
            json!({"channel_id": "5", "message_id": "6", "user_id": "7", "emoji": {"id": null, "name": "x"}, "burst": false}),
        );
        assert!(matches!(add, Some(CoreEvent::ReactionAdd { user_id: Id(7), burst: false, .. })));
        let all = decode_dispatch("MESSAGE_REACTION_REMOVE_ALL", json!({"channel_id": "5", "message_id": "6"}));
        assert!(matches!(all, Some(CoreEvent::ReactionRemoveAll { message_id: Id(6), .. })));
        let one = decode_dispatch(
            "MESSAGE_REACTION_REMOVE_EMOJI",
            json!({"channel_id": "5", "message_id": "6", "emoji": {"id": "9", "name": "p"}}),
        );
        assert!(matches!(one, Some(CoreEvent::ReactionRemoveEmoji { .. })));
        assert!(decode_dispatch("MESSAGE_REACTION_ADD", json!({"channel_id": "5", "message_id": "6"})).is_none());
    }
}
