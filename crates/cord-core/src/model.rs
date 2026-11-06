//! The parts of Discord's payloads this client uses. Unknown fields are
//! ignored, and anything Discord may omit has a default.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::Id;
use crate::rich::{Attachment, Component, Embed, MessageReference, StickerItem};

/// Decodes a list element by element, skipping (and warning about) elements
/// that fail to decode. Only the error is logged, never the payload.
pub(crate) fn lenient_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let items = Option::<Vec<Value>>::deserialize(d)?.unwrap_or_default();
    Ok(items
        .into_iter()
        .filter_map(|v| match serde_json::from_value::<T>(v) {
            Ok(t) => Some(t),
            Err(error) => {
                tracing::warn!(collection = std::any::type_name::<T>(), %error, "skipping undecodable element");
                None
            }
        })
        .collect())
}

/// Permission bitfields arrive as decimal strings (older payloads: numbers).
fn bits<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    Ok(match Option::<Value>::deserialize(d)? {
        Some(Value::String(s)) => s.parse().map_err(serde::de::Error::custom)?,
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        _ => 0,
    })
}

/// A server role. `permissions` is the raw Discord bitfield.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Role {
    pub id: Id,
    #[serde(default)]
    pub name: String,
    /// 0xRRGGBB; 0 means no colour.
    #[serde(default)]
    pub color: u32,
    #[serde(default)]
    pub position: i32,
    #[serde(default, deserialize_with = "bits")]
    pub permissions: u64,
    /// Shown as its own group in the member list.
    #[serde(default)]
    pub hoist: bool,
    /// Role icon image hash (CDN `role-icons/{id}/{hash}`).
    #[serde(default)]
    pub icon: Option<String>,
    /// Or a plain emoji as the role icon.
    #[serde(default)]
    pub unicode_emoji: Option<String>,
    /// Enhanced role colours: a second (and third, for holographic) colour
    /// makes the name a gradient.
    #[serde(default)]
    pub colors: Option<RoleColors>,
    /// Anyone may ping it.
    #[serde(default)]
    pub mentionable: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct RoleColors {
    #[serde(default)]
    pub primary_color: u32,
    #[serde(default)]
    pub secondary_color: Option<u32>,
    #[serde(default)]
    pub tertiary_color: Option<u32>,
}

impl Role {
    /// The colours a name with this role is drawn in: one, or a gradient's
    /// stops. Empty when the role has no colour.
    pub fn name_colors(&self) -> Vec<u32> {
        if self.color == 0 {
            return Vec::new();
        }
        let mut colors = vec![self.color];
        if let Some(c) = &self.colors {
            colors.extend(c.secondary_color);
            colors.extend(c.tertiary_color);
        }
        colors
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverwriteKind {
    Role,
    Member,
}

impl<'de> Deserialize<'de> for OverwriteKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::Number(n) if n.as_u64() == Some(1) => Ok(OverwriteKind::Member),
            Value::String(s) if s == "member" || s == "1" => Ok(OverwriteKind::Member),
            _ => Ok(OverwriteKind::Role),
        }
    }
}

/// A channel's permission override for one role or member.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct PermissionOverwrite {
    pub id: Id,
    #[serde(rename = "type")]
    pub kind: OverwriteKind,
    #[serde(default, deserialize_with = "bits")]
    pub allow: u64,
    #[serde(default, deserialize_with = "bits")]
    pub deny: u64,
}

/// Someone's membership of a server: nickname, server avatar, roles.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Member {
    /// READY's `merged_members` name the user by id…
    #[serde(default)]
    pub user_id: Option<Id>,
    /// …member events carry the whole user.
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub nick: Option<String>,
    /// Server-specific avatar hash.
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub roles: Vec<Id>,
    /// ISO 8601 end of a timeout, if any.
    #[serde(default)]
    pub communication_disabled_until: Option<String>,
    /// ISO 8601 time they joined the server.
    #[serde(default)]
    pub joined_at: Option<String>,
}

impl Member {
    pub fn user_id(&self) -> Option<Id> {
        self.user_id.or_else(|| self.user.as_ref().map(|u| u.id))
    }
}

