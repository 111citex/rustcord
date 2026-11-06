//! In-memory state built from READY and later events. Only channels the app
//! has opened keep messages, at most [`MESSAGE_CAP`] each.

use std::collections::{HashMap, VecDeque};

use crate::member_list::UserPresence;
use crate::Id;
use crate::cdn;
use crate::event::CoreEvent;
use crate::member_list::{self, ListMember, MemberListModel};
use crate::model::{Channel, Guild, Member, Message, ReactionEmoji, User, VoiceState};
use crate::permissions;
use crate::settings_proto::{self, GuildFolder};

pub const MESSAGE_CAP: usize = 200;
/// While the user scrolls back through older pages, a channel may hold up to
/// this many; it trims back to [`MESSAGE_CAP`] once they return to the bottom.
pub const SCROLLBACK_CAP: usize = 50_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendState {
    Sent,
    Pending,
    Failed,
}

#[derive(Clone, Debug)]
pub struct ChatMessage {
    pub message: Message,
    pub state: SendState,
    /// Changes whenever the message's content or state does, so views know
    /// which rows to measure again. Unique across all messages.
    pub revision: u64,
}

impl ChatMessage {
    pub fn new(message: Message, state: SendState) -> Self {
        ChatMessage { message, state, revision: next_revision() }
    }

    /// Marks the content or state as changed.
    fn touch(&mut self) {
        self.revision = next_revision();
    }
}

fn next_revision() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[derive(Debug)]
pub struct ChannelMessages {
    items: VecDeque<ChatMessage>,
    /// Older messages exist on the server than the ones held here.
    pub has_older: bool,
    /// Newer messages exist than the ones held here: it's a window somewhere back in
    /// the channel (a linked message), not the end of it. Live ones aren't added to it.
    pub has_newer: bool,
    /// Live messages evict the oldest beyond this.
    cap: usize,
}

impl Default for ChannelMessages {
    fn default() -> Self {
        ChannelMessages { items: VecDeque::new(), has_older: false, has_newer: false, cap: MESSAGE_CAP }
    }
}

impl ChannelMessages {
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn get(&self, i: usize) -> Option<&ChatMessage> {
        self.items.get(i)
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &ChatMessage> {
        self.items.iter()
    }

    fn position(&self, id: Id) -> Option<usize> {
        self.items.iter().position(|m| m.message.id == id)
    }

    fn push(&mut self, m: ChatMessage) {
        self.items.push_back(m);
        while self.items.len() > self.cap {
            self.items.pop_front();
            self.has_older = true;
        }
    }
}

/// How much the cache holds, for the app's info panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub guilds: usize,
    pub channels: usize,
    pub users: usize,
    pub messages: usize,
}

/// Whether `id` is muted right now: a timed mute is over once its end has passed.
fn still_muted(muted: &std::collections::HashMap<Id, Option<i64>>, id: Id) -> bool {
    muted.get(&id).is_some_and(|end| end.is_none_or(|end| end > chrono::Utc::now().timestamp_millis()))
}

/// What we may do to one person in a server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Moderation {
    pub kick: bool,
    pub ban: bool,
    pub timeout: bool,
    pub nickname: bool,
    pub roles: bool,
    /// Whether Discord's mod view is ours to open.
    pub mod_view: bool,
}

#[derive(Default)]
pub struct Cache {
    me: Option<User>,
    users: HashMap<Id, User>,
    guild_order: Vec<Id>,
    guilds: HashMap<Id, Guild>,
    channels: HashMap<Id, Channel>,
    messages: HashMap<Id, ChannelMessages>,
    /// (guild, user) → membership: nickname, server avatar, roles.
    members: HashMap<(Id, Id), Member>,
    /// (guild, user) → the voice channel they're in, and how.
    voice_states: HashMap<(Id, Id), VoiceState>,
    /// Voice channel → the (guild, user) keys connected to it, so listing a
    /// channel's participants doesn't scan every voice state.
    voice_by_channel: HashMap<Id, Vec<(Id, Id)>>,
    /// Changes whenever something the channel column shows does.
    sidebar_rev: u64,
    /// Our own reaction changes still waiting for Discord's answer.
    reactions_in_flight: HashMap<ReactionKey, InFlight>,
    /// Gateway echoes still due for our own settled reaction changes
    /// (`true`: an add), so late ones aren't counted twice.
    reaction_echoes: HashMap<ReactionKey, Vec<bool>>,
    /// The lazy member list of the channel the app is subscribed to.
    member_list: Option<MemberListModel>,
    /// The guild whose member list the app is subscribed to. Updates for any
    /// other guild are leftovers and ignored.
    member_sub: Option<Id>,
    /// The held list is the last channel's: the next SYNC replaces it.
    member_list_replacing: bool,
    /// The channel the list is for, the lists stepped away from, and which list
    /// each channel has turned out to use.
    member_channel: Option<Id>,
    saved_lists: HashMap<String, MemberListModel>,
    channel_lists: HashMap<Id, String>,
    /// Channel → how far we've read it and how many pings wait there.
    read: HashMap<Id, ReadMark>,
    /// Muted servers, and muted channels (by channel id).
    /// What's muted and when that ends (`None`: when it's undone).
    muted_guilds: std::collections::HashMap<Id, Option<i64>>,
    muted_channels: std::collections::HashMap<Id, Option<i64>>,
    /// The server rail's folders, in order (see `settings_proto`).
    folders: Vec<GuildFolder>,
    /// Voice channels' status text and when they started, ms since 1970.
    voice_info: HashMap<Id, (Option<String>, Option<u64>)>,
    /// Nicknames given to friends, shown outside servers.
    friend_nicks: HashMap<Id, String>,
    /// When each friendship began, ISO 8601.
    friends_since: HashMap<Id, String>,
    presences: HashMap<Id, UserPresence>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ReadMark {
    last: Option<Id>,
    mentions: u32,
}

/// A role's icon, as Discord draws it after a member's name.
#[derive(Clone, Debug, PartialEq)]
pub enum RoleIcon {
    Image { url: String, name: String },
    Emoji(String),
}

type ReactionKey = (Id, Id, ReactionEmoji);

/// An optimistic reaction change that hasn't been answered yet.
#[derive(Debug)]
struct InFlight {
    on: bool,
    /// Where the emoji sat in the message's list before the change.
    index: Option<usize>,
    /// Our gateway echo already arrived.
    echoed: bool,
}

impl Cache {
    /// Changes whenever what the channel column shows may have: servers,
    /// channels, permissions, voice participants, the DM list's order.
    pub fn sidebar_revision(&self) -> u64 {
        self.sidebar_rev
    }

    /// A new message in a channel: our own reads it up to there; anyone
    /// else's may make it unread, or ping us.
    fn note_unread(&mut self, m: &Message, was_unread: bool) {
        let Some(me) = self.me.as_ref().map(|u| u.id) else { return };
        if m.author.id == me {
            self.read.insert(m.channel_id, ReadMark { last: Some(m.id), mentions: 0 });
            if was_unread {
                self.bump_sidebar();
            }
            return;
        }
        let pinged = self.pings_me(m);
        if pinged {
            self.read.entry(m.channel_id).or_default().mentions += 1;
        }
        if pinged || self.is_unread(m.channel_id) != was_unread {
            self.bump_sidebar();
        }
    }

    /// Newer messages than we've read, in a channel that isn't muted.
    pub fn is_unread(&self, channel: Id) -> bool {
        if still_muted(&self.muted_channels, channel) {
            return false;
        }
        let Some(last) = self.channels.get(&channel).and_then(|c| c.last_message_id) else { return false };
        // Never opened: Discord counts that as read.
        let Some(mark) = self.read.get(&channel) else { return false };
        mark.last.is_none_or(|read| last > read)
    }

    /// The newest message we had read in `channel`, if it has unread ones.
    pub fn unread_after(&self, channel: Id) -> Option<Id> {
        self.is_unread(channel).then(|| self.read.get(&channel).and_then(|m| m.last)).flatten()
    }

    /// Pings waiting in a channel.
    pub fn mentions(&self, channel: Id) -> u32 {
        self.read.get(&channel).map_or(0, |m| m.mentions)
    }

    /// The server's unread channel with the newest message, unless the server is muted.
    pub fn newest_unread_channel(&self, guild: Id) -> Option<&Channel> {
        if still_muted(&self.muted_guilds, guild) {
            return None;
        }
        self.channels
            .values()
            .filter(|c| c.guild_id == Some(guild) && self.is_unread(c.id) && self.can_view(c.id))
            .max_by_key(|c| c.last_message_id)
    }

    /// Whether the server's rail icon gets the unread pill: some readable,
    /// unmuted channel has news, and the server itself isn't muted.
    pub fn guild_unread(&self, guild: Id) -> bool {
        !still_muted(&self.muted_guilds, guild)
            && self.channels.values().any(|c| {
                c.guild_id == Some(guild) && self.is_unread(c.id) && self.can_view(c.id)
            })
    }

    /// Pings across a server's channels (shown even when it's muted).
    pub fn guild_mentions(&self, guild: Id) -> u32 {
        self.channels.values().filter(|c| c.guild_id == Some(guild)).map(|c| self.mentions(c.id)).sum()
    }

    /// We've read `channel` up to its newest message.
    pub fn mark_read(&mut self, channel: Id) {
        let last = self.channels.get(&channel).and_then(|c| c.last_message_id);
        let was = self.read.get(&channel).copied().unwrap_or_default();
        if was.last != last || was.mentions != 0 {
            self.read.insert(channel, ReadMark { last, mentions: 0 });
            self.bump_sidebar();
        }
    }

    /// A conversation muted, or not, until we say otherwise.
    pub fn set_muted(&mut self, channel: Id, muted: bool) {
        if muted {
            self.muted_channels.insert(channel, None);
        } else {
            self.muted_channels.remove(&channel);
        }
        self.bump_sidebar();
    }

    /// Marks `channel` unread from the message after `before` on.
    pub fn mark_unread_after(&mut self, channel: Id, before: Id) {
        self.read.insert(channel, ReadMark { last: Some(before), mentions: 0 });
        self.bump_sidebar();
    }

    pub fn guild_folders(&self) -> &[GuildFolder] {
        &self.folders
    }

    fn bump_sidebar(&mut self) {
        self.sidebar_rev = next_revision();
    }

    /// Whether a change to `user`'s membership in `guild` shows in the
    /// sidebar: our own permissions, or the name of someone in voice.
    fn member_shows_in_sidebar(&self, guild: Id, user: Id) -> bool {
        self.me.as_ref().is_some_and(|m| m.id == user) || self.voice_states.contains_key(&(guild, user))
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            guilds: self.guilds().count(),
            channels: self.channels.len(),
            users: self.users.len(),
            messages: self.messages.values().map(ChannelMessages::len).sum(),
        }
    }

    pub fn member(&self, guild: Id, user: Id) -> Option<&Member> {
        self.members.get(&(guild, user))
    }

    /// `user`'s voice connection in `guild`, if any.
    pub fn voice_state(&self, guild: Id, user: Id) -> Option<&VoiceState> {
        self.voice_states.get(&(guild, user))
    }

    /// Everyone connected to voice channel `channel`, in no particular order.
    pub fn voice_participants(&self, channel: Id) -> Vec<&VoiceState> {
        self.voice_by_channel
            .get(&channel)
            .map(|keys| keys.iter().filter_map(|k| self.voice_states.get(k)).collect())
            .unwrap_or_default()
    }

    fn remove_voice(&mut self, key: (Id, Id)) {
        let Some(old) = self.voice_states.remove(&key) else { return };
        if let Some(channel) = old.channel_id {
            self.unindex_voice(channel, key);
        }
    }

    fn unindex_voice(&mut self, channel: Id, key: (Id, Id)) {
        if let Some(keys) = self.voice_by_channel.get_mut(&channel) {
            keys.retain(|k| *k != key);
            if keys.is_empty() {
                self.voice_by_channel.remove(&channel);
            }
        }
    }

    fn set_voice(&mut self, guild: Id, mut state: VoiceState) {
        let Some(channel) = state.channel_id else { return };
        state.guild_id = Some(guild);
        let key = (guild, state.user_id);
        self.remove_voice(key);
        self.voice_by_channel.entry(channel).or_default().push(key);
        self.voice_states.insert(key, state);
    }

    fn clear_guild_voice(&mut self, guild: Id) {
        let keys: Vec<(Id, Id)> = self.voice_states.keys().copied().filter(|(g, _)| *g == guild).collect();
        for key in keys {
            self.remove_voice(key);
        }
    }

    pub fn member_list(&self) -> Option<&MemberListModel> {
        self.member_list.as_ref()
    }

    /// Starts expecting the member list of a channel in `guild`: drops the
    /// held list, and takes the next one from its first SYNC.
    pub fn subscribe_member_list(&mut self, guild: Id) {
        self.subscribe_member_list_for(guild, None);
    }

    /// Discord sends a list's SYNC once per session: subscribing again to one we
    /// already had (or to a channel sharing the current one) brings none. So
    /// lists stepped away from are kept, and kept current, to come back to; and
    /// for an unknown channel the current list stays up until a SYNC replaces it.
    pub fn subscribe_member_list_for(&mut self, guild: Id, channel: Option<Id>) {
        if self.member_list.as_ref().is_none_or(|m| m.guild_id != guild) {
            self.member_list = None;
            self.saved_lists.clear();
            self.channel_lists.clear();
        }
        self.member_sub = Some(guild);
        self.member_channel = channel;
        self.member_list_replacing = false;
        let known = channel.and_then(|c| self.channel_lists.get(&c)).cloned();
        let Some(active) = self.member_list.take() else { return };
        match known {
            Some(id) if id == active.list_id => self.member_list = Some(active),
            Some(id) if self.saved_lists.contains_key(&id) => {
                self.member_list = self.saved_lists.remove(&id);
                self.saved_lists.insert(active.list_id.clone(), active);
            }
            _ => {
                self.saved_lists.insert(active.list_id.clone(), active.clone());
                self.member_list = Some(active);
                self.member_list_replacing = true;
            }
        }
    }

    /// A message's text as it reads, for where it can't be drawn properly: mentions and
    /// custom emoji by name instead of as `<@&123>`.
    pub fn readable_text(&self, m: &Message) -> String {
        let guild = m.guild_id.or_else(|| self.channels.get(&m.channel_id)?.guild_id);
        let mut out = String::with_capacity(m.content.len());
        let mut rest = m.content.as_str();
        while let Some(open) = rest.find('<') {
            out.push_str(&rest[..open]);
            let tag = &rest[open..];
            let Some(close) = tag.find('>') else {
                out.push_str(tag);
                return out;
            };
            let inner = &tag[1..close];
            let number = |text: &str| text.parse::<u64>().ok().map(Id);
            let shown = if let Some(role) = inner.strip_prefix("@&").and_then(number) {
                let name = guild.and_then(|g| self.guilds.get(&g)).and_then(|g| g.roles.iter().find(|r| r.id == role)).map(|r| r.name.clone());
                Some(format!("@{}", name.unwrap_or_else(|| "deleted-role".to_string())))
            } else if let Some(user) = inner.strip_prefix("@!").or_else(|| inner.strip_prefix('@')).and_then(number) {
                let known = m.mentions.iter().find(|u| u.id == user).or_else(|| self.users.get(&user));
                Some(format!("@{}", known.map_or_else(|| "someone".to_string(), |u| self.display_name(guild, u).to_string())))
            } else if let Some(channel) = inner.strip_prefix('#').and_then(number) {
                Some(format!("#{}", self.channels.get(&channel).and_then(|c| c.name.clone()).unwrap_or_else(|| "channel".to_string())))
            } else if inner.starts_with(':') || inner.starts_with("a:") {
                inner.split(':').nth(1).map(|name| format!(":{name}:"))
            } else {
                None
            };
            match shown {
                Some(shown) => out.push_str(&shown),
                None => out.push_str(&tag[..=close]),
            }
            rest = &tag[close + 1..];
        }
        out.push_str(rest);
        out
    }