/// One server's list of members, decoded leniently.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(transparent)]
pub struct MemberList(#[serde(deserialize_with = "lenient_vec")] pub Vec<Member>);

impl std::ops::Deref for MemberList {
    type Target = Vec<Member>;
    fn deref(&self) -> &Vec<Member> {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct User {
    pub id: Id,
    pub username: String,
    #[serde(default)]
    pub global_name: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub bot: bool,
    #[serde(flatten)]
    pub extras: UserExtras,
}

/// Discord's own account, whose messages tell you about yours.
pub const DISCORD_ACCOUNT: Id = Id(643_945_264_868_098_049);

impl User {
    /// Discord's own account, whose messages tell you about yours.
    pub fn is_discord(&self) -> bool {
        self.id == DISCORD_ACCOUNT
    }

    pub fn is_verified_bot(&self) -> bool {
        self.bot && self.extras.public_flags.is_some_and(|flags| flags & (1 << 16) != 0)
    }
}

/// The cosmetic parts of a user: server tag, avatar decoration, nameplate.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct UserExtras {
    /// Our own Nitro level, in READY: 0 or absent is none.
    #[serde(default)]
    pub premium_type: Option<u8>,
    /// Discord's account flags, of which we only read whether a bot's verified.
    #[serde(default)]
    pub public_flags: Option<u64>,
    /// Bots still have these: `eara#6801`. "0" for everyone else.
    #[serde(default)]
    pub discriminator: Option<String>,
    #[serde(default)]
    pub primary_guild: Option<PrimaryGuild>,
    #[serde(default)]
    pub avatar_decoration_data: Option<AvatarDecoration>,
    #[serde(default)]
    pub collectibles: Option<Collectibles>,
    /// Nitro "Display Name Styles": a font, an effect and its colours.
    #[serde(default)]
    pub display_name_styles: Option<NameStyle>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct NameStyle {
    #[serde(default)]
    pub font_id: u8,
    /// 1 solid, 2 gradient, 3 neon, 4 toon, 5 pop, 7 multi-colour.
    #[serde(default)]
    pub effect_id: u8,
    /// 0xRRGGBB, first to last.
    #[serde(default)]
    pub colors: Vec<u32>,
}

/// The server tag a user wears ("STBL" with its little badge).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct PrimaryGuild {
    #[serde(default)]
    pub identity_guild_id: Option<Id>,
    #[serde(default)]
    pub identity_enabled: Option<bool>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub badge: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct AvatarDecoration {
    #[serde(default)]
    pub asset: String,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Collectibles {
    #[serde(default)]
    pub nameplate: Option<Nameplate>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Nameplate {
    /// A folder under the collectibles CDN, ending in `/`.
    #[serde(default)]
    pub asset: String,
    /// The name of the colour its row is tinted with.
    #[serde(default)]
    pub palette: String,
}

impl User {
    /// The tag and the badge image to show beside the name, when worn.
    pub fn server_tag(&self) -> Option<(&str, Option<String>)> {
        let g = self.extras.primary_guild.as_ref()?;
        if g.identity_enabled == Some(false) {
            return None;
        }
        let tag = g.tag.as_deref().filter(|t| !t.is_empty())?;
        let badge = match (g.identity_guild_id, g.badge.as_deref()) {
            (Some(guild), Some(badge)) => Some(format!("https://cdn.discordapp.com/clan-badges/{guild}/{badge}.png?size=32")),
            _ => None,
        };
        Some((tag, badge))
    }

    /// Their avatar decoration, sharp at `size` pixels, for where it's shown large.
    pub fn decoration_url_at(&self, size: u32) -> Option<String> {
        let asset = &self.extras.avatar_decoration_data.as_ref()?.asset;
        (!asset.is_empty()).then(|| format!("https://cdn.discordapp.com/avatar-decoration-presets/{asset}.png?size={size}&passthrough=false"))
    }

    pub fn decoration_url(&self) -> Option<String> {
        let asset = &self.extras.avatar_decoration_data.as_ref()?.asset;
        (!asset.is_empty()).then(|| format!("https://cdn.discordapp.com/avatar-decoration-presets/{asset}.png?size=128&passthrough=false"))
    }

    /// The nameplate's animated version, played while the row is hovered.
    pub fn nameplate_video_url(&self) -> Option<String> {
        let asset = &self.extras.collectibles.as_ref()?.nameplate.as_ref()?.asset;
        (!asset.is_empty()).then(|| format!("https://cdn.discordapp.com/assets/collectibles/{asset}asset.webm"))
    }

    /// The name of the colour their nameplate tints its row with.
    pub fn nameplate_palette(&self) -> Option<&str> {
        let palette = &self.extras.collectibles.as_ref()?.nameplate.as_ref()?.palette;
        (!palette.is_empty()).then_some(palette.as_str())
    }

    pub fn nameplate_url(&self) -> Option<String> {
        let asset = &self.extras.collectibles.as_ref()?.nameplate.as_ref()?.asset;
        (!asset.is_empty()).then(|| format!("https://cdn.discordapp.com/assets/collectibles/{asset}static.png"))
    }
    /// How the account is written out: `name`, or `name#1234` for a bot.
    pub fn handle(&self) -> String {
        match self.extras.discriminator.as_deref().filter(|d| self.bot && *d != "0") {
            Some(tag) => format!("{}#{tag}", self.username),
            None => self.username.clone(),
        }
    }

    pub fn display_name(&self) -> &str {
        self.global_name.as_deref().filter(|n| !n.is_empty()).unwrap_or(&self.username)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelKind {
    Text,
    Dm,
    Voice,
    GroupDm,
    Category,
    Announcement,
    Stage,
    Forum,
    /// A thread (announcement, public or private) or a forum post.
    Thread,
    /// A media channel: a forum whose posts are mostly pictures.
    Media,
    Other(u8),
}

impl ChannelKind {
    /// Channels whose messages this client can show and send to.
    pub fn is_text_like(self) -> bool {
        matches!(self, Self::Text | Self::Dm | Self::GroupDm | Self::Announcement | Self::Thread)
    }

    /// Channels that hold posts (each a thread) instead of messages.
    pub fn is_forum_like(self) -> bool {
        matches!(self, Self::Forum | Self::Media)
    }
}

impl From<u8> for ChannelKind {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Text,
            1 => Self::Dm,
            2 => Self::Voice,
            3 => Self::GroupDm,
            4 => Self::Category,
            5 => Self::Announcement,
            13 => Self::Stage,
            15 => Self::Forum,
            16 => Self::Media,
            10..=12 => Self::Thread,
            other => Self::Other(other),
        }
    }
}

impl<'de> Deserialize<'de> for ChannelKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        u8::deserialize(d).map(Self::from)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Channel {
    pub id: Id,
    pub kind: ChannelKind,
    pub guild_id: Option<Id>,
    pub name: Option<String>,
    pub position: i32,
    pub parent_id: Option<Id>,
    pub last_message_id: Option<Id>,
    pub recipient_ids: Vec<Id>,
    pub recipients: Vec<User>,
    /// Who owns a group DM.
    pub owner_id: Option<Id>,
    /// A group DM's picture (`channel-icons/{id}/{hash}`).
    pub icon: Option<String>,
    pub permission_overwrites: Vec<PermissionOverwrite>,
    /// The topic line of a server text channel; a forum's guidelines.
    pub topic: Option<String>,
    /// Set on threads and forum posts.
    pub thread: Option<ThreadInfo>,
    /// Set on forum and media channels.
    pub forum: Option<ForumInfo>,
}

/// A thread's flag for a post pinned to the top of its forum.
pub const THREAD_PINNED: u64 = 1 << 1;
/// A forum's flag: new posts must carry a tag.
pub const FORUM_REQUIRE_TAG: u64 = 1 << 4;

/// What a thread or forum post carries beyond a plain channel.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ThreadInfo {
    /// A private thread: only invited people see it.
    pub private: bool,
    pub archived: bool,
    pub locked: bool,
    /// Minutes of quiet before it archives itself.
    pub auto_archive: u32,
    /// ISO 8601: when it was last archived or unarchived.
    pub archived_at: Option<String>,
    /// ISO 8601; absent on threads made before 2022-01-09.
    pub created_at: Option<String>,
    /// Messages in it, not counting the first or deleted ones (Discord stops at 50 on old clients).
    pub message_count: u32,
    pub member_count: u32,
    /// We're a member: it shows in the channel list and pings us.
    pub joined: bool,
    pub flags: u64,
    /// A forum post's tags.
    pub applied_tags: Vec<Id>,
}

impl ThreadInfo {
    pub fn pinned(&self) -> bool {
        self.flags & THREAD_PINNED != 0
    }
}

/// A forum's label for its posts.
#[derive(Clone, Debug, PartialEq)]
pub struct ForumTag {
    pub id: Id,
    pub name: String,
    /// Only moderators may put it on a post.
    pub moderated: bool,
    pub emoji_id: Option<Id>,
    /// A unicode emoji, or the custom one's name.
    pub emoji_name: Option<String>,
}

/// What a forum or media channel adds to a channel.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ForumInfo {
    pub tags: Vec<ForumTag>,
    /// 0 latest activity, 1 creation date.
    pub sort_order: u8,
    /// 1 list, 2 gallery; 0 when the server hasn't picked.
    pub layout: u8,
    pub flags: u64,
    /// The reaction offered on each post.
    pub reaction: Option<(Option<Id>, Option<String>)>,
    /// What the body of a new post starts with.
    pub template: Option<String>,
}

impl ForumInfo {
    pub fn requires_tag(&self) -> bool {
        self.flags & FORUM_REQUIRE_TAG != 0
    }

    pub fn tag(&self, id: Id) -> Option<&ForumTag> {
        self.tags.iter().find(|t| t.id == id)
    }
}

#[derive(Deserialize)]
struct RawTag {
    id: Id,
    #[serde(default)]
    name: String,
    #[serde(default)]
    moderated: bool,
    #[serde(default)]
    emoji_id: Option<Id>,
    #[serde(default)]
    emoji_name: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawThreadMeta {
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    locked: bool,
    #[serde(default)]
    auto_archive_duration: u32,
    #[serde(default)]
    archive_timestamp: Option<String>,
    #[serde(default)]
    create_timestamp: Option<String>,
}

#[derive(Deserialize)]
struct RawReaction {
    #[serde(default)]
    emoji_id: Option<Id>,
    #[serde(default)]
    emoji_name: Option<String>,
}

#[derive(Deserialize)]
struct RawChannel {
    #[serde(default)]
    owner_id: Option<Id>,
    id: Id,
    #[serde(rename = "type")]
    kind: u8,
    #[serde(default)]
    guild_id: Option<Id>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    position: i32,
    #[serde(default)]
    parent_id: Option<Id>,
    #[serde(default)]
    last_message_id: Option<Id>,
    #[serde(default)]
    recipient_ids: Vec<Id>,
    #[serde(default)]
    recipients: Vec<User>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default, deserialize_with = "lenient_vec")]
    permission_overwrites: Vec<PermissionOverwrite>,
    #[serde(default)]
    topic: Option<String>,
    #[serde(default)]
    thread_metadata: Option<RawThreadMeta>,
    /// Present on a thread we've joined.
    #[serde(default)]
    member: Option<serde_json::Value>,
    #[serde(default)]
    message_count: u32,
    #[serde(default)]
    member_count: u32,
    #[serde(default)]
    flags: u64,
    #[serde(default, deserialize_with = "lenient_vec")]
    applied_tags: Vec<Id>,
    #[serde(default, deserialize_with = "lenient_vec")]
    available_tags: Vec<RawTag>,
    #[serde(default)]
    default_sort_order: Option<u8>,
    #[serde(default)]
    default_forum_layout: Option<u8>,
    #[serde(default)]
    default_reaction_emoji: Option<RawReaction>,
    #[serde(default)]
    template: Option<String>,
}

impl<'de> Deserialize<'de> for Channel {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let r = RawChannel::deserialize(d)?;
        let recipient_ids = if r.recipient_ids.is_empty() {
            r.recipients.iter().map(|u| u.id).collect()
        } else {
            r.recipient_ids
        };
        let kind = ChannelKind::from(r.kind);
        let thread = (kind == ChannelKind::Thread).then(|| {
            let meta = r.thread_metadata.unwrap_or_default();
            ThreadInfo {
                private: r.kind == 12,
                archived: meta.archived,
                locked: meta.locked,
                auto_archive: meta.auto_archive_duration,
                archived_at: meta.archive_timestamp,
                created_at: meta.create_timestamp,
                message_count: r.message_count,
                member_count: r.member_count,
                joined: r.member.as_ref().is_some_and(|m| !m.is_null()),
                flags: r.flags,
                applied_tags: r.applied_tags,
            }
        });
        let forum = kind.is_forum_like().then(|| ForumInfo {
            tags: r
                .available_tags
                .into_iter()
                .map(|t| ForumTag { id: t.id, name: t.name, moderated: t.moderated, emoji_id: t.emoji_id.filter(|id| id.0 != 0), emoji_name: t.emoji_name.filter(|n| !n.is_empty()) })
                .collect(),
            sort_order: r.default_sort_order.unwrap_or(0),
            layout: r.default_forum_layout.unwrap_or(0),
            flags: r.flags,
            reaction: r.default_reaction_emoji.map(|e| (e.emoji_id, e.emoji_name)),
            template: r.template.filter(|t| !t.trim().is_empty()),
        });
        Ok(Channel {
            id: r.id,
            kind,
            guild_id: r.guild_id,
            name: r.name,
            position: r.position,
            parent_id: r.parent_id,
            last_message_id: r.last_message_id,
            recipient_ids,
            recipients: r.recipients,
            owner_id: r.owner_id,
            icon: r.icon,
            permission_overwrites: r.permission_overwrites,
            topic: r.topic.filter(|t| !t.trim().is_empty()),
            thread,
            forum,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Guild {
    pub id: Id,
    pub name: String,
    pub icon: Option<String>,
    pub unavailable: bool,
    pub channels: Vec<Channel>,
    pub roles: Vec<Role>,
    pub owner_id: Option<Id>,
    /// Who sat in which voice channel when the guild was sent.
    pub voice_states: Vec<VoiceState>,
    pub emojis: Vec<GuildEmoji>,
    /// Its boost level, 0 to 3.
    pub premium_tier: u8,
    pub banner: Option<String>,
    /// The channel set as its rules channel.
    pub rules_channel: Option<Id>,
}

/// A server's own emoji.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct GuildEmoji {
    pub id: Id,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub animated: bool,
    /// False while the server's boosts don't cover it.
    #[serde(default = "yes")]
    pub available: bool,
    /// Non-empty: only these roles may use it.
    #[serde(default)]
    pub roles: Vec<Id>,
}

fn yes() -> bool {
    true
}

/// Someone's presence in a voice channel. READY and GUILD_CREATE list them per
/// guild; VOICE_STATE_UPDATE changes them. Everything but the user is lenient.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct VoiceState {
    /// Absent inside a guild's own `voice_states` list.
    #[serde(default)]
    pub guild_id: Option<Id>,
    pub user_id: Id,
    /// `None` when the user left voice.
    #[serde(default)]
    pub channel_id: Option<Id>,
    #[serde(default)]
    pub self_mute: bool,
    #[serde(default)]
    pub self_deaf: bool,
    #[serde(default)]
    pub self_video: bool,
    #[serde(default)]
    pub self_stream: bool,
    /// The gateway session it's for: joining a call needs it.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Muted or deafened by a moderator.
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub deaf: bool,
}

impl VoiceState {
    pub fn is_muted(&self) -> bool {
        self.self_mute || self.mute
    }

    pub fn is_deafened(&self) -> bool {
        self.self_deaf || self.deaf
    }
}

#[derive(Deserialize, Default)]
struct GuildProperties {
    #[serde(default)]
    banner: Option<String>,
    #[serde(default)]
    premium_tier: Option<u8>,
    #[serde(default)]
    rules_channel_id: Option<Id>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    owner_id: Option<Id>,
}

#[derive(Deserialize)]
struct RawGuild {
    id: Id,
    #[serde(default)]
    banner: Option<String>,
    #[serde(default)]
    premium_tier: Option<u8>,
    #[serde(default)]
    rules_channel_id: Option<Id>,
    #[serde(default, deserialize_with = "lenient_vec")]
    emojis: Vec<GuildEmoji>,
    #[serde(default)]
    properties: Option<GuildProperties>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    unavailable: bool,
    #[serde(default, deserialize_with = "lenient_vec")]
    channels: Vec<Channel>,
    #[serde(default, deserialize_with = "lenient_vec")]
    roles: Vec<Role>,
    #[serde(default)]
    owner_id: Option<Id>,
    #[serde(default, deserialize_with = "lenient_vec")]
    voice_states: Vec<VoiceState>,
    /// Active threads; the ones we've joined (`member` set) show in the sidebar.
    #[serde(default)]
    threads: Vec<serde_json::Value>,
}

impl<'de> Deserialize<'de> for Guild {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let r = RawGuild::deserialize(d)?;
        let props = r.properties.unwrap_or_default();
        let mut channels = r.channels;
        let joined = r.threads.into_iter().filter(|t| {
            !t["member"].is_null() && !t["thread_metadata"]["archived"].as_bool().unwrap_or(false)
        });
        channels.extend(joined.filter_map(|t| serde_json::from_value::<Channel>(t).ok()));
        for c in &mut channels {
            c.guild_id = Some(r.id);
        }
        Ok(Guild {
            id: r.id,
            name: props.name.or(r.name).unwrap_or_default(),
            icon: props.icon.or(r.icon),
            unavailable: r.unavailable,
            channels,
            roles: r.roles,
            owner_id: props.owner_id.or(r.owner_id),
            voice_states: r.voice_states,
            emojis: r.emojis,
            premium_tier: props.premium_tier.or(r.premium_tier).unwrap_or(0),
            banner: props.banner.or(r.banner),
            rules_channel: props.rules_channel_id.or(r.rules_channel_id),
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct MessageCall {
    #[serde(default)]
    pub participants: Vec<Id>,
    #[serde(default)]
    pub ended_timestamp: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Message {
    pub id: Id,
    pub channel_id: Id,
    #[serde(default)]
    pub guild_id: Option<Id>,
    pub author: User,
    #[serde(default)]
    pub content: String,
    pub timestamp: String,
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default, deserialize_with = "string_or_number")]
    pub nonce: Option<String>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub attachments: Vec<Attachment>,
    /// The author's membership, on guild messages from the gateway.
    #[serde(default)]
    pub member: Option<Member>,
    /// Discord's message `type`: 0 default, 19 reply...
    #[serde(default, rename = "type")]
    pub kind: u8,
    #[serde(default)]
    pub flags: u64,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub embeds: Vec<Embed>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub components: Vec<Component>,
    #[serde(default)]
    pub message_reference: Option<MessageReference>,
    /// On a call's message: who was in it, and when it ended.
    #[serde(default)]
    pub call: Option<MessageCall>,
    /// The message a reply answers, when Discord included it (null when that
    /// message was deleted, absent when it wasn't fetched).
    #[serde(default, deserialize_with = "lenient_boxed")]
    pub referenced_message: Option<Box<Message>>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub sticker_items: Vec<StickerItem>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub mentions: Vec<User>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub reactions: Vec<Reaction>,
    #[serde(default)]
    pub mention_everyone: bool,
    #[serde(default)]
    pub mention_roles: Vec<Id>,
    #[serde(default, deserialize_with = "lenient_poll")]
    pub poll: Option<Box<crate::poll::Poll>>,
    /// Who ran the command this message answers.
    #[serde(default, deserialize_with = "lenient_command")]
    pub interaction: Option<CommandUse>,
    #[serde(default, deserialize_with = "lenient_command")]
    pub interaction_metadata: Option<CommandUse>,
    /// A forward's copy of the original message.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub message_snapshots: Vec<MessageSnapshot>,
    /// The thread started from this message, when Discord sent it along.
    #[serde(default, deserialize_with = "lenient_thread")]
    pub thread: Option<Box<Channel>>,
}

/// One forwarded message.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct MessageSnapshot {
    pub message: SnapshotMessage,
}

impl Message {
    /// Only we can see it, and it's gone at the next restart.
    pub fn is_ephemeral(&self) -> bool {
        self.flags & (1 << 6) != 0
    }
}

/// What a forward keeps of the original: no author or id.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct SnapshotMessage {
    #[serde(default)]
    pub content: String,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub attachments: Vec<Attachment>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub embeds: Vec<Embed>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub sticker_items: Vec<StickerItem>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub mentions: Vec<User>,
}

impl Message {
    /// A forward's copy of the original, as a message of its own to draw.
    pub fn forwarded(&self) -> Option<Message> {
        let snapshot = &self.message_snapshots.first()?.message;
        Some(Message {
            content: snapshot.content.clone(),
            attachments: snapshot.attachments.clone(),
            embeds: snapshot.embeds.clone(),
            sticker_items: snapshot.sticker_items.clone(),
            mentions: snapshot.mentions.clone(),
            message_snapshots: Vec::new(),
            reactions: Vec::new(),
            components: Vec::new(),
            edited_timestamp: None,
            kind: 0,
            ..self.clone()
        })
    }

    /// Who ran the command this message answers, and its name. Buttons and the
    /// like carry a user but no name: those aren't "used" anything.
    pub fn command_use(&self) -> Option<(&User, &str)> {
        [&self.interaction_metadata, &self.interaction]
            .into_iter()
            .flatten()
            .find_map(|c| Some((&c.user, c.name.as_deref().filter(|n| !n.is_empty())?)))
    }
}

/// A message's `interaction` or `interaction_metadata`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct CommandUse {
    #[serde(default)]
    pub name: Option<String>,
    pub user: User,
}

fn lenient_command<'de, D: Deserializer<'de>>(d: D) -> Result<Option<CommandUse>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?.and_then(|v| serde_json::from_value(v).ok()))
}

/// The emoji of a reaction: a custom one has an id, a unicode one only a name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Deserialize)]
pub struct ReactionEmoji {
    #[serde(default)]
    pub id: Option<Id>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub animated: bool,
}

impl ReactionEmoji {
    /// The emoji as the REST routes want it: `name:id` for custom emoji,
    /// otherwise the percent-encoded unicode.
    pub fn path_segment(&self) -> String {
        let name = self.name.as_deref().unwrap_or("");
        match self.id {
            Some(id) => format!("{name}:{id}"),
            None => percent_encode(name),
        }
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// One emoji's reactions on a message.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Reaction {
    pub emoji: ReactionEmoji,
    #[serde(default)]
    pub count: u32,
    /// The signed-in user reacted with this emoji.
    #[serde(default)]
    pub me: bool,
    /// Super reactions among `count`.
    #[serde(default)]
    pub burst_count: u32,
    /// The signed-in user sent a super reaction with this emoji. Separate
    /// from `me`: the two toggle independently.
    #[serde(default)]
    pub me_burst: bool,
}

/// A nested message that fails to decode counts as absent rather than
/// failing the whole payload.
pub(crate) fn lenient_poll<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Box<crate::poll::Poll>>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?.and_then(|v| serde_json::from_value(v).ok()).map(Box::new))
}