    /// The server we're in that has this emoji.
    pub fn guild_with_emoji(&self, emoji: Id) -> Option<&Guild> {
        self.guilds.values().find(|g| g.emojis.iter().any(|e| e.id == emoji))
    }

    /// The channel a list from another channel is standing in for, until its own comes.
    pub fn member_list_stale_for(&self) -> Option<Id> {
        self.member_channel.filter(|_| self.member_list_replacing)
    }

    /// Whether `user` can see `channel`, as far as is known: unknown counts as yes.
    pub fn member_can_see(&self, channel: Id, user: Id) -> bool {
        let Some(place) = self.channels.get(&channel) else { return true };
        let Some(guild_id) = place.guild_id else { return true };
        let (Some(guild), Some(member)) = (self.guilds.get(&guild_id), self.member(guild_id, user)) else { return true };
        permissions::compute(guild, place, user, &member.roles) & permissions::VIEW_CHANNEL != 0
    }

    /// Stops expecting a member list and drops the held one.
    pub fn unsubscribe_member_list(&mut self) {
        self.member_list = None;
        self.saved_lists.clear();
        self.channel_lists.clear();
        self.member_channel = None;
        self.member_sub = None;
    }

    /// Whether the channel just subscribed to has had its list come in.
    pub fn member_list_arrived(&self) -> bool {
        !self.member_list_replacing && self.member_list.as_ref().is_some_and(|m| m.len() > 0)
    }

    /// Drops what the member list holds for the inclusive row range.
    pub fn forget_member_rows(&mut self, start: u32, end: u32) {
        if let Some(list) = &mut self.member_list {
            list.forget(start, end);
        }
    }

    /// The name to show for `user`: their nickname in `guild` (outside a
    /// server, the nickname you gave them as a friend), else their global
    /// display name, else their username.
    pub fn display_name<'a>(&'a self, guild: Option<Id>, user: &'a User) -> &'a str {
        let nick = match guild {
            Some(g) => self.member(g, user.id).and_then(|m| m.nick.as_deref()),
            None => self.friend_nicks.get(&user.id).map(String::as_str),
        };
        nick.filter(|n| !n.is_empty()).unwrap_or_else(|| user.display_name())
    }