fn lenient_boxed<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Box<Message>>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?.and_then(|v| serde_json::from_value(v).ok()).map(Box::new))
}

fn lenient_thread<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Box<Channel>>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?.and_then(|v| serde_json::from_value(v).ok()).map(Box::new))
}

fn string_or_number<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Option::<Value>::deserialize(d)? {
        Some(Value::String(s)) => Some(s),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    })
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Ready {
    pub user: User,
    pub session_id: String,
    pub resume_gateway_url: String,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub guilds: Vec<Guild>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub private_channels: Vec<Channel>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub users: Vec<User>,
    /// Our own membership of each guild, in `guilds` order.
    #[serde(default)]
    pub merged_members: Vec<MemberList>,
    #[serde(default)]
    pub read_state: ReadStates,
    /// Base64 `PreloadedUserSettings` protobuf; see `settings_proto`.
    #[serde(default)]
    pub user_settings_proto: Option<String>,
    /// Friends and blocks; only the nicknames are read.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub relationships: Vec<Relationship>,
    /// Our own sessions, whose status is ours everywhere.
    #[serde(default)]
    pub sessions: Vec<Value>,
    #[serde(default)]
    pub user_guild_settings: GuildSettingsList,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Relationship {
    pub id: Id,
    /// 1 is a friend; the rest are requests and blocks, which aren't.
    #[serde(default, rename = "type")]
    pub kind: u8,
    #[serde(default)]
    pub nickname: Option<String>,
    /// When you became friends, ISO 8601.
    #[serde(default)]
    pub since: Option<String>,
}

/// Per-server notification settings (READY's `user_guild_settings`); the app
/// only reads what is muted.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct GuildSettingsList {
    #[serde(default, deserialize_with = "lenient_vec")]
    pub entries: Vec<GuildSettings>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct GuildSettings {
    /// `None`: the Direct Messages settings.
    #[serde(default)]
    pub guild_id: Option<Id>,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub mute_config: Option<MuteConfig>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub channel_overrides: Vec<ChannelOverride>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ChannelOverride {
    pub channel_id: Id,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub mute_config: Option<MuteConfig>,
}

/// How long a mute lasts. Discord leaves `muted` set after it's over: only the end says so.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct MuteConfig {
    #[serde(default)]
    pub end_time: Option<String>,
}

impl MuteConfig {
    /// The moment it ends, in milliseconds; `None` if it never does.
    pub fn ends_at(config: Option<&MuteConfig>) -> Option<i64> {
        let end = config?.end_time.as_deref()?;
        Some(chrono::DateTime::parse_from_rfc3339(end).ok()?.timestamp_millis())
    }
}

/// How far the account has read in each channel (READY's `read_state`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct ReadStates {
    #[serde(default, deserialize_with = "lenient_vec")]
    pub entries: Vec<ReadState>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ReadState {
    /// A channel, for type 0 (the only kind the app reads).
    pub id: Id,
    #[serde(default)]
    pub last_message_id: Option<Id>,
    #[serde(default)]
    pub mention_count: u32,
    #[serde(default)]
    pub read_state_type: u8,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_reactions() {
        let m: Message = serde_json::from_value(json!({
            "id": "1", "channel_id": "2", "author": {"id": "3", "username": "a"}, "timestamp": "t",
            "reactions": [
                {"emoji": {"id": null, "name": "\u{2705}"}, "count": 1, "me": true, "burst_count": 0},
                {"emoji": {"id": "9", "name": "pog", "animated": true}, "count": 4, "me": false},
                "garbage"
            ]
        }))
        .unwrap();
        assert_eq!(m.reactions.len(), 2);
        assert!(m.reactions[0].me && m.reactions[0].emoji.id.is_none());
        assert_eq!((m.reactions[1].count, m.reactions[1].emoji.animated), (4, true));
    }

    #[test]
    fn decodes_channel_topic() {
        let c: Channel = serde_json::from_value(json!({"id": "1", "type": 0, "topic": "put /x in your status"})).unwrap();
        assert_eq!(c.topic.as_deref(), Some("put /x in your status"));
        let blank: Channel = serde_json::from_value(json!({"id": "1", "type": 0, "topic": " "})).unwrap();
        assert_eq!(blank.topic, None);
    }

    #[test]
    fn emoji_path_segments() {
        let uni = ReactionEmoji { id: None, name: Some("\u{2705}".into()), animated: false };
        assert_eq!(uni.path_segment(), "%E2%9C%85");
        let custom = ReactionEmoji { id: Some(Id(9)), name: Some("pog".into()), animated: true };
        assert_eq!(custom.path_segment(), "pog:9");
    }
}