    /// Our presence from our sessions: Discord's combined "all" entry when
    /// there is one, else the first.
    fn set_own_presence(&mut self, sessions: &[serde_json::Value]) {
        let Some(me) = self.me.as_ref().map(|u| u.id) else { return };
        let Some(session) = sessions.iter().find(|s| s["session_id"] == "all").or(sessions.first()) else { return };
        let status = serde_json::from_value(session["status"].clone()).unwrap_or_default();
        let activities = session["activities"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        self.presences.insert(me, UserPresence::new(status, activities, &session["client_info"]["none"]));
    }

    pub fn voice_status(&self, channel: Id) -> Option<&str> {
        self.voice_info.get(&channel)?.0.as_deref()
    }

    pub fn voice_started(&self, channel: Id) -> Option<u64> {
        self.voice_info.get(&channel)?.1
    }

    /// Everyone we're friends with.
    pub fn friend_ids(&self) -> Vec<Id> {
        self.friends_since.keys().copied().collect()
    }

    /// When `user` and we became friends, ISO 8601, if we are.
    pub fn friends_since(&self, user: Id) -> Option<&str> {
        self.friends_since.get(&user).map(String::as_str)
    }

    /// What we last heard of `user`'s presence (friends, mostly).
    pub fn presence(&self, user: Id) -> Option<&UserPresence> {
        self.presences.get(&user)
    }

    /// Whether `user` is in a voice channel of a server we share.
    pub fn in_voice(&self, user: Id) -> bool {
        self.voice_states.keys().any(|(_, u)| *u == user)
    }

    /// The colour of `user`'s name in `guild`: that of their highest role
    /// that has one (a role without a colour doesn't count).
    /// The icon of the member's highest role that has one: an image URL, or
    /// a plain emoji (`RoleIcon::Emoji`).
    pub fn role_icon(&self, guild: Id, user: Id) -> Option<RoleIcon> {
        let member = self.member(guild, user)?;
        self.role_icon_for(guild, &member.roles)
    }

    /// Like `role_icon`, for a role list the caller already has (the member list's).
    pub fn role_icon_for(&self, guild: Id, roles: &[Id]) -> Option<RoleIcon> {
        let role = self
            .guilds
            .get(&guild)?
            .roles
            .iter()
            .filter(|r| (r.icon.is_some() || r.unicode_emoji.is_some()) && roles.contains(&r.id))
            .max_by_key(|r| (r.position, r.id))?;
        Some(match (&role.icon, &role.unicode_emoji) {
            (Some(hash), _) => RoleIcon::Image {
                url: format!("https://cdn.discordapp.com/role-icons/{}/{hash}.png?size=40&quality=lossless", role.id),
                name: role.name.clone(),
            },
            (None, Some(emoji)) => RoleIcon::Emoji(emoji.clone()),
            (None, None) => return None,
        })
    }

    pub fn member_color(&self, guild: Id, user: Id) -> Option<u32> {
        let member = self.member(guild, user)?;
        self.guilds
            .get(&guild)?
            .roles
            .iter()
            .filter(|r| r.color != 0 && member.roles.contains(&r.id))
            .max_by_key(|r| (r.position, r.id))
            .map(|r| r.color)
    }

    /// People `@query` could mean in `channel`: the server's known members
    /// (or a DM's recipients) and whoever wrote the loaded messages, matched
    /// on server nickname, display name and username. Prefix matches first.
    pub fn mention_candidates(&self, guild: Option<Id>, channel: Id, query: &str, limit: usize) -> Vec<&User> {
        let q = query.to_lowercase();
        let mut pool: Vec<&User> = Vec::new();
        // Recent authors first: they're who people usually answer.
        if let Some(messages) = self.messages.get(&channel) {
            pool.extend(messages.iter().rev().map(|m| self.users.get(&m.message.author.id).unwrap_or(&m.message.author)));
        }
        match guild {
            Some(g) => pool.extend(self.members.keys().filter(|(mg, _)| *mg == g).filter_map(|(_, user)| self.users.get(user))),
            None => pool.extend(self.channels.get(&channel).map(|c| c.recipients.iter()).into_iter().flatten()),
        }
        let mut seen = std::collections::HashSet::new();
        pool.retain(|u| seen.insert(u.id));
        let mut found: Vec<(u8, usize, &User)> = pool
            .into_iter()
            .enumerate()
            .filter_map(|(order, user)| {
                let names = [Some(self.display_name(guild, user)), user.global_name.as_deref(), Some(user.username.as_str())];
                let rank = names.iter().flatten().map(|n| n.to_lowercase()).filter_map(|n| {
                    if n.starts_with(&q) { Some(0) } else if n.contains(&q) { Some(1) } else { None }
                }).min()?;
                Some((rank, order, user))
            })
            .collect();
        found.sort_by_key(|(rank, order, _)| (*rank, *order));
        found.into_iter().take(limit).map(|(_, _, u)| u).collect()
    }

    /// The colours of `user`'s name in `guild`: their highest coloured
    /// role's, with gradient stops when it has them.
    pub fn member_colors(&self, guild: Id, user: Id) -> Vec<u32> {
        let Some(member) = self.member(guild, user) else { return Vec::new() };
        self.guilds
            .get(&guild)
            .and_then(|g| {
                g.roles
                    .iter()
                    .filter(|r| r.color != 0 && member.roles.contains(&r.id))
                    .max_by_key(|r| (r.position, r.id))
            })
            .map(|r| r.name_colors())
            .unwrap_or_default()
    }

    /// Whether `user` joined `guild` within the week: Discord's "new here" mark.
    pub fn is_new_member(&self, guild: Id, user: Id) -> bool {
        const NEW_FOR: i64 = 7;
        self.member(guild, user)
            .and_then(|m| m.joined_at.as_deref())
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .is_some_and(|at| chrono::Utc::now().signed_duration_since(at).num_days() < NEW_FOR)
    }

    /// `user`'s avatar in `guild`: their server avatar when set, else the global one.
    pub fn avatar_url(&self, guild: Option<Id>, user: &User, size: u32) -> String {
        let server = guild
            .and_then(|g| Some((g, self.member(g, user.id)?.avatar.as_deref()?)))
            .filter(|(_, hash)| !hash.is_empty());
        match server {
            Some((guild, hash)) => cdn::guild_avatar_url(guild, user.id, hash, size),
            None => cdn::avatar_url(user.id, user.avatar.as_deref(), size),
        }
    }

    fn insert_member(&mut self, guild: Id, mut member: Member) {
        let Some(user) = member.user_id() else { return };
        if let Some(u) = member.user.take() {
            self.users.insert(u.id, u);
        }
        member.user_id = Some(user);
        if self.member_shows_in_sidebar(guild, user) {
            self.bump_sidebar();
        }
        self.members.insert((guild, user), member);
    }

    /// What a list row tells about a membership. The list doesn't carry
    /// timeouts, so an existing record keeps its own.
    fn merge_list_member(&mut self, guild: Id, m: &ListMember) {
        self.users.insert(m.user.id, m.user.clone());
        if self.member_shows_in_sidebar(guild, m.user.id) {
            self.bump_sidebar();
        }
        let record = self.members.entry((guild, m.user.id)).or_insert_with(|| Member {
            user_id: Some(m.user.id),
            user: None,
            nick: None,
            avatar: None,
            roles: Vec::new(),
            communication_disabled_until: None, joined_at: None,
        });
        record.nick = m.nick.clone();
        record.avatar = m.avatar.clone();
        record.roles = m.roles.clone();
    }

    pub fn me(&self) -> Option<&User> {
        self.me.as_ref()
    }

    /// What the logged-in user may do in `channel`. DMs allow everything;
    /// `None` while it can't be known (no READY yet, or our membership of the
    /// server hasn't arrived).
    pub fn my_permissions(&self, channel: Id) -> Option<u64> {
        let channel = self.channels.get(&channel)?;
        let Some(guild) = channel.guild_id else { return Some(permissions::ALL) };
        let me = self.me.as_ref()?.id;
        let member = self.member(guild, me)?;
        Some(permissions::compute(self.guilds.get(&guild)?, channel, me, &member.roles))
    }

    /// Whether `channel` should be listed. True while unknown, so nothing
    /// vanishes before READY's data is complete.
    pub fn can_view(&self, channel: Id) -> bool {
        self.my_permissions(channel).is_none_or(|p| p & permissions::VIEW_CHANNEL != 0)
    }

    /// Whether we hold this permission in `channel`. False while unknown: for
    /// offering actions, which are better missing than refused.
    pub fn has_permission(&self, channel: Id, permission: u64) -> bool {
        self.my_permissions(channel).is_some_and(|p| p & permission != 0)
    }

    /// Whether `m` is for us: any DM, `@everyone`, us by name, or one of our roles.
    pub fn pings_me(&self, m: &Message) -> bool {
        let Some(me) = self.me.as_ref().map(|u| u.id) else { return false };
        let dm = self.channels.get(&m.channel_id).is_some_and(|c| c.guild_id.is_none());
        let by_role = || {
            let guild = m.guild_id.or_else(|| self.channels.get(&m.channel_id)?.guild_id);
            guild.and_then(|g| self.member(g, me)).is_some_and(|member| m.mention_roles.iter().any(|r| member.roles.contains(r)))
        };
        dm || m.mention_everyone || m.mentions.iter().any(|u| u.id == me) || by_role()
    }

    /// `user` as drawn, name style and all. Copies differ in what they carry: the
    /// member list's has styles and READY's may not, while a message's author does.
    pub fn styled_user<'a>(&'a self, user: &'a User) -> &'a User {
        match self.users.get(&user.id) {
            Some(known) if known.extras.display_name_styles.is_some() || user.extras.display_name_styles.is_none() => known,
            _ => user,
        }
    }

    /// Our server-wide permissions, or none while they aren't known.
    pub fn guild_permissions(&self, guild: Id) -> u64 {
        let me = self.me.as_ref().map(|u| u.id);
        let member = me.and_then(|me| self.member(guild, me));
        match (self.guilds.get(&guild), me, member) {
            (Some(g), Some(me), Some(member)) => permissions::base(g, me, &member.roles),
            _ => 0,
        }
    }

    /// What we call a friend, if we gave them a name of our own.
    pub fn friend_nickname(&self, user: Id) -> Option<&str> {
        self.friend_nicks.get(&user).map(String::as_str).filter(|n| !n.is_empty())
    }

    /// What we may do to `target` in `guild`: each action needs its permission and, but for
    /// our own nickname, a higher place in the server's ranking than they have.
    pub fn moderation(&self, guild: Id, target: Id) -> Moderation {
        let (Some(me), Some(g)) = (self.me.as_ref().map(|u| u.id), self.guilds.get(&guild)) else { return Moderation::default() };
        let held = self.guild_permissions(guild);
        let may = |p: u64| held & p != 0;
        let roles = |user: Id| self.member(guild, user).map(|m| m.roles.clone()).unwrap_or_default();
        if target == me {
            return Moderation { nickname: may(permissions::CHANGE_NICKNAME), ..Moderation::default() };
        }
        let ranks = permissions::outranks(g, me, &roles(me), target, &roles(target));
        let admin = permissions::base(g, target, &roles(target)) & permissions::ADMINISTRATOR != 0;
        Moderation {
            kick: ranks && may(permissions::KICK_MEMBERS),
            ban: ranks && may(permissions::BAN_MEMBERS),
            timeout: ranks && !admin && may(permissions::MODERATE_MEMBERS),
            nickname: ranks && may(permissions::MANAGE_NICKNAMES),
            roles: ranks && may(permissions::MANAGE_ROLES),
            mod_view: may(permissions::KICK_MEMBERS | permissions::BAN_MEMBERS | permissions::MODERATE_MEMBERS),
        }
    }

    /// What's unread in a server, as (channel, newest message).
    pub fn unread_channels(&self, guild: Id) -> Vec<(Id, Id)> {
        self.channels
            .values()
            .filter(|c| c.guild_id == Some(guild) && (self.is_unread(c.id) || self.mentions(c.id) > 0))
            .filter_map(|c| Some((c.id, c.last_message_id?)))
            .collect()
    }

    /// The largest file we may send in `channel`, in bytes: the larger of what our
    /// Nitro allows and what the server's boosts do. Discord's own figures.
    pub fn upload_limit(&self, channel: Id) -> u64 {
        const MB: u64 = 1024 * 1024;
        let mine = match self.me.as_ref().and_then(|me| me.extras.premium_type) {
            Some(2) => 500 * MB,
            Some(1 | 3) => 50 * MB,
            _ => 10 * MB,
        };
        let tier = self.channels.get(&channel).and_then(|c| self.guilds.get(&c.guild_id?)).map_or(0, |g| g.premium_tier);
        let server = match tier {
            3 => 100 * MB,
            2 => 50 * MB,
            _ => 10 * MB,
        };
        mine.max(server)
    }

    pub fn is_rules_channel(&self, channel: &Channel) -> bool {
        channel.guild_id.and_then(|g| self.guilds.get(&g)).is_some_and(|g| g.rules_channel == Some(channel.id))
    }

    /// Whether we have Nitro of any kind.
    pub fn has_nitro(&self) -> bool {
        self.me.as_ref().and_then(|me| me.extras.premium_type).unwrap_or(0) != 0
    }

    /// The server emoji we may use writing in `channel`, by the same rules as
    /// Discord's own client: its server's only without Nitro, and none animated;
    /// with Nitro any server's, if the channel allows outside emoji. The channel's
    /// server comes first.
    pub fn usable_emojis(&self, channel: Id) -> Vec<&crate::model::GuildEmoji> {
        let here = self.channels.get(&channel).and_then(|c| c.guild_id);
        let nitro = self.has_nitro();
        let me = self.me.as_ref().map(|u| u.id);
        let may_use = |guild: Id, emoji: &crate::model::GuildEmoji| {
            let by_role = emoji.roles.is_empty() || me.and_then(|me| self.member(guild, me)).is_some_and(|m| emoji.roles.iter().any(|r| m.roles.contains(r)));
            emoji.available && by_role && (nitro || !emoji.animated)
        };
        let mut usable: Vec<&crate::model::GuildEmoji> = Vec::new();
        if let Some(guild) = here.and_then(|g| self.guilds.get(&g)) {
            usable.extend(guild.emojis.iter().filter(|e| may_use(guild.id, e)));
        }
        if nitro && (here.is_none() || self.my_permissions(channel).is_some_and(|p| p & permissions::USE_EXTERNAL_EMOJIS != 0)) {
            for id in &self.guild_order {
                if Some(*id) == here {
                    continue;
                }
                if let Some(guild) = self.guilds.get(id) {
                    usable.extend(guild.emojis.iter().filter(|e| may_use(guild.id, e)));
                }
            }
        }
        usable
    }

    pub fn is_friend(&self, user: Id) -> bool {
        self.friends_since.contains_key(&user)
    }

    /// Muted, the channel or its server.
    pub fn is_muted(&self, channel: Id) -> bool {
        still_muted(&self.muted_channels, channel) || self.channels.get(&channel).and_then(|c| c.guild_id).is_some_and(|g| still_muted(&self.muted_guilds, g))
    }

    /// Whether the user may ping @everyone, @here and any role in `channel`.
    pub fn can_mention_everyone(&self, channel: Id) -> bool {
        // Anyone in a group may: there are no permissions there.
        if self.channels.get(&channel).is_some_and(|c| c.kind == crate::model::ChannelKind::GroupDm) {
            return true;
        }
        self.my_permissions(channel).is_some_and(|p| p & permissions::MENTION_EVERYONE != 0)
    }

    /// Whether the user may write in `channel`. True while unknown: the
    /// server has the final say.
    pub fn can_send(&self, channel: Id) -> bool {
        self.my_permissions(channel).is_none_or(|p| p & permissions::SEND_MESSAGES != 0)
    }

    /// The raw ISO 8601 end of our timeout in `guild`, while it's still ahead.
    pub fn timed_out_until(&self, guild: Id) -> Option<String> {
        let until = self.member(guild, self.me.as_ref()?.id)?.communication_disabled_until.as_deref()?;
        let end = chrono::DateTime::parse_from_rfc3339(until).ok()?;
        (end > chrono::Utc::now()).then(|| until.to_string())
    }

    pub fn user(&self, id: Id) -> Option<&User> {
        self.users.get(&id)
    }

    pub fn guilds(&self) -> impl Iterator<Item = &Guild> {
        self.guild_order.iter().filter_map(|id| self.guilds.get(id)).filter(|g| !g.unavailable)
    }

    pub fn guild(&self, id: Id) -> Option<&Guild> {
        self.guilds.get(&id)
    }

    pub fn channel(&self, id: Id) -> Option<&Channel> {
        self.channels.get(&id)
    }

    pub fn guild_channels(&self, guild: Id) -> Vec<&Channel> {
        self.channels.values().filter(|c| c.guild_id == Some(guild)).collect()
    }

    pub fn private_channels(&self) -> Vec<&Channel> {
        let mut list: Vec<&Channel> = self.channels.values().filter(|c| c.guild_id.is_none()).collect();
        // Newest activity first; channels with no messages last.
        list.sort_by(|a, b| b.last_message_id.cmp(&a.last_message_id).then(a.id.cmp(&b.id)));
        list
    }

    pub fn messages(&self, channel: Id) -> Option<&ChannelMessages> {
        self.messages.get(&channel)
    }

    pub fn open_messages(&mut self, channel: Id) {
        self.messages.entry(channel).or_default();
    }

    pub fn forget_messages(&mut self, channel: Id) {
        self.messages.remove(&channel);
    }

    /// Replaces the buffer with a REST page (newest first, as Discord returns
    /// it). Live messages newer than the page, and unsent ones, are kept.
    pub fn set_history(&mut self, channel: Id, newest_first: Vec<Message>, limit: usize) {
        self.refresh_authors(&newest_first);
        let has_older = newest_first.len() >= limit;
        let newest = newest_first.first().map(|m| m.id);
        let old = self.messages.remove(&channel).unwrap_or_default();
        let mut buf = ChannelMessages::default();
        for m in newest_first.into_iter().rev() {
            buf.push(ChatMessage::new(m, SendState::Sent));
        }
        for m in old.items {
            let newer = newest.is_none_or(|n| m.message.id > n);
            if m.state != SendState::Sent || newer {
                buf.push(m);
            }
        }
        buf.has_older = buf.has_older || has_older;
        self.messages.insert(channel, buf);
    }

    /// Adds a page of older messages (newest first, as Discord returns it)
    /// above what's held, up to [`SCROLLBACK_CAP`]. A page shorter than
    /// `limit` means the channel's first message has been reached.
    pub fn prepend_older(&mut self, channel: Id, newest_first: Vec<Message>, limit: usize) {
        self.refresh_authors(&newest_first);
        let Some(buf) = self.messages.get_mut(&channel) else { return };
        let oldest = buf.items.front().map(|m| m.message.id);
        let reached_start = newest_first.len() < limit;
        let older: Vec<Message> = newest_first.into_iter().filter(|m| oldest.is_none_or(|o| m.id < o)).collect();
        let room = SCROLLBACK_CAP.saturating_sub(buf.items.len());
        let fits = older.len() <= room;
        buf.cap = SCROLLBACK_CAP;
        // Newest first: each goes in front of the previous one.
        for message in older.into_iter().take(room) {
            buf.items.push_front(ChatMessage::new(message, SendState::Sent));
        }
        buf.has_older = !(reached_start && fits);
    }

    /// What Discord says about people now, from the messages it just sent. Their copy here
    /// came from a list or READY, and without this stayed as it was for the whole session:
    /// a changed name style, decoration or server profile never showed.
    pub fn refresh_authors(&mut self, messages: &[Message]) {
        for m in messages {
            self.users.insert(m.author.id, m.author.clone());
        }
    }

    /// Shows only a window of `channel` found by asking for the messages around one
    /// (newest first, as Discord returns it): what's held now is dropped, since the two
    /// wouldn't join up. Unsent messages stay.
    pub fn set_around(&mut self, channel: Id, newest_first: Vec<Message>, limit: usize) {
        self.refresh_authors(&newest_first);
        let last = self.channels.get(&channel).and_then(|c| c.last_message_id);
        let newest = newest_first.first().map(|m| m.id);
        let full = newest_first.len() >= limit;
        let old = self.messages.remove(&channel).unwrap_or_default();
        let mut buf = ChannelMessages { cap: SCROLLBACK_CAP, ..Default::default() };
        for m in newest_first.into_iter().rev() {
            buf.items.push_back(ChatMessage::new(m, SendState::Sent));
        }
        buf.has_older = full;
        buf.has_newer = full && newest.is_none_or(|n| last.is_none_or(|last| n < last));
        buf.items.extend(old.items.into_iter().filter(|m| m.state != SendState::Sent));
        self.messages.insert(channel, buf);
    }

    /// Adds a page of newer messages (newest first) below what's held. A short page, or
    /// one reaching the channel's newest message, closes the gap to the present.
    pub fn append_newer(&mut self, channel: Id, newest_first: Vec<Message>, limit: usize) {
        self.refresh_authors(&newest_first);
        let last = self.channels.get(&channel).and_then(|c| c.last_message_id);
        let Some(buf) = self.messages.get_mut(&channel) else { return };
        let held = buf.items.iter().rev().find(|m| m.state == SendState::Sent).map(|m| m.message.id);
        let short = newest_first.len() < limit;
        let mut fresh: Vec<Message> = newest_first.into_iter().filter(|m| held.is_none_or(|h| m.id > h)).collect();
        fresh.reverse();
        let reached = fresh.last().map(|m| m.id).or(held).zip(last).is_some_and(|(n, last)| n >= last);
        // Before any unsent ones, which belong at the very end.
        let at = buf.items.iter().rposition(|m| m.state == SendState::Sent).map_or(0, |i| i + 1);
        for (offset, message) in fresh.into_iter().enumerate() {
            buf.items.insert(at + offset, ChatMessage::new(message, SendState::Sent));
        }
        buf.has_newer = !(short || reached);
    }

    /// Whether `channel` is a window back in its history, with newer messages unloaded.
    pub fn has_newer(&self, channel: Id) -> bool {
        self.messages.get(&channel).is_some_and(|m| m.has_newer)
    }

    /// Newest message held for `channel` (not counting unsent ones).
    pub fn newest(&self, channel: Id) -> Option<Id> {
        self.messages.get(&channel)?.items.iter().rev().find(|m| m.state == SendState::Sent).map(|m| m.message.id)
    }

    /// Oldest message held for `channel`; older pages are fetched before it.
    pub fn oldest(&self, channel: Id) -> Option<Id> {
        self.messages.get(&channel)?.items.front().map(|m| m.message.id)
    }

    /// Whether `channel` holds as much scrollback as it may.
    pub fn scrollback_full(&self, channel: Id) -> bool {
        self.messages.get(&channel).is_some_and(|m| m.items.len() >= SCROLLBACK_CAP)
    }

    /// Back at the bottom: drop scrollback beyond [`MESSAGE_CAP`].
    pub fn trim_to_tail(&mut self, channel: Id) {
        let Some(buf) = self.messages.get_mut(&channel) else { return };
        // A window back in the channel isn't the bottom, and its newest are not the ones to keep.
        if buf.has_newer {
            return;
        }
        buf.cap = MESSAGE_CAP;
        while buf.items.len() > MESSAGE_CAP {
            buf.items.pop_front();
            buf.has_older = true;
        }
    }

    pub fn add_pending(&mut self, message: Message) {
        let buf = self.messages.entry(message.channel_id).or_default();
        buf.push(ChatMessage::new(message, SendState::Pending));
    }

    pub fn mark_failed(&mut self, channel: Id, nonce: &str) {
        if let Some(m) = self.find_pending(channel, nonce) {
            m.state = SendState::Failed;
            m.touch();
        }
    }

    fn find_pending(&mut self, channel: Id, nonce: &str) -> Option<&mut ChatMessage> {
        self.messages.get_mut(&channel)?.items.iter_mut().find(|m| {
            m.state != SendState::Sent && m.message.nonce.as_deref() == Some(nonce)
        })
    }

    fn insert_channel(&mut self, mut channel: Channel) {
        for u in channel.recipients.drain(..) {
            self.users.insert(u.id, u);
        }
        self.channels.insert(channel.id, channel);
    }

    fn remove_channel(&mut self, id: Id) {
        self.channels.remove(&id);
        self.messages.remove(&id);
        for key in self.voice_by_channel.get(&id).cloned().unwrap_or_default() {
            self.remove_voice(key);
        }
    }

    fn insert_guild(&mut self, mut guild: Guild) {
        self.clear_guild_voice(guild.id);
        for v in std::mem::take(&mut guild.voice_states) {
            self.set_voice(guild.id, v);
        }
        self.bump_sidebar();
        let old: Vec<Id> = self.guild_channels(guild.id).iter().map(|c| c.id).collect();
        for id in old {
            self.channels.remove(&id);
        }
        for c in std::mem::take(&mut guild.channels) {
            self.insert_channel(c);
        }
        if !self.guild_order.contains(&guild.id) {
            self.guild_order.push(guild.id);
        }
        self.guilds.insert(guild.id, guild);
    }

    /// Makes `answers` our votes on a poll right away; returns what they were.
    pub fn set_my_poll_votes(&mut self, channel: Id, message: Id, answers: &[u32]) -> Option<Vec<u32>> {
        let chat = self.chat_message(channel, message)?;
        let before = chat.message.poll.as_mut()?.set_my_votes(answers);
        chat.touch();
        Some(before)
    }

    fn chat_message(&mut self, channel: Id, message: Id) -> Option<&mut ChatMessage> {
        let buf = self.messages.get_mut(&channel)?;
        let i = buf.position(message)?;
        buf.items.get_mut(i)
    }

    /// Finds or creates (count 0) `emoji`'s reaction and runs `change` on it;
    /// the closure gets whether it existed and says whether anything changed.
    fn change_reaction(
        &mut self,
        channel: Id,
        message: Id,
        emoji: &ReactionEmoji,
        change: impl FnOnce(&mut crate::model::Reaction, bool) -> bool,
    ) {
        let Some(chat) = self.chat_message(channel, message) else { return };
        let reactions = &mut chat.message.reactions;
        let (ix, existed) = match reactions.iter().position(|r| r.emoji == *emoji) {
            Some(i) => (i, true),
            None => {
                reactions.push(crate::model::Reaction { emoji: emoji.clone(), count: 0, me: false, burst_count: 0, me_burst: false });
                (reactions.len() - 1, false)
            }
        };
        if change(&mut reactions[ix], existed) {
            chat.touch();
        } else if !existed {
            reactions.pop();
        }
    }

    /// One person's reaction going away. `burst`: a super reaction, which
    /// only ever touches the burst flag, never `me`.
    fn remove_reaction(&mut self, channel: Id, message: Id, emoji: &ReactionEmoji, mine: bool, burst: bool) {
        let Some(chat) = self.chat_message(channel, message) else { return };
        let reactions = &mut chat.message.reactions;
        let Some(i) = reactions.iter().position(|r| r.emoji == *emoji) else { return };
        let r = &mut reactions[i];
        // Our own removal echoing one already applied optimistically.
        if mine && !burst && !r.me {
            return;
        }
        if mine && burst && !r.me_burst {
            return;
        }
        r.count = r.count.saturating_sub(1);
        if burst {
            r.burst_count = r.burst_count.saturating_sub(1);
            if mine {
                r.me_burst = false;
            }
        } else if mine {
            r.me = false;
        }
        if r.count == 0 {
            reactions.remove(i);
        }
        chat.touch();
    }

    /// Whether our change to `emoji` on `message` still awaits Discord's answer.
    pub fn reaction_in_flight(&self, channel: Id, message: Id, emoji: &ReactionEmoji) -> bool {
        self.reactions_in_flight.contains_key(&(channel, message, emoji.clone()))
    }

    /// Starts our own reaction change: shows it at once and, until
    /// [`settle_my_reaction`](Self::settle_my_reaction), ignores our own
    /// gateway echoes for it. Returns false (changing nothing) while an
    /// earlier change to the same reaction is still in flight.
    pub fn begin_my_reaction(&mut self, channel: Id, message: Id, emoji: &ReactionEmoji, on: bool) -> bool {
        let key = (channel, message, emoji.clone());
        if self.reactions_in_flight.contains_key(&key) {
            return false;
        }
        let index = self
            .chat_message(channel, message)
            .and_then(|c| c.message.reactions.iter().position(|r| r.emoji == *emoji));
        self.set_my_reaction(channel, message, emoji, on);
        self.reactions_in_flight.insert(key, InFlight { on, index, echoed: false });
        true
    }

    /// Discord answered our change. On failure the reaction goes back to how
    /// it was, in its old place, keeping what others did meanwhile.
    pub fn settle_my_reaction(&mut self, channel: Id, message: Id, emoji: &ReactionEmoji, ok: bool) {
        let key = (channel, message, emoji.clone());
        let Some(flight) = self.reactions_in_flight.remove(&key) else { return };
        if ok {
            // The echo may still be on its way.
            if !flight.echoed {
                let due = self.reaction_echoes.entry(key).or_default();
                due.push(flight.on);
                if due.len() > 4 {
                    due.remove(0);
                }
            }
            return;
        }
        if flight.on {
            self.remove_reaction(channel, message, emoji, true, false);
            return;
        }
        let Some(chat) = self.chat_message(channel, message) else { return };
        let reactions = &mut chat.message.reactions;
        match reactions.iter_mut().find(|r| r.emoji == *emoji) {
            Some(r) => {
                r.count += 1;
                r.me = true;
            }
            None => {
                let at = flight.index.unwrap_or(reactions.len()).min(reactions.len());
                reactions.insert(
                    at,
                    crate::model::Reaction { emoji: emoji.clone(), count: 1, me: true, burst_count: 0, me_burst: false },
                );
            }
        }
        chat.touch();
    }

    /// Whether a gateway reaction event is the echo of a change of ours that
    /// is in flight or just settled. Consumes the echo.
    fn is_my_echo(&mut self, channel: Id, message: Id, emoji: &ReactionEmoji, added: bool) -> bool {
        let key = (channel, message, emoji.clone());
        if let Some(due) = self.reaction_echoes.get_mut(&key) {
            if let Some(i) = due.iter().position(|d| *d == added) {
                due.remove(i);
                if due.is_empty() {
                    self.reaction_echoes.remove(&key);
                }
                return true;
            }
        }
        match self.reactions_in_flight.get_mut(&key) {
            Some(flight) if flight.on == added => {
                flight.echoed = true;
                true
            }
            // The late echo of an earlier change, or another device's.
            Some(_) => false,
            None => false,
        }
    }

    /// Optimistically adds or removes the signed-in user's reaction, before
    /// the server confirms. Calling it again with the same state changes
    /// nothing, so the gateway's echo can't double count.
    pub fn set_my_reaction(&mut self, channel: Id, message: Id, emoji: &ReactionEmoji, on: bool) {
        if on {
            self.change_reaction(channel, message, emoji, |r, _| {
                if r.me {
                    return false;
                }
                r.count += 1;
                r.me = true;
                true
            });
        } else {
            self.remove_reaction(channel, message, emoji, true, false);
        }
    }

    pub fn apply(&mut self, event: CoreEvent) {
        match event {
            CoreEvent::Ready(ready) => {
                // Open buffers survive a fresh READY so live messages keep flowing.
                let messages = std::mem::take(&mut self.messages);
                // The app re-sends its member list subscription after READY.
                let member_sub = self.member_sub;
                let member_channel = self.member_channel;
                let ready = *ready;
                let same_account = self.me.as_ref().is_some_and(|me| me.id == ready.user.id);
                let rev = self.sidebar_rev;
                let old = std::mem::take(self);
                self.messages = messages;
                self.member_sub = member_sub;
                self.member_channel = member_channel;
                self.sidebar_rev = rev;
                // A reconnect shouldn't show: what's on screen stays until what's new
                // replaces it. READY's own data lands on top of this just below, the
                // member list's SYNC replaces the list, and the voice snapshot the voice rows.
                if same_account {
                    self.member_list_replacing = old.member_list.is_some();
                    self.member_list = old.member_list;
                    self.members = old.members;
                    self.users = old.users;
                    self.presences = old.presences;
                    self.voice_states = old.voice_states;
                    self.voice_by_channel = old.voice_by_channel;
                    self.voice_info = old.voice_info;
                }
                self.bump_sidebar();
                self.users.insert(ready.user.id, ready.user.clone());
                self.me = Some(ready.user);
                for u in ready.users {
                    self.users.insert(u.id, u);
                }
                let guild_ids: Vec<Id> = ready.guilds.iter().map(|g| g.id).collect();
                for g in ready.guilds {
                    self.insert_guild(g);
                }
                for (guild, members) in guild_ids.into_iter().zip(ready.merged_members) {
                    for member in members.0 {
                        self.insert_member(guild, member);
                    }
                }
                for c in ready.private_channels {
                    self.insert_channel(c);
                }
                for r in ready.read_state.entries.into_iter().filter(|r| r.read_state_type == 0) {
                    self.read.insert(r.id, ReadMark { last: r.last_message_id, mentions: r.mention_count });
                }
                for g in ready.user_guild_settings.entries {
                    if let (Some(guild), true) = (g.guild_id, g.muted) {
                        self.muted_guilds.insert(guild, crate::model::MuteConfig::ends_at(g.mute_config.as_ref()));
                    }
                    self.muted_channels.extend(g.channel_overrides.iter().filter(|o| o.muted).map(|o| (o.channel_id, crate::model::MuteConfig::ends_at(o.mute_config.as_ref()))));
                }
                self.set_own_presence(&ready.sessions);
                self.friends_since = ready.relationships.iter().filter(|r| r.kind == 1).filter_map(|r| Some((r.id, r.since.clone()?))).collect();
                self.friend_nicks = ready.relationships.into_iter().filter_map(|r| Some((r.id, r.nickname?))).collect();
                self.folders = ready.user_settings_proto.as_deref().map(settings_proto::guild_folders).unwrap_or_default();
                tracing::info!(
                    voice_states = self.voice_states.len(),
                    read_states = self.read.len(),
                    folders = self.folders.len(),
                    "ready applied"
                );
            }
            CoreEvent::TypingStart { guild_id, member, .. } => {
                // The store keeps who is typing; the cache only learns names.
                if let (Some(guild), Some(member)) = (guild_id, member) {
                    self.insert_member(guild, member);
                }
            }
            CoreEvent::PresenceUpdate { user, guild, presence } => {
                let changed = self.presences.get(&user) != Some(&presence);
                self.presences.insert(user, presence);
                // Server members' changes reach the member list its own way;
                // the channel column only shows friends'.
                if changed && guild.is_none() {
                    self.bump_sidebar();
                }
            }
            CoreEvent::ChannelInfo(channels) => {
                self.voice_info.extend(channels.into_iter().map(|(id, status, started)| (id, (status, started))));
                self.bump_sidebar();
            }
            CoreEvent::VoiceChannelStatus { id, status } => {
                self.voice_info.entry(id).or_default().0 = status;
                self.bump_sidebar();
            }
            CoreEvent::VoiceStartTime { id, started } => {
                self.voice_info.entry(id).or_default().1 = started;
                self.bump_sidebar();
            }
            CoreEvent::OwnSessions(sessions) => {
                self.set_own_presence(&sessions);
                self.bump_sidebar();
            }
            CoreEvent::FriendNickname { user, nickname } => {
                match nickname {
                    Some(n) => self.friend_nicks.insert(user, n),
                    None => self.friend_nicks.remove(&user),
                };
                self.bump_sidebar();
            }
            CoreEvent::VoiceSnapshot(guilds, presences) => {
                self.presences.extend(presences);
                for (guild, states) in guilds {
                    self.clear_guild_voice(guild);
                    for v in states {
                        self.set_voice(guild, v);
                    }
                }
                tracing::info!(voice_states = self.voice_states.len(), "voice snapshot applied");
                self.bump_sidebar();
            }
            CoreEvent::MessageAck { channel_id, message_id } => {
                let was = self.read.get(&channel_id).copied();
                self.read.insert(channel_id, ReadMark { last: Some(message_id), mentions: 0 });
                if was.is_none_or(|w| w.last != Some(message_id) || w.mentions != 0) {
                    self.bump_sidebar();
                }
            }
            CoreEvent::GuildCreate(g) => self.insert_guild(g),
            CoreEvent::GuildDelete { id } => {
                self.guilds.remove(&id);
                self.members.retain(|(guild, _), _| *guild != id);
                self.clear_guild_voice(id);
                self.bump_sidebar();
                self.guild_order.retain(|g| *g != id);
                let channels: Vec<Id> = self.guild_channels(id).iter().map(|c| c.id).collect();
                for c in channels {
                    self.remove_channel(c);
                }
            }
            CoreEvent::ChannelUpsert(c) => {
                self.insert_channel(c);
                self.bump_sidebar();
            }
            CoreEvent::ChannelDelete { id } => {
                self.remove_channel(id);
                self.bump_sidebar();
            }
            CoreEvent::MessageCreate(mut m) => {
                self.users.insert(m.author.id, m.author.clone());
                if let (Some(guild), Some(mut member)) = (m.guild_id, m.member.take()) {
                    member.user_id = Some(m.author.id);
                    self.insert_member(guild, member);
                }
                let was_unread = self.is_unread(m.channel_id);
                if let Some(c) = self.channels.get_mut(&m.channel_id) {
                    c.last_message_id = Some(m.id);
                    // The DM list is ordered by last activity.
                    if c.guild_id.is_none() {
                        self.sidebar_rev = next_revision();
                    }
                }
                self.note_unread(&m, was_unread);
                let channel = m.channel_id;
                if let Some(nonce) = m.nonce.clone() {
                    let in_history = self.messages.get(&channel).is_some_and(|b| b.position(m.id).is_some());
                    if in_history {
                        // History already has the sent message: drop the pending copy.
                        if let Some(buf) = self.messages.get_mut(&channel) {
                            if let Some(i) = buf.items.iter().position(|x| {
                                x.state != SendState::Sent && x.message.nonce.as_deref() == Some(nonce.as_str())
                            }) {
                                buf.items.remove(i);
                            }
                        }
                        return;
                    }
                    if let Some(pending) = self.find_pending(channel, &nonce) {
                        *pending = ChatMessage::new(m, SendState::Sent);
                        return;
                    }
                }
                if let Some(buf) = self.messages.get_mut(&channel) {
                    // Not into a window back in history: it'd sit past a gap.
                    if buf.position(m.id).is_none() && !buf.has_newer {
                        buf.push(ChatMessage::new(m, SendState::Sent));
                    }
                }
            }
            // For whoever's in the call; nothing to keep here.
            CoreEvent::VoiceServer(_) => {}
            CoreEvent::RecipientAdd { channel_id, user } => {
                let id = user.id;
                self.users.insert(id, user);
                if let Some(channel) = self.channels.get_mut(&channel_id)
                    && !channel.recipient_ids.contains(&id)
                {
                    channel.recipient_ids.push(id);
                }
                self.bump_sidebar();
            }
            CoreEvent::RecipientRemove { channel_id, user_id } => {
                if let Some(channel) = self.channels.get_mut(&channel_id) {
                    channel.recipient_ids.retain(|id| *id != user_id);
                }
                self.bump_sidebar();
            }
            CoreEvent::MessageUpdate(edit) => {
                let edit = *edit;
                let Some(buf) = self.messages.get_mut(&edit.channel_id) else { return };
                let Some(i) = buf.position(edit.id) else { return };
                buf.items[i].touch();
                let m = &mut buf.items[i].message;
                if let Some(content) = edit.content {
                    m.content = content;
                }
                if edit.edited_timestamp.is_some() {
                    m.edited_timestamp = edit.edited_timestamp;
                }
                if let Some(pinned) = edit.pinned {
                    m.pinned = pinned;
                }
                if let Some(embeds) = edit.embeds {
                    m.embeds = embeds;
                }
                if edit.poll.is_some() {
                    m.poll = edit.poll;
                }
                if let Some(components) = edit.components {
                    m.components = components;
                }
                if let Some(attachments) = edit.attachments {
                    m.attachments = attachments;
                }
                if let Some(flags) = edit.flags {
                    m.flags = flags;
                }
                if let Some(mentions) = edit.mentions {
                    m.mentions = mentions;
                }
                if let Some(roles) = edit.mention_roles {
                    m.mention_roles = roles;
                }
                if let Some(everyone) = edit.mention_everyone {
                    m.mention_everyone = everyone;
                }
            }
            CoreEvent::MessageDelete { id, channel_id } => {
                if let Some(buf) = self.messages.get_mut(&channel_id) {
                    if let Some(i) = buf.position(id) {
                        buf.items.remove(i);
                    }
                }
            }
            CoreEvent::MessageDeleteBulk { ids, channel_id } => {
                if let Some(buf) = self.messages.get_mut(&channel_id) {
                    for id in ids {
                        if let Some(i) = buf.position(id) {
                            buf.items.remove(i);
                        }
                    }
                }
            }
            CoreEvent::GuildEmojis { guild_id, emojis } => {
                if let Some(guild) = self.guilds.get_mut(&guild_id) {
                    guild.emojis = emojis;
                }
            }
            CoreEvent::PollVote { channel_id, message_id, user_id, answer_id, add } => {
                let mine = self.me.as_ref().is_some_and(|m| m.id == user_id);
                if let Some(chat) = self.chat_message(channel_id, message_id) {
                    if let Some(poll) = chat.message.poll.as_mut() {
                        poll.apply_vote(answer_id, mine, add);
                        chat.touch();
                    }
                }
            }
            CoreEvent::ReactionAdd { channel_id, message_id, user_id, emoji, burst } => {
                let mine = self.me.as_ref().is_some_and(|m| m.id == user_id);
                if mine && !burst && self.is_my_echo(channel_id, message_id, &emoji, true) {
                    return;
                }
                self.change_reaction(channel_id, message_id, &emoji, |r, existed| {
                    // Our own echo of an optimistic change is already counted.
                    if existed && mine && ((!burst && r.me) || (burst && r.me_burst)) {
                        return false;
                    }
                    r.count += 1;
                    if burst {
                        r.burst_count += 1;
                        r.me_burst |= mine;
                    } else {
                        r.me |= mine;
                    }
                    true
                });
            }
            CoreEvent::ReactionRemove { channel_id, message_id, user_id, emoji, burst } => {
                let mine = self.me.as_ref().is_some_and(|m| m.id == user_id);
                if mine && !burst && self.is_my_echo(channel_id, message_id, &emoji, false) {
                    return;
                }
                self.remove_reaction(channel_id, message_id, &emoji, mine, burst);
            }
            CoreEvent::ReactionRemoveAll { channel_id, message_id } => {
                if let Some(chat) = self.chat_message(channel_id, message_id) {
                    chat.message.reactions.clear();
                    chat.touch();
                }
            }
            CoreEvent::ReactionRemoveEmoji { channel_id, message_id, emoji } => {
                if let Some(chat) = self.chat_message(channel_id, message_id) {
                    chat.message.reactions.retain(|r| r.emoji != emoji);
                    chat.touch();
                }
            }
            CoreEvent::RoleUpsert { guild_id, role } => {
                if let Some(g) = self.guilds.get_mut(&guild_id) {
                    match g.roles.iter_mut().find(|r| r.id == role.id) {
                        Some(existing) => *existing = role,
                        None => g.roles.push(role),
                    }
                    self.bump_sidebar();
                }
            }
            CoreEvent::RoleDelete { guild_id, role_id } => {
                if let Some(g) = self.guilds.get_mut(&guild_id) {
                    g.roles.retain(|r| r.id != role_id);
                    self.bump_sidebar();
                }
            }
            CoreEvent::MemberUpsert { guild_id, member } => self.insert_member(guild_id, member),
            CoreEvent::MembersChunk { guild_id, members, presences } => {
                for member in members {
                    self.insert_member(guild_id, member);
                }
                if !presences.is_empty() {
                    self.presences.extend(presences);
                    self.bump_sidebar();
                }
            }
            CoreEvent::VoiceStateUpdate { guild_id, state, member } => {
                if let Some(member) = member {
                    self.insert_member(guild_id, member);
                }
                match state.channel_id {
                    Some(_) => self.set_voice(guild_id, state),
                    None => self.remove_voice((guild_id, state.user_id)),
                }
                self.bump_sidebar();
            }
            CoreEvent::MemberListUpdate(update) => {
                if self.member_list.as_ref().is_none_or(|m| m.guild_id != update.guild_id) {
                    tracing::info!(guild = %update.guild_id, "member list: first update received");
                }
                let ours = self.member_sub == Some(update.guild_id);
                let sync = update.ops.iter().any(|op| matches!(op, member_list::Op::Sync { .. }));
                let current = self.member_list.as_ref().is_some_and(|m| m.is_for(&update));
                if ours && self.member_list_replacing && sync {
                    self.member_list = None;
                    self.member_list_replacing = false;
                }
                // A list we stepped away from stays current for coming back to.
                if ours && !current && self.member_list.is_some() {
                    if let Some(saved) = self.saved_lists.get_mut(&update.id) {
                        saved.apply(&update);
                        for m in update.members() {
                            self.merge_list_member(update.guild_id, m);
                        }
                        return;
                    }
                }
                if ours && sync {
                    self.saved_lists.remove(&update.id);
                }
                if !member_list::accepts(self.member_sub, self.member_list.as_ref(), &update) {
                    return;
                }
                // Everyone the list shows also fills in names, colours and avatars elsewhere.
                for m in update.members() {
                    self.merge_list_member(update.guild_id, m);
                }
                if self.member_list.is_none() {
                    self.member_list = Some(MemberListModel::new(update.guild_id, update.id.clone()));
                }
                if let Some(list) = &mut self.member_list {
                    list.apply(&update);
                }
                if let Some(channel) = self.member_channel.filter(|_| !self.member_list_replacing) {
                    self.channel_lists.insert(channel, update.id.clone());
                }
            }
            CoreEvent::Connection(_) | CoreEvent::AuthFailed => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::MessageEdit;
    use crate::model::{ChannelKind, Member, MemberList, Ready, Role};

    fn user(id: u64, name: &str) -> User {
        User { id: Id(id), username: name.into(), global_name: None, avatar: None, bot: false, extras: Default::default() }
    }

    fn channel(id: u64, guild: Option<u64>, kind: ChannelKind, last: Option<u64>) -> Channel {
        Channel {
            id: Id(id), kind, guild_id: guild.map(Id), name: Some(format!("c{id}")), position: 0,
            parent_id: None, last_message_id: last.map(Id), recipient_ids: vec![], recipients: vec![], owner_id: None, icon: None,
            permission_overwrites: vec![], topic: None,
        }
    }

    fn msg(id: u64, channel: u64) -> Message {
        Message {
            pinned: false,
            id: Id(id), channel_id: Id(channel), guild_id: None, author: user(2, "lin"),
            content: format!("m{id}"), timestamp: String::new(), edited_timestamp: None, poll: None, interaction: None, interaction_metadata: None, message_snapshots: Vec::new(),
            nonce: None, attachments: vec![], member: None,
            kind: 0, flags: 0, embeds: vec![], components: vec![], message_reference: None, call: None,
            referenced_message: None, sticker_items: vec![], mentions: vec![], reactions: vec![], mention_everyone: false, mention_roles: vec![],
        }
    }

    fn ready_cache() -> Cache {
        let mut c = Cache::default();
        c.apply(CoreEvent::Ready(Box::new(Ready {
            user: user(1, "me"),
            session_id: "s".into(),
            resume_gateway_url: "u".into(), read_state: Default::default(), user_settings_proto: None, relationships: vec![], sessions: vec![], user_guild_settings: Default::default(),
            guilds: vec![Guild {
                id: Id(300), name: "g".into(), icon: None, unavailable: false,
                channels: vec![channel(311, Some(300), ChannelKind::Text, None)],
                roles: vec![], owner_id: None, voice_states: vec![], emojis: vec![], premium_tier: 0, banner: None, rules_channel: None,
            }],
            private_channels: vec![
                channel(400, None, ChannelKind::Dm, Some(10)),
                channel(401, None, ChannelKind::GroupDm, Some(20)),
                channel(402, None, ChannelKind::Dm, None),
            ],
            users: vec![user(2, "lin")],
            merged_members: vec![],
        })));
        c
    }

    fn ids(c: &Cache, ch: u64) -> Vec<u64> {
        c.messages(Id(ch)).unwrap().iter().map(|m| m.message.id.0).collect()
    }

    /// Message ids `from..=to`, newest first, as the REST API returns pages.
    fn page(from: u64, to: u64, channel: u64) -> Vec<Message> {
        (from..=to).rev().map(|i| msg(i, channel)).collect()
    }

    #[test]
    fn older_pages_load_above_and_grow_past_the_cap() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        c.set_history(Id(311), page(801, 1000, 311), 50);
        c.prepend_older(Id(311), page(751, 800, 311), 50);
        let m = c.messages(Id(311)).unwrap();
        assert_eq!(m.len(), 250);
        assert_eq!(m.get(0).unwrap().message.id, Id(751));
        assert!(m.has_older);
        assert_eq!(c.oldest(Id(311)), Some(Id(751)));

        // A short page means the start of the channel.
        c.prepend_older(Id(311), page(731, 750, 311), 50);
        assert!(!c.messages(Id(311)).unwrap().has_older);
    }

    #[test]
    fn overlapping_older_pages_are_not_duplicated() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        c.set_history(Id(311), page(11, 20, 311), 50);
        c.prepend_older(Id(311), page(5, 12, 311), 50);
        assert_eq!(ids(&c, 311), (5..=20).collect::<Vec<_>>());
    }

    #[test]
    fn live_messages_do_not_evict_scrollback() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        c.set_history(Id(311), page(801, 1000, 311), 50);
        c.prepend_older(Id(311), page(751, 800, 311), 50);
        c.apply(CoreEvent::MessageCreate(msg(1001, 311)));
        assert_eq!(c.messages(Id(311)).unwrap().get(0).unwrap().message.id, Id(751));
        assert_eq!(c.messages(Id(311)).unwrap().len(), 251);
    }

    #[test]
    fn scrollback_never_exceeds_its_limit() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        c.set_history(Id(311), page(1_000_001, 1_000_200, 311), 50);
        let mut top = 1_000_000;
        for _ in 0..SCROLLBACK_CAP / 50 + 2 {
            c.prepend_older(Id(311), page(top - 49, top, 311), 50);
            top -= 50;
        }
        let m = c.messages(Id(311)).unwrap();
        assert_eq!(m.len(), SCROLLBACK_CAP);
        assert!(m.has_older);
        assert!(c.scrollback_full(Id(311)));
        // The newest messages are the ones kept.
        assert_eq!(m.get(m.len() - 1).unwrap().message.id, Id(1_000_200));
    }

    #[test]
    fn trimming_returns_to_the_live_cap() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        c.set_history(Id(311), page(801, 1000, 311), 50);
        c.prepend_older(Id(311), page(751, 800, 311), 50);
        c.trim_to_tail(Id(311));
        let m = c.messages(Id(311)).unwrap();
        assert_eq!(m.len(), MESSAGE_CAP);
        assert_eq!(m.get(0).unwrap().message.id, Id(801));
        assert!(m.has_older);
        // Back to normal: a live message evicts the oldest again.
        c.apply(CoreEvent::MessageCreate(msg(1001, 311)));
        assert_eq!(c.messages(Id(311)).unwrap().len(), MESSAGE_CAP);
    }

    #[test]
    fn a_window_around_a_message_replaces_what_was_held_and_leaves_the_present_out() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        c.set_history(Id(311), page(951, 1000, 311), 50);
        c.set_around(Id(311), page(401, 450, 311), 50);
        let m = c.messages(Id(311)).unwrap();
        assert_eq!((m.get(0).unwrap().message.id, m.get(m.len() - 1).unwrap().message.id), (Id(401), Id(450)));
        assert!(m.has_older && m.has_newer);
        // Live messages don't land past the gap.
        c.apply(CoreEvent::MessageCreate(msg(1001, 311)));
        assert_eq!(c.messages(Id(311)).unwrap().len(), 50);
        assert_eq!(c.newest(Id(311)), Some(Id(450)));
    }

    #[test]
    fn newer_pages_join_on_below_until_the_gap_closes() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        c.set_around(Id(311), page(401, 450, 311), 50);
        c.append_newer(Id(311), page(451, 500, 311), 50);
        assert!(c.has_newer(Id(311)), "a full page: more may follow");
        assert_eq!(c.newest(Id(311)), Some(Id(500)));
        c.append_newer(Id(311), page(501, 520, 311), 50);
        assert!(!c.has_newer(Id(311)), "a short page reaches the present");
        let m = c.messages(Id(311)).unwrap();
        assert_eq!(m.len(), 120);
        assert!((0..m.len() - 1).all(|i| m.get(i).unwrap().message.id < m.get(i + 1).unwrap().message.id), "in order");
        // And live messages are taken again.
        c.apply(CoreEvent::MessageCreate(msg(521, 311)));
        assert_eq!(c.messages(Id(311)).unwrap().len(), 121);
    }

    #[test]
    fn a_window_is_not_trimmed_like_the_bottom() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        c.set_around(Id(311), page(1, 250, 311), 250);
        c.trim_to_tail(Id(311));
        assert_eq!(c.messages(Id(311)).unwrap().len(), 250);
    }

    #[test]
    fn fetched_pages_refresh_what_is_known_about_their_authors() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        let mut old = user(7, "kai");
        old.global_name = Some("Old Name".into());
        c.users.insert(Id(7), old);
        let mut m = msg(10, 311);
        m.author = user(7, "kai");
        m.author.global_name = Some("New Name".into());
        c.set_history(Id(311), vec![m], 50);
        assert_eq!(c.user(Id(7)).unwrap().display_name(), "New Name");
    }

    fn member(user: u64, nick: Option<&str>, roles: &[u64]) -> Member {
        Member {
            user_id: Some(Id(user)),
            user: None,
            nick: nick.map(Into::into),
            avatar: None,
            roles: roles.iter().copied().map(Id).collect(),
            communication_disabled_until: None, joined_at: None,
        }
    }

    #[test]
    fn ready_stores_our_own_membership_per_guild() {
        let mut c = Cache::default();
        c.apply(CoreEvent::Ready(Box::new(Ready {
            user: user(1, "me"),
            session_id: "s".into(),
            resume_gateway_url: "u".into(), read_state: Default::default(), user_settings_proto: None, relationships: vec![], sessions: vec![], user_guild_settings: Default::default(),
            guilds: vec![Guild {
                id: Id(300), name: "g".into(), icon: None, unavailable: false, channels: vec![],
                roles: vec![Role { id: Id(301), name: "mod".into(), color: 0xff0000, position: 1, permissions: 8, hoist: true, icon: None, unicode_emoji: None, colors: None, mentionable: false }],
                owner_id: Some(Id(9)), voice_states: vec![], emojis: vec![], premium_tier: 0, banner: None, rules_channel: None,
            }],
            private_channels: vec![],
            users: vec![],
            merged_members: vec![MemberList(vec![member(1, Some("Boss"), &[301])])],
        })));
        assert_eq!(c.member(Id(300), Id(1)).unwrap().nick.as_deref(), Some("Boss"));
        assert_eq!(c.display_name(Some(Id(300)), c.me().unwrap()), "Boss");
        assert_eq!(c.display_name(None, c.me().unwrap()), "me");
        assert_eq!(c.guild(Id(300)).unwrap().roles[0].permissions, 8);
    }

    #[test]
    fn message_members_and_member_events_update_nicknames() {
        let mut c = ready_cache();
        let mut m = msg(5, 311);
        m.guild_id = Some(Id(300));
        m.member = Some(Member { user_id: None, ..member(0, Some("Zenny"), &[]) });
        c.apply(CoreEvent::MessageCreate(m.clone()));
        assert_eq!(c.display_name(Some(Id(300)), &m.author), "Zenny");

        let mut update = member(2, None, &[301]);
        update.user_id = None;
        update.user = Some(user(2, "lin"));
        c.apply(CoreEvent::MemberUpsert { guild_id: Id(300), member: update });
        assert_eq!(c.display_name(Some(Id(300)), &m.author), "lin");
        assert_eq!(c.member(Id(300), Id(2)).unwrap().roles, vec![Id(301)]);
    }

    fn role(id: u64, color: u32, position: i32) -> Role {
        Role { id: Id(id), name: format!("r{id}"), color, position, permissions: 0, hoist: false, icon: None, unicode_emoji: None, colors: None, mentionable: false }
    }

    fn cache_with_roles(roles: Vec<Role>, member_roles: &[u64]) -> Cache {
        let mut c = ready_cache();
        c.apply(CoreEvent::GuildCreate(Guild {
            id: Id(300), name: "g".into(), icon: None, unavailable: false, channels: vec![], roles, owner_id: None, voice_states: vec![], emojis: vec![], premium_tier: 0, banner: None, rules_channel: None,
        }));
        c.apply(CoreEvent::MemberUpsert { guild_id: Id(300), member: member(2, None, member_roles) });
        c
    }

    #[test]
    fn gradient_roles_give_their_stops() {
        let mut top = role(2, 0x222222, 5);
        top.colors = Some(crate::model::RoleColors { primary_color: 0x222222, secondary_color: Some(0xaaaaaa), tertiary_color: None });
        let c = cache_with_roles(vec![role(1, 0x111111, 1), top], &[1, 2]);
        assert_eq!(c.member_colors(Id(300), Id(2)), vec![0x222222, 0xaaaaaa]);
        let plain = cache_with_roles(vec![role(1, 0x111111, 1)], &[1]);
        assert_eq!(plain.member_colors(Id(300), Id(2)), vec![0x111111]);
        let json = r#"{"id":"1","color":5,"colors":{"primary_color":5,"secondary_color":6,"tertiary_color":null}}"#;
        let role: Role = serde_json::from_str(json).unwrap();
        assert_eq!(role.name_colors(), vec![5, 6]);
    }

    #[test]
    fn member_color_is_the_highest_coloured_role() {
        let c = cache_with_roles(vec![role(1, 0x111111, 1), role(2, 0x222222, 5), role(3, 0x333333, 3)], &[1, 3]);
        assert_eq!(c.member_color(Id(300), Id(2)), Some(0x333333));
        // A higher role without a colour is skipped.
        let c = cache_with_roles(vec![role(1, 0x111111, 1), role(2, 0, 9)], &[1, 2]);
        assert_eq!(c.member_color(Id(300), Id(2)), Some(0x111111));
        // Roles the member doesn't have don't count; no coloured role, no colour.
        let c = cache_with_roles(vec![role(1, 0x111111, 1), role(2, 0, 9)], &[2]);
        assert_eq!(c.member_color(Id(300), Id(2)), None);
        assert_eq!(c.member_color(Id(300), Id(77)), None, "unknown member");
        assert_eq!(c.member_color(Id(999), Id(2)), None, "unknown guild");
    }

    #[test]
    fn avatar_prefers_the_server_avatar() {
        let mut c = ready_cache();
        let mut lin = user(2, "lin");
        lin.avatar = Some("global".into());
        assert_eq!(c.avatar_url(Some(Id(300)), &lin, 80), "https://cdn.discordapp.com/avatars/2/global.png?size=80");
        let mut m = member(2, None, &[]);
        m.avatar = Some("local".into());
        c.apply(CoreEvent::MemberUpsert { guild_id: Id(300), member: m });
        assert_eq!(
            c.avatar_url(Some(Id(300)), &lin, 80),
            "https://cdn.discordapp.com/guilds/300/users/2/avatars/local.png?size=80"
        );
        // Outside the server (a DM) the global avatar is used.
        assert_eq!(c.avatar_url(None, &lin, 80), "https://cdn.discordapp.com/avatars/2/global.png?size=80");
    }

    fn list_update(id: &str, member: u64, name: &str) -> CoreEvent {
        let update = serde_json::from_value(serde_json::json!({
            "id": id, "guild_id": "300", "member_count": 1, "online_count": 1,
            "groups": [{"id": "online", "count": 1}],
            "ops": [{"op": "SYNC", "range": [0, 99], "items": [
                {"group": {"id": "online", "count": 1}},
                {"member": {"user": {"id": member.to_string(), "username": name}, "nick": "Nicky",
                    "avatar": "av", "roles": ["301"], "presence": {"status": "online"}}},
            ]}]
        }))
        .unwrap();
        CoreEvent::MemberListUpdate(Box::new(update))
    }

    #[test]
    fn member_list_fills_the_member_cache() {
        let mut c = ready_cache();
        assert!(c.member_list().is_none());
        c.subscribe_member_list(Id(300));
        c.apply(list_update("everyone", 7, "gus"));
        assert_eq!(c.member_list().unwrap().len(), 2);
        // The user and their server nickname, avatar and roles are now known everywhere.
        assert_eq!(c.user(Id(7)).unwrap().username, "gus");
        let m = c.member(Id(300), Id(7)).unwrap();
        assert_eq!((m.nick.as_deref(), m.avatar.as_deref(), m.roles.clone()), (Some("Nicky"), Some("av"), vec![Id(301)]));
    }

    fn fresh_ready(id: u64) -> CoreEvent {
        CoreEvent::Ready(Box::new(Ready {
            user: user(id, "me"),
            session_id: "s2".into(),
            resume_gateway_url: "u".into(),
            read_state: Default::default(),
            user_settings_proto: None,
            user_guild_settings: Default::default(),
            relationships: vec![],
            sessions: vec![],
            guilds: vec![],
            private_channels: vec![],
            users: vec![],
            merged_members: vec![],
        }))
    }

    #[test]
    fn upload_limits_are_discords() {
        const MB: u64 = 1024 * 1024;
        let mut c = ready_cache();
        let here = c.channels.get(&Id(311)).and_then(|ch| ch.guild_id).unwrap();
        assert_eq!(c.upload_limit(Id(311)), 10 * MB);
        c.guilds.get_mut(&here).unwrap().premium_tier = 2;
        assert_eq!(c.upload_limit(Id(311)), 50 * MB);
        c.guilds.get_mut(&here).unwrap().premium_tier = 3;
        assert_eq!(c.upload_limit(Id(311)), 100 * MB);
        c.me.as_mut().unwrap().extras.premium_type = Some(1);
        assert_eq!(c.upload_limit(Id(311)), 100 * MB, "the larger of the two");
        c.me.as_mut().unwrap().extras.premium_type = Some(2);
        assert_eq!(c.upload_limit(Id(311)), 500 * MB);
    }

    #[test]
    fn emoji_follow_the_rules_for_nitro() {
        let emoji = |id: u64, animated: bool| crate::model::GuildEmoji { id: Id(id), name: format!("e{id}"), animated, available: true, roles: vec![] };
        let mut c = ready_cache();
        let here = c.channels.get(&Id(311)).and_then(|ch| ch.guild_id).unwrap();
        c.guilds.get_mut(&here).unwrap().emojis = vec![emoji(1, false), emoji(2, true)];
        let mut other = c.guilds.get(&here).unwrap().clone();
        other.id = Id(777);
        other.emojis = vec![emoji(3, false)];
        c.guilds.insert(Id(777), other);
        c.guild_order.push(Id(777));
        let ids = |c: &Cache| c.usable_emojis(Id(311)).iter().map(|e| e.id.0).collect::<Vec<_>>();
        // No Nitro: this server's, and not the animated one.
        assert_eq!(ids(&c), [1]);
        c.me.as_mut().unwrap().extras.premium_type = Some(2);
        // Nitro, as far as the channel allows outside ones.
        let allowed = c.has_permission(Id(311), permissions::USE_EXTERNAL_EMOJIS);
        assert_eq!(ids(&c), if allowed { vec![1, 2, 3] } else { vec![1, 2] });
        // Locked to a role we don't have.
        c.guilds.get_mut(&here).unwrap().emojis[0].roles = vec![Id(424242)];
        assert!(!ids(&c).contains(&1));
    }

    #[test]
    fn coming_back_to_a_channel_brings_its_list_back_with_no_new_sync() {
        let mut c = ready_cache();
        c.subscribe_member_list_for(Id(300), Some(Id(1)));
        c.apply(list_update("a", 7, "gus"));
        c.subscribe_member_list_for(Id(300), Some(Id(2)));
        assert_eq!(c.member_list().unwrap().list_id, "a", "stays up until there's a new one");
        c.apply(list_update("b", 8, "hal"));
        assert_eq!(c.member_list().unwrap().list_id, "b");
        // Back to the first: Discord sends no SYNC, and it's there anyway.
        c.subscribe_member_list_for(Id(300), Some(Id(1)));
        let list = c.member_list().unwrap();
        assert_eq!(list.list_id, "a");
        assert!(list.find(Id(7)).is_some());
        // And forward again.
        c.subscribe_member_list_for(Id(300), Some(Id(2)));
        assert_eq!(c.member_list().unwrap().list_id, "b");
    }

    #[test]
    fn a_list_stepped_away_from_keeps_up_with_changes() {
        let mut c = ready_cache();
        c.subscribe_member_list_for(Id(300), Some(Id(1)));
        c.apply(list_update("a", 7, "gus"));
        c.subscribe_member_list_for(Id(300), Some(Id(2)));
        c.apply(list_update("b", 8, "hal"));
        c.apply(list_update("a", 9, "ida"));
        assert_eq!(c.member_list().unwrap().list_id, "b");
        c.subscribe_member_list_for(Id(300), Some(Id(1)));
        assert!(c.member_list().unwrap().find(Id(9)).is_some());
    }

    #[test]
    fn a_reconnect_keeps_whats_on_screen_until_its_replaced() {
        let mut c = ready_cache();
        c.subscribe_member_list(Id(300));
        c.apply(list_update("everyone", 7, "gus"));
        let rev = c.sidebar_rev;
        c.apply(fresh_ready(1));
        assert!(c.sidebar_rev > rev, "a reused revision wouldn't redraw the sidebar");
        assert!(c.user(Id(7)).is_some());
        assert!(c.member_list().unwrap().find(Id(7)).is_some());
        // The new session's SYNC starts over, whichever list it's for.
        c.apply(list_update("everyone", 8, "hal"));
        let list = c.member_list().unwrap();
        assert!(list.find(Id(8)).is_some() && list.find(Id(7)).is_none());
    }

    #[test]
    fn another_account_keeps_nothing() {
        let mut c = ready_cache();
        c.subscribe_member_list(Id(300));
        c.apply(list_update("everyone", 7, "gus"));
        c.apply(fresh_ready(2));
        assert!(c.member_list().is_none());
        assert!(c.user(Id(7)).is_none());
    }

    #[test]
    fn resubscribing_takes_the_next_lists_first_sync() {
        let mut c = ready_cache();
        c.subscribe_member_list(Id(300));
        c.apply(list_update("everyone", 7, "gus"));
        // While one list is held another list's updates are leftovers.
        c.apply(list_update("hash-of-perms", 8, "hal"));
        assert_eq!(c.member_list().unwrap().list_id, "everyone");
        assert!(c.user(Id(8)).is_none(), "an ignored update teaches nothing");
        // Subscribing for another channel keeps the list up, since channels sharing
        // a list get no new SYNC; the next SYNC, if any, replaces it.
        c.subscribe_member_list(Id(300));
        assert_eq!(c.member_list().unwrap().list_id, "everyone");
        c.apply(list_update("hash-of-perms", 8, "hal"));
        let list = c.member_list().unwrap();
        assert_eq!(list.list_id, "hash-of-perms");
        assert!(list.find(Id(7)).is_none(), "the old list's rows are gone");
        assert!(list.find(Id(8)).is_some());
    }

    #[test]
    fn updates_are_ignored_without_a_subscription() {
        let mut c = ready_cache();
        c.apply(list_update("everyone", 7, "gus"));
        assert!(c.member_list().is_none());
        c.subscribe_member_list(Id(999));
        c.apply(list_update("everyone", 7, "gus"));
        assert!(c.member_list().is_none(), "another guild's");
        c.subscribe_member_list(Id(300));
        c.apply(list_update("everyone", 7, "gus"));
        assert!(c.member_list().is_some());
        c.unsubscribe_member_list();
        assert!(c.member_list().is_none());
        c.apply(list_update("everyone", 7, "gus"));
        assert!(c.member_list().is_none());
    }

    #[test]
    fn list_updates_keep_our_timeout() {
        let mut c = ready_cache();
        let mut timed = member(7, Some("old"), &[]);
        timed.communication_disabled_until = Some("2999-01-01T00:00:00+00:00".into());
        c.apply(CoreEvent::MemberUpsert { guild_id: Id(300), member: timed });
        c.subscribe_member_list(Id(300));
        c.apply(list_update("everyone", 7, "gus"));
        let m = c.member(Id(300), Id(7)).unwrap();
        assert_eq!(m.nick.as_deref(), Some("Nicky"), "nick comes from the list");
        assert_eq!(m.communication_disabled_until.as_deref(), Some("2999-01-01T00:00:00+00:00"));
    }

    #[test]
    fn a_fresh_ready_keeps_the_subscription() {
        let mut c = ready_cache();
        c.subscribe_member_list(Id(300));
        c.apply(CoreEvent::Ready(Box::new(Ready {
            user: user(1, "me"), session_id: "s".into(), resume_gateway_url: "u".into(), read_state: Default::default(), user_settings_proto: None, relationships: vec![], sessions: vec![], user_guild_settings: Default::default(),
            guilds: vec![], private_channels: vec![], users: vec![], merged_members: vec![],
        })));
        c.apply(list_update("everyone", 7, "gus"));
        assert!(c.member_list().is_some());
    }

    #[test]
    fn forgetting_rows_empties_them() {
        let mut c = ready_cache();
        c.subscribe_member_list(Id(300));
        c.apply(list_update("everyone", 7, "gus"));
        c.forget_member_rows(1, 1);
        assert!(c.member_list().unwrap().find(Id(7)).is_none());
    }

    #[test]
    fn role_events_edit_the_guilds_roles() {
        let mut c = ready_cache();
        let role = Role { id: Id(301), name: "mod".into(), color: 0, position: 1, permissions: 8, hoist: false, icon: None, unicode_emoji: None, colors: None, mentionable: false };
        c.apply(CoreEvent::RoleUpsert { guild_id: Id(300), role: role.clone() });
        c.apply(CoreEvent::RoleUpsert { guild_id: Id(300), role: Role { permissions: 16, ..role } });
        assert_eq!(c.guild(Id(300)).unwrap().roles.len(), 1);
        assert_eq!(c.guild(Id(300)).unwrap().roles[0].permissions, 16);
        c.apply(CoreEvent::RoleDelete { guild_id: Id(300), role_id: Id(301) });
        assert!(c.guild(Id(300)).unwrap().roles.is_empty());
    }

    #[test]
    fn stats_count_what_is_held() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        c.apply(CoreEvent::MessageCreate(msg(5, 311)));
        c.apply(CoreEvent::MessageCreate(msg(6, 311)));
        assert_eq!(c.stats(), CacheStats { guilds: 1, channels: 4, users: 2, messages: 2 });
    }

    #[test]
    fn ready_populates() {
        let c = ready_cache();
        assert_eq!(c.me().unwrap().username, "me");
        assert_eq!(c.user(Id(2)).unwrap().username, "lin");
        assert_eq!(c.guilds().count(), 1);
        assert!(c.guild(Id(300)).unwrap().channels.is_empty());
        assert_eq!(c.guild_channels(Id(300)).len(), 1);
        let dms: Vec<u64> = c.private_channels().iter().map(|ch| ch.id.0).collect();
        assert_eq!(dms, [401, 400, 402]);
    }

    #[test]
    fn messages_only_kept_for_open_channels() {
        let mut c = ready_cache();
        c.apply(CoreEvent::MessageCreate(msg(50, 311)));
        assert!(c.messages(Id(311)).is_none());
        assert_eq!(c.channel(Id(311)).unwrap().last_message_id, Some(Id(50)));
        c.open_messages(Id(311));
        c.apply(CoreEvent::MessageCreate(msg(51, 311)));
        assert_eq!(ids(&c, 311), [51]);
    }

    #[test]
    fn ring_buffer_caps_at_200() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        for i in 1..=250 {
            c.apply(CoreEvent::MessageCreate(msg(i, 311)));
        }
        let m = c.messages(Id(311)).unwrap();
        assert_eq!(m.len(), MESSAGE_CAP);
        assert_eq!(m.get(0).unwrap().message.id, Id(51));
        assert!(m.has_older);
    }

    #[test]
    fn duplicate_create_is_ignored() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        c.apply(CoreEvent::MessageCreate(msg(5, 311)));
        c.apply(CoreEvent::MessageCreate(msg(5, 311)));
        assert_eq!(ids(&c, 311), [5]);
    }

    #[test]
    fn pending_is_replaced_by_its_echo() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        let mut pending = msg(1_000, 311);
        pending.nonce = Some("n1".into());
        c.add_pending(pending);
        assert_eq!(c.messages(Id(311)).unwrap().get(0).unwrap().state, SendState::Pending);

        let mut echo = msg(60, 311);
        echo.nonce = Some("n1".into());
        c.apply(CoreEvent::MessageCreate(echo));
        let m = c.messages(Id(311)).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m.get(0).unwrap().message.id, Id(60));
        assert_eq!(m.get(0).unwrap().state, SendState::Sent);
    }

    #[test]
    fn mark_failed_keeps_the_message() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        let mut pending = msg(1_000, 311);
        pending.nonce = Some("n2".into());
        c.add_pending(pending);
        c.mark_failed(Id(311), "n2");
        assert_eq!(c.messages(Id(311)).unwrap().get(0).unwrap().state, SendState::Failed);
    }

    #[test]
    fn history_merges_live_and_pending() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        c.apply(CoreEvent::MessageCreate(msg(30, 311))); // arrived while history loaded
        let mut pending = msg(1_000, 311);
        pending.nonce = Some("n3".into());
        c.add_pending(pending);
        // REST returns newest first, and already contains 20 but not 30.
        c.set_history(Id(311), vec![msg(20, 311), msg(10, 311)], 50);
        assert_eq!(ids(&c, 311), [10, 20, 30, 1_000]);
        assert!(!c.messages(Id(311)).unwrap().has_older);
        c.set_history(Id(311), (1..=50).rev().map(|i| msg(i, 311)).collect(), 50);
        assert!(c.messages(Id(311)).unwrap().has_older);
    }

    /// An edit of message 5 in channel 311.
    fn edit(e: MessageEdit) -> CoreEvent {
        CoreEvent::MessageUpdate(Box::new(MessageEdit { id: Id(5), channel_id: Id(311), ..e }))
    }

    #[test]
    fn update_and_delete() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        c.apply(CoreEvent::MessageCreate(msg(5, 311)));
        c.apply(edit(MessageEdit { content: Some("edited".into()), edited_timestamp: Some("t".into()), ..Default::default() }));
        let m = &c.messages(Id(311)).unwrap().get(0).unwrap().message;
        assert_eq!(m.content, "edited");
        assert_eq!(m.edited_timestamp.as_deref(), Some("t"));
        c.apply(edit(MessageEdit::default()));
        assert_eq!(c.messages(Id(311)).unwrap().get(0).unwrap().message.content, "edited");
        let embed = crate::rich::Embed { title: Some("t".into()), ..Default::default() };
        c.apply(edit(MessageEdit { embeds: Some(vec![embed]), ..Default::default() }));
        assert_eq!(c.messages(Id(311)).unwrap().get(0).unwrap().message.embeds.len(), 1);
        // Components V2 edits replace layout, attachments and flags too.
        c.apply(edit(MessageEdit {
            components: Some(vec![crate::rich::Component::ActionRow(vec![])]),
            attachments: Some(vec![crate::rich::Attachment::default()]),
            flags: Some(32768),
            ..Default::default()
        }));
        let m = &c.messages(Id(311)).unwrap().get(0).unwrap().message;
        assert_eq!((m.components.len(), m.attachments.len(), m.flags), (1, 1, 32768));
        // Each edit gives the row a new revision, so views re-measure it.
        let before = c.messages(Id(311)).unwrap().get(0).unwrap().revision;
        c.apply(edit(MessageEdit { content: Some("again".into()), ..Default::default() }));
        assert_ne!(c.messages(Id(311)).unwrap().get(0).unwrap().revision, before);
        c.apply(CoreEvent::MessageDelete { id: Id(5), channel_id: Id(311) });
        assert!(c.messages(Id(311)).unwrap().is_empty());
    }

    #[test]
    fn a_purge_removes_every_message_named() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        c.set_history(Id(311), page(1, 5, 311), 50);
        c.apply(CoreEvent::MessageDeleteBulk { ids: vec![Id(2), Id(4), Id(99)], channel_id: Id(311) });
        assert_eq!(ids(&c, 311), vec![1, 3, 5]);
    }

    fn emoji(name: &str) -> ReactionEmoji {
        ReactionEmoji { id: None, name: Some(name.into()), animated: false }
    }

    fn reactions(c: &Cache) -> Vec<(String, u32, bool)> {
        let m = &c.messages(Id(311)).unwrap().get(0).unwrap().message;
        m.reactions.iter().map(|r| (r.emoji.name.clone().unwrap(), r.count, r.me)).collect()
    }

    fn react(user: u64, name: &str, add: bool) -> CoreEvent {
        let (channel_id, message_id, user_id, emoji) = (Id(311), Id(5), Id(user), emoji(name));
        if add {
            CoreEvent::ReactionAdd { channel_id, message_id, user_id, emoji, burst: false }
        } else {
            CoreEvent::ReactionRemove { channel_id, message_id, user_id, emoji, burst: false }
        }
    }

    fn cache_with_message() -> Cache {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        c.apply(CoreEvent::MessageCreate(msg(5, 311)));
        c
    }

    #[test]
    fn reaction_events_update_counts() {
        let mut c = cache_with_message();
        c.apply(react(2, "a", true));
        c.apply(react(3, "a", true));
        c.apply(react(3, "b", true));
        assert_eq!(reactions(&c), [("a".into(), 2, false), ("b".into(), 1, false)]);
        // Our own reaction from another device sets `me`.
        c.apply(react(1, "a", true));
        assert_eq!(reactions(&c)[0], ("a".into(), 3, true));
        c.apply(react(1, "a", false));
        c.apply(react(2, "a", false));
        assert_eq!(reactions(&c)[0], ("a".into(), 1, false));
        c.apply(react(3, "a", false));
        assert_eq!(reactions(&c), [("b".into(), 1, false)]);
        let before = c.messages(Id(311)).unwrap().get(0).unwrap().revision;
        c.apply(CoreEvent::ReactionRemoveEmoji { channel_id: Id(311), message_id: Id(5), emoji: emoji("b") });
        assert!(reactions(&c).is_empty());
        assert_ne!(c.messages(Id(311)).unwrap().get(0).unwrap().revision, before);
        c.apply(react(2, "a", true));
        c.apply(CoreEvent::ReactionRemoveAll { channel_id: Id(311), message_id: Id(5) });
        assert!(reactions(&c).is_empty());
        // Unknown messages are ignored.
        c.apply(CoreEvent::ReactionAdd { channel_id: Id(311), message_id: Id(99), user_id: Id(2), emoji: emoji("a"), burst: false });
    }

    #[test]
    fn optimistic_reaction_is_not_counted_twice_by_the_echo() {
        let mut c = cache_with_message();
        c.apply(react(2, "a", true));
        c.set_my_reaction(Id(311), Id(5), &emoji("a"), true);
        assert_eq!(reactions(&c), [("a".into(), 2, true)]);
        c.apply(react(1, "a", true)); // the gateway echo
        assert_eq!(reactions(&c), [("a".into(), 2, true)]);
        c.set_my_reaction(Id(311), Id(5), &emoji("a"), false);
        assert_eq!(reactions(&c), [("a".into(), 1, false)]);
        c.apply(react(1, "a", false)); // echo of the removal
        assert_eq!(reactions(&c), [("a".into(), 1, false)]);
        // A new emoji, then removed again, leaves nothing behind.
        c.set_my_reaction(Id(311), Id(5), &emoji("z"), true);
        c.apply(react(1, "z", true));
        c.set_my_reaction(Id(311), Id(5), &emoji("z"), false);
        c.apply(react(1, "z", false));
        assert_eq!(reactions(&c), [("a".into(), 1, false)]);
    }

    fn reaction_state(c: &Cache) -> Vec<(String, u32, bool, u32, bool)> {
        let m = &c.messages(Id(311)).unwrap().get(0).unwrap().message;
        m.reactions.iter().map(|r| (r.emoji.name.clone().unwrap(), r.count, r.me, r.burst_count, r.me_burst)).collect()
    }

    fn burst(user: u64, name: &str, add: bool) -> CoreEvent {
        let (channel_id, message_id, user_id, emoji) = (Id(311), Id(5), Id(user), emoji(name));
        if add {
            CoreEvent::ReactionAdd { channel_id, message_id, user_id, emoji, burst: true }
        } else {
            CoreEvent::ReactionRemove { channel_id, message_id, user_id, emoji, burst: true }
        }
    }

    #[test]
    fn burst_events_never_touch_me_or_the_normal_toggle() {
        let mut c = cache_with_message();
        c.apply(burst(1, "a", true));
        assert_eq!(reaction_state(&c), [("a".into(), 1, false, 1, true)]);
        // A normal reaction of ours is independent of the burst one.
        assert!(c.begin_my_reaction(Id(311), Id(5), &emoji("a"), true));
        assert_eq!(reaction_state(&c), [("a".into(), 2, true, 1, true)]);
        c.settle_my_reaction(Id(311), Id(5), &emoji("a"), true);
        c.apply(react(1, "a", true)); // its echo
        c.apply(burst(1, "a", false));
        assert_eq!(reaction_state(&c), [("a".into(), 1, true, 0, false)]);
        // Removing a normal reaction leaves our burst alone.
        c.apply(burst(1, "a", true));
        assert!(c.begin_my_reaction(Id(311), Id(5), &emoji("a"), false));
        assert_eq!(reaction_state(&c), [("a".into(), 1, false, 1, true)]);
        // Someone else's burst changes no flag of ours.
        c.apply(burst(2, "a", true));
        c.apply(burst(2, "a", false));
        assert_eq!(reaction_state(&c), [("a".into(), 1, false, 1, true)]);
    }

    #[test]
    fn burst_fields_decode() {
        let m: Message = serde_json::from_value(serde_json::json!({
            "id": "1", "channel_id": "2", "author": {"id": "3", "username": "u"},
            "content": "", "timestamp": "t",
            "reactions": [{"emoji": {"id": null, "name": "x"}, "count": 3, "me": false, "me_burst": true, "burst_count": 2}]
        }))
        .unwrap();
        assert!(m.reactions[0].me_burst && !m.reactions[0].me);
        assert_eq!(m.reactions[0].burst_count, 2);
    }

    #[test]
    fn rapid_toggles_ignore_echoes_in_any_order() {
        let mut c = cache_with_message();
        let (ch, m, e) = (Id(311), Id(5), emoji("a"));
        c.apply(react(2, "a", true));
        // on: the echo arrives before the answer
        assert!(c.begin_my_reaction(ch, m, &e, true));
        assert!(c.reaction_in_flight(ch, m, &e));
        assert!(!c.begin_my_reaction(ch, m, &e, false), "clicks wait for the answer");
        c.apply(react(1, "a", true));
        c.settle_my_reaction(ch, m, &e, true);
        assert_eq!(reactions(&c), [("a".into(), 2, true)]);
        // off: the echo arrives after the answer, and so does a late one
        // of the "on" that was already settled.
        assert!(c.begin_my_reaction(ch, m, &e, false));
        c.settle_my_reaction(ch, m, &e, true);
        assert_eq!(reactions(&c), [("a".into(), 1, false)]);
        // on again, while the earlier echoes are still outstanding.
        assert!(c.begin_my_reaction(ch, m, &e, true));
        c.apply(react(1, "a", false)); // late echo of the off
        assert_eq!(reactions(&c), [("a".into(), 2, true)]);
        c.settle_my_reaction(ch, m, &e, true);
        assert_eq!(reactions(&c), [("a".into(), 2, true)]);
        c.apply(react(1, "a", true)); // echo of the second on
        assert_eq!(reactions(&c), [("a".into(), 2, true)]);
        // Nothing is owed any more: another device's removal counts.
        c.apply(react(1, "a", false));
        assert_eq!(reactions(&c), [("a".into(), 1, false)]);
    }

    #[test]
    fn a_late_on_echo_after_an_off_settles_is_ignored() {
        let mut c = cache_with_message();
        let (ch, m, e) = (Id(311), Id(5), emoji("a"));
        assert!(c.begin_my_reaction(ch, m, &e, true));
        c.settle_my_reaction(ch, m, &e, true);
        assert!(c.begin_my_reaction(ch, m, &e, false));
        c.settle_my_reaction(ch, m, &e, true);
        assert!(reactions(&c).is_empty());
        c.apply(react(1, "a", true)); // late echo of the first on
        assert!(reactions(&c).is_empty());
        c.apply(react(1, "a", false));
        assert!(reactions(&c).is_empty());
    }

    #[test]
    fn a_failed_reaction_restores_the_old_list_in_order() {
        let mut c = cache_with_message();
        let (ch, m) = (Id(311), Id(5));
        c.apply(react(2, "a", true));
        c.apply(react(2, "b", true));
        c.apply(react(1, "b", true));
        c.apply(react(2, "c", true));
        let before = reactions(&c);
        // Removing ours empties "b": a failure puts it back where it was.
        assert!(c.begin_my_reaction(ch, m, &emoji("b"), false));
        assert_eq!(reactions(&c).len(), 3);
        c.apply(react(2, "d", true)); // someone else reacts meanwhile
        c.settle_my_reaction(ch, m, &emoji("b"), false);
        let mut expected = before.clone();
        expected.push(("d".into(), 1, false));
        assert_eq!(reactions(&c), expected);
        // Adding a new one and failing removes it again.
        assert!(c.begin_my_reaction(ch, m, &emoji("z"), true));
        c.settle_my_reaction(ch, m, &emoji("z"), false);
        assert_eq!(reactions(&c), expected);
        assert!(!c.reaction_in_flight(ch, m, &emoji("z")));
        // After a failure the pill can be clicked again.
        assert!(c.begin_my_reaction(ch, m, &emoji("z"), true));
    }

    #[test]
    fn edits_carry_mentions() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        c.apply(CoreEvent::MessageCreate(msg(5, 311)));
        c.apply(edit(MessageEdit {
            mentions: Some(vec![user(1, "me")]),
            mention_roles: Some(vec![Id(7)]),
            mention_everyone: Some(true),
            ..Default::default()
        }));
        let m = &c.messages(Id(311)).unwrap().get(0).unwrap().message;
        assert_eq!((m.mentions.len(), m.mention_roles.clone(), m.mention_everyone), (1, vec![Id(7)], true));
        c.apply(edit(MessageEdit { content: Some("x".into()), ..Default::default() }));
        let m = &c.messages(Id(311)).unwrap().get(0).unwrap().message;
        assert_eq!((m.mentions.len(), m.mention_everyone), (1, true), "absent fields stay");
        let parsed: MessageEdit = serde_json::from_value(serde_json::json!({
            "id": "5", "channel_id": "311", "mentions": [{"id": "1", "username": "me"}], "mention_roles": ["7"], "mention_everyone": false
        }))
        .unwrap();
        assert_eq!((parsed.mentions.unwrap().len(), parsed.mention_roles, parsed.mention_everyone), (1, Some(vec![Id(7)]), Some(false)));
    }

    fn voice(user: u64, channel: Option<u64>) -> VoiceState {
        VoiceState { session_id: None,
            guild_id: None, user_id: Id(user), channel_id: channel.map(Id), self_mute: false, self_deaf: false,
            self_video: false, self_stream: false, mute: false, deaf: false,
        }
    }

    fn in_channel(c: &Cache, ch: u64) -> Vec<u64> {
        let mut v: Vec<u64> = c.voice_participants(Id(ch)).iter().map(|s| s.user_id.0).collect();
        v.sort();
        v
    }

    #[test]
    fn voice_states_are_indexed_by_channel() {
        let mut c = ready_cache();
        let update = |user, channel| CoreEvent::VoiceStateUpdate { guild_id: Id(300), state: voice(user, channel), member: None };
        c.apply(update(2, Some(20)));
        c.apply(update(3, Some(20)));
        c.apply(update(4, Some(21)));
        assert_eq!((in_channel(&c, 20), in_channel(&c, 21)), (vec![2, 3], vec![4]));
        c.apply(update(3, Some(21))); // moved
        assert_eq!((in_channel(&c, 20), in_channel(&c, 21)), (vec![2], vec![3, 4]));
        c.apply(update(2, None)); // left
        assert!(in_channel(&c, 20).is_empty());
        // A deleted channel drops its participants.
        c.apply(CoreEvent::ChannelDelete { id: Id(21) });
        assert!(in_channel(&c, 21).is_empty());
        // GUILD_CREATE replaces the server's states; GUILD_DELETE clears them.
        c.apply(update(5, Some(20)));
        let g = Guild { id: Id(300), name: "g".into(), icon: None, unavailable: false, channels: vec![], roles: vec![], owner_id: None, emojis: vec![], premium_tier: 0, banner: None, rules_channel: None, voice_states: vec![voice(6, Some(22))] };
        c.apply(CoreEvent::GuildCreate(g.clone()));
        assert_eq!((in_channel(&c, 20), in_channel(&c, 22)), (vec![], vec![6]));
        c.apply(CoreEvent::GuildDelete { id: Id(300) });
        assert!(in_channel(&c, 22).is_empty());
        // READY keeps the rows up (a reconnect shouldn't show); the snapshot after it replaces them.
        c.apply(update(7, Some(20)));
        c.apply(CoreEvent::Ready(Box::new(Ready {
            user: user(1, "me"), session_id: "s".into(), resume_gateway_url: "u".into(), read_state: Default::default(), user_settings_proto: None, relationships: vec![], sessions: vec![], user_guild_settings: Default::default(),
            guilds: vec![], private_channels: vec![], users: vec![], merged_members: vec![],
        })));
        assert_eq!(in_channel(&c, 20), vec![7]);
        c.apply(CoreEvent::VoiceSnapshot(vec![(Id(300), vec![])], vec![]));
        assert!(in_channel(&c, 20).is_empty());
    }

    #[test]
    fn the_sidebar_revision_moves_only_for_what_it_shows() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        let rev = |c: &Cache| c.sidebar_revision();
        let quiet = |c: &mut Cache, e: CoreEvent| {
            let before = rev(c);
            c.apply(e);
            assert_eq!(rev(c), before);
        };
        let mut m = msg(5, 311);
        m.guild_id = Some(Id(300));
        quiet(&mut c, CoreEvent::MessageCreate(m));
        quiet(&mut c, edit(MessageEdit { content: Some("x".into()), ..Default::default() }));
        quiet(&mut c, react(2, "a", true));
        quiet(&mut c, CoreEvent::MemberUpsert { guild_id: Id(300), member: Member { user: Some(user(9, "x")), ..member(9, None, &[]) } });
        quiet(&mut c, CoreEvent::MessageDelete { id: Id(5), channel_id: Id(311) });

        let loud = |c: &mut Cache, e: CoreEvent| {
            let before = rev(c);
            c.apply(e);
            assert_ne!(rev(c), before);
        };
        loud(&mut c, CoreEvent::ChannelUpsert(channel(312, Some(300), ChannelKind::Text, None)));
        loud(&mut c, CoreEvent::ChannelDelete { id: Id(312) });
        loud(&mut c, CoreEvent::VoiceStateUpdate { guild_id: Id(300), state: voice(2, Some(20)), member: None });
        loud(&mut c, CoreEvent::RoleUpsert { guild_id: Id(300), role: role(5, 0, 1) });
        loud(&mut c, CoreEvent::RoleDelete { guild_id: Id(300), role_id: Id(5) });
        // Our own membership changes permissions; so does a voice member's nickname.
        loud(&mut c, CoreEvent::MemberUpsert { guild_id: Id(300), member: member(1, Some("me!"), &[]) });
        loud(&mut c, CoreEvent::MemberUpsert { guild_id: Id(300), member: member(2, Some("Zed"), &[]) });
        // A new DM message reorders the DM list.
        loud(&mut c, CoreEvent::MessageCreate({ let mut d = msg(900, 400); d.guild_id = None; d }));
        loud(&mut c, CoreEvent::GuildCreate(Guild { id: Id(301), name: "n".into(), icon: None, unavailable: false, channels: vec![], roles: vec![], owner_id: None, emojis: vec![], premium_tier: 0, banner: None, rules_channel: None, voice_states: vec![] }));
        loud(&mut c, CoreEvent::GuildDelete { id: Id(301) });
        loud(&mut c, CoreEvent::Ready(Box::new(Ready {
            user: user(1, "me"), session_id: "s".into(), resume_gateway_url: "u".into(), read_state: Default::default(), user_settings_proto: None, relationships: vec![], sessions: vec![], user_guild_settings: Default::default(),
            guilds: vec![], private_channels: vec![], users: vec![], merged_members: vec![],
        })));
    }

    #[test]
    fn guild_delete_removes_channels_and_messages() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        c.apply(CoreEvent::GuildDelete { id: Id(300) });
        assert_eq!(c.guilds().count(), 0);
        assert!(c.channel(Id(311)).is_none());
        assert!(c.messages(Id(311)).is_none());
    }

    #[test]
    fn guild_create_replaces_in_place() {
        let mut c = ready_cache();
        c.apply(CoreEvent::GuildCreate(Guild { id: Id(301), name: "second".into(), icon: None, unavailable: false, channels: vec![], roles: vec![], owner_id: None, emojis: vec![], premium_tier: 0, banner: None, rules_channel: None, voice_states: vec![] }));
        c.apply(CoreEvent::GuildCreate(Guild {
            id: Id(300), name: "renamed".into(), icon: None, unavailable: false,
            channels: vec![channel(312, Some(300), ChannelKind::Text, None)],
            roles: vec![], owner_id: None, voice_states: vec![], emojis: vec![], premium_tier: 0, banner: None, rules_channel: None,
        }));
        let names: Vec<&str> = c.guilds().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["renamed", "second"]);
        assert!(c.channel(Id(311)).is_none(), "old channel list replaced");
        assert!(c.channel(Id(312)).is_some());
    }

    #[test]
    fn dm_create_adds_recipient_users() {
        let mut c = ready_cache();
        let mut dm = channel(403, None, ChannelKind::Dm, None);
        dm.recipients = vec![user(7, "new")];
        dm.recipient_ids = vec![Id(7)];
        c.apply(CoreEvent::ChannelUpsert(dm));
        assert_eq!(c.user(Id(7)).unwrap().username, "new");
        c.apply(CoreEvent::ChannelDelete { id: Id(403) });
        assert!(c.channel(Id(403)).is_none());
    }

    #[test]
    fn echo_already_in_history_is_not_duplicated() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        let mut pending = msg(1_000, 311);
        pending.nonce = Some("n4".into());
        c.add_pending(pending);
        let mut sent = msg(60, 311);
        sent.nonce = Some("n4".into());
        c.set_history(Id(311), vec![sent.clone()], 50);
        c.apply(CoreEvent::MessageCreate(sent));
        assert_eq!(ids(&c, 311), [60]);
        assert_eq!(c.messages(Id(311)).unwrap().get(0).unwrap().state, SendState::Sent);
    }

    #[test]
    fn ready_keeps_open_buffers() {
        let mut c = ready_cache();
        c.open_messages(Id(311));
        c.apply(CoreEvent::MessageCreate(msg(5, 311)));
        c.apply(CoreEvent::Ready(Box::new(Ready {
            user: user(1, "me"),
            session_id: "s2".into(),
            resume_gateway_url: "u".into(), read_state: Default::default(), user_settings_proto: None, relationships: vec![], sessions: vec![], user_guild_settings: Default::default(),
            guilds: vec![Guild {
                id: Id(300), name: "g".into(), icon: None, unavailable: false,
                channels: vec![channel(311, Some(300), ChannelKind::Text, None)],
                roles: vec![], owner_id: None, voice_states: vec![], emojis: vec![], premium_tier: 0, banner: None, rules_channel: None,
            }],
            private_channels: vec![],
            users: vec![],
            merged_members: vec![],
        })));
        assert_eq!(ids(&c, 311), [5]);
        c.apply(CoreEvent::MessageCreate(msg(6, 311)));
        assert_eq!(ids(&c, 311), [5, 6]);
    }

    /// We (user 1) are in guild 300 with `roles`; @everyone may view and
    /// send. Channel 311 denies sending to role 301.
    fn permission_cache(roles: &[u64], timeout: Option<&str>) -> Cache {
        use crate::model::{OverwriteKind, PermissionOverwrite};
        let mut locked = channel(311, Some(300), ChannelKind::Text, None);
        locked.permission_overwrites =
            vec![PermissionOverwrite { id: Id(301), kind: OverwriteKind::Role, allow: 0, deny: permissions::SEND_MESSAGES }];
        let role = |id: u64, p: u64| Role { id: Id(id), name: String::new(), color: 0, position: 0, permissions: p, hoist: false, icon: None, unicode_emoji: None, colors: None, mentionable: false };
        let mut c = Cache::default();
        c.apply(CoreEvent::Ready(Box::new(Ready {
            user: user(1, "me"),
            session_id: "s".into(),
            resume_gateway_url: "u".into(), read_state: Default::default(), user_settings_proto: None, relationships: vec![], sessions: vec![], user_guild_settings: Default::default(),
            guilds: vec![Guild {
                id: Id(300), name: "g".into(), icon: None, unavailable: false,
                channels: vec![locked],
                roles: vec![role(300, permissions::VIEW_CHANNEL | permissions::SEND_MESSAGES), role(301, 0)],
                owner_id: Some(Id(9)), voice_states: vec![], emojis: vec![], premium_tier: 0, banner: None, rules_channel: None,
            }],
            private_channels: vec![channel(400, None, ChannelKind::Dm, None)],
            users: vec![],
            merged_members: vec![MemberList(vec![Member {
                user_id: Some(Id(1)), user: None, nick: None, avatar: None,
                roles: roles.iter().map(|r| Id(*r)).collect(),
                communication_disabled_until: timeout.map(String::from),
                joined_at: None,
            }])],
        })));
        c
    }

    #[test]
    fn my_permissions_follow_roles_and_overwrites() {
        let c = permission_cache(&[], None);
        assert!(c.can_view(Id(311)) && c.can_send(Id(311)));
        let c = permission_cache(&[301], None);
        assert!(c.can_view(Id(311)) && !c.can_send(Id(311)));
    }

    #[test]
    fn my_permissions_update_with_my_member_event() {
        let mut c = permission_cache(&[], None);
        let member = Member {
            user_id: None, user: Some(user(1, "me")), nick: None, avatar: None, roles: vec![Id(301)],
            communication_disabled_until: None, joined_at: None,
        };
        c.apply(CoreEvent::MemberUpsert { guild_id: Id(300), member });
        assert!(!c.can_send(Id(311)));
    }

    #[test]
    fn permissions_are_unknown_without_data_and_open_in_dms() {
        let c = ready_cache();
        // Ready had no merged_members, so our roles are unknown.
        assert_eq!(c.my_permissions(Id(311)), None);
        assert!(c.can_view(Id(311)) && c.can_send(Id(311)));
        assert_eq!(c.my_permissions(Id(404)), None);
        assert_eq!(permission_cache(&[], None).my_permissions(Id(400)), Some(permissions::ALL));
        assert_eq!(Cache::default().my_permissions(Id(400)), None);
    }

    #[test]
    fn timeouts_count_only_while_in_the_future() {
        let future = "2999-01-01T00:00:00.000000+00:00";
        assert_eq!(permission_cache(&[], Some(future)).timed_out_until(Id(300)).as_deref(), Some(future));
        assert_eq!(permission_cache(&[], Some("2001-01-01T00:00:00+00:00")).timed_out_until(Id(300)), None);
        assert_eq!(permission_cache(&[], Some("garbage")).timed_out_until(Id(300)), None);
        assert_eq!(permission_cache(&[], None).timed_out_until(Id(300)), None);
    }

    fn vs(user: u64, channel: Option<u64>) -> VoiceState {
        VoiceState { session_id: None,
            guild_id: None, user_id: Id(user), channel_id: channel.map(Id), self_mute: false, self_deaf: false,
            self_video: false, self_stream: false, mute: false, deaf: false,
        }
    }

    #[test]
    fn voice_states_follow_joins_moves_and_leaves() {
        let mut c = Cache::default();
        c.apply(CoreEvent::GuildCreate(Guild {
            id: Id(300), name: "g".into(), icon: None, unavailable: false, channels: vec![], roles: vec![], owner_id: None, emojis: vec![], premium_tier: 0, banner: None, rules_channel: None,
            voice_states: vec![vs(5, Some(20)), vs(6, Some(20)), vs(7, None)],
        }));
        let ids = |c: &Cache, ch: u64| {
            let mut v: Vec<u64> = c.voice_participants(Id(ch)).iter().map(|s| s.user_id.0).collect();
            v.sort();
            v
        };
        assert_eq!(ids(&c, 20), [5, 6]);
        // Moves to another channel, with a member record that names them.
        let member = Member {
            user_id: None, user: Some(user(5, "kai")), nick: Some("K".into()), avatar: None, roles: vec![],
            communication_disabled_until: None, joined_at: None,
        };
        c.apply(CoreEvent::VoiceStateUpdate { guild_id: Id(300), state: vs(5, Some(21)), member: Some(member) });
        assert_eq!((ids(&c, 20), ids(&c, 21)), (vec![6], vec![5]));
        assert_eq!(c.display_name(Some(Id(300)), c.user(Id(5)).unwrap()), "K");
        // Leaving removes them; the same user id in another guild is separate.
        c.apply(CoreEvent::VoiceStateUpdate { guild_id: Id(301), state: vs(6, Some(20)), member: None });
        c.apply(CoreEvent::VoiceStateUpdate { guild_id: Id(300), state: vs(6, None), member: None });
        assert_eq!(ids(&c, 20), [6]);
        c.apply(CoreEvent::GuildDelete { id: Id(301) });
        assert!(ids(&c, 20).is_empty());
    }
}
