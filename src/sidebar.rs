//! Pure helpers for the channel column: which rows a (possibly collapsed)
//! category shows, who sits in a voice channel, and the DM list's second line.

use std::collections::HashSet;

use cord_core::Id;
use cord_core::member_list::{PresenceLine, Status};
use cord_core::cache::Cache;
use cord_core::cdn;
use cord_core::model::{Channel, ChannelKind};

use crate::nav::{self, Location};
use gpui_kit::assets::IconName;

/// The categories the user folded away this session. Kept in memory only.
#[derive(Default)]
pub struct Collapsed {
    folded: HashSet<Id>,
    /// Changes with every toggle, so a cached sidebar knows it is stale.
    generation: u64,
}

impl Collapsed {
    /// Folds `category` if it is open, opens it if folded.
    pub fn toggle(&mut self, category: Id) {
        if !self.folded.remove(&category) {
            self.folded.insert(category);
        }
        self.generation += 1;
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn contains(&self, category: Id) -> bool {
        self.folded.contains(&category)
    }

    /// The rows of a category: all of them, or while it is folded just the
    /// selected channel when it lives in there and the voice channels that
    /// have someone in them (`occupied`), as Discord does.
    pub fn visible<'a>(
        &self,
        category: Option<Id>,
        channels: &[&'a Channel],
        selected: Option<Id>,
        occupied: impl Fn(Id) -> bool,
        unread: impl Fn(Id) -> bool,
    ) -> Vec<&'a Channel> {
        match category {
            Some(id) if self.contains(id) => channels
                .iter()
                .copied()
                .filter(|c| Some(c.id) == selected || (matches!(c.kind, ChannelKind::Voice | ChannelKind::Stage) && occupied(c.id)) || unread(c.id))
                .collect(),
            _ => channels.to_vec(),
        }
    }
}

/// The icon in front of a channel's name.
pub fn channel_icon(kind: ChannelKind) -> IconName {
    match kind {
        ChannelKind::Announcement => IconName::Megaphone,
        ChannelKind::Voice | ChannelKind::Stage => IconName::Volume2,
        ChannelKind::Forum => IconName::MessageSquare,
        _ => IconName::Hash,
    }
}

/// Someone connected to a voice channel, ready to draw.
#[derive(Clone, Debug, PartialEq)]
pub struct Participant {
    pub user_id: Id,
    pub name: String,
    pub avatar_url: String,
    pub muted: bool,
    pub deafened: bool,
    /// Done to them by a moderator, not by themselves.
    pub server_muted: bool,
    pub server_deafened: bool,
    pub streaming: bool,
    /// Their display name style's font, and their server tag's badge.
    pub font: Option<&'static str>,
    pub tag_badge: Option<String>,
    pub decoration: Option<String>,
}

/// Who is in voice channel `channel` of `guild`, by name.
pub fn participants(cache: &Cache, guild: Id, channel: Id) -> Vec<Participant> {
    let mut list: Vec<Participant> = cache
        .voice_participants(channel)
        .into_iter()
        .map(|state| {
            let user = cache.user(state.user_id);
            Participant {
                user_id: state.user_id,
                name: user.map_or_else(|| "Unknown".to_string(), |u| cache.display_name(Some(guild), u).to_string()),
                avatar_url: match user {
                    Some(u) => cache.avatar_url(Some(guild), u, 64),
                    None => cdn::avatar_url(state.user_id, None, 64),
                },
                muted: state.is_muted(),
                deafened: state.is_deafened(),
                server_muted: state.mute,
                server_deafened: state.deaf,
                streaming: state.self_stream,
                font: user.and_then(|u| crate::name_fonts::family(u.extras.display_name_styles.as_ref()?.font_id)),
                tag_badge: user.and_then(|u| u.server_tag()?.1),
                decoration: user.and_then(|u| u.decoration_url()),
            }
        })
        .collect();
    // Whoever's sharing their screen goes first.
    list.sort_by_cached_key(|p| (!p.streaming, p.name.to_lowercase(), p.user_id));
    list
}

const CONNECT: u64 = 1 << 20;

/// How long a voice channel has been going: "24:59", "2:17:32".
pub fn elapsed_label(started_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(started_ms) / 1000;
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m:02}:{s:02}") }
}

/// A DM or group with messages waiting: its icon sits in the rail, under Home.
#[derive(Clone, Debug, PartialEq)]
pub struct RailDm {
    pub id: Id,
    pub name: String,
    pub picture: String,
    /// A group with no picture of its own: two of its people, as Discord stacks them.
    pub faces: Vec<String>,
    pub target: String,
    pub mentions: u32,
}

/// DMs with messages waiting, newest first.
fn dm_alerts(cache: &Cache) -> Vec<RailDm> {
    cache
        .private_channels()
        .into_iter()
        .filter(|c| cache.mentions(c.id) > 0 && cache.is_unread(c.id))
        .map(|channel| RailDm {
            id: channel.id,
            name: nav::channel_label(cache, channel),
            picture: match &channel.icon {
                Some(hash) => format!("{}/channel-icons/{}/{hash}.png?size=64", cdn::CDN, channel.id),
                None => {
                    let first = channel.recipient_ids.first().and_then(|id| cache.user(*id));
                    let (id, hash) = first.map_or((channel.id, None), |u| (u.id, u.avatar.clone()));
                    cdn::avatar_url(id, hash.as_deref(), 64)
                }
            },
            faces: group_faces(cache, channel),
            target: nav::channel_path(channel),
            mentions: cache.mentions(channel.id),
        })
        .collect()
}

/// The first two people of a group without its own picture, which Discord shows overlapping.
/// Empty for anything else.
pub fn group_faces(cache: &Cache, channel: &cord_core::model::Channel) -> Vec<String> {
    if channel.icon.is_some() || channel.recipient_ids.len() < 2 {
        return Vec::new();
    }
    channel
        .recipient_ids
        .iter()
        .take(2)
        .map(|id| {
            let hash = cache.user(*id).and_then(|u| u.avatar.clone());
            cdn::avatar_url(*id, hash.as_deref(), 64)
        })
        .collect()
}

/// A server icon in the rail.
#[derive(Clone, Debug, PartialEq)]
pub struct RailGuild {
    pub banner: Option<String>,
    /// The hover card's "New activity in #…".
    pub activity: Option<String>,
    pub id: Id,
    pub name: String,
    pub icon: Option<String>,
    /// Where clicking it goes.
    pub target: String,
    pub unread: bool,
    pub mentions: u32,
}

/// A server folder in the rail (Discord's guild folders).
#[derive(Clone, Debug, PartialEq)]
pub struct RailFolder {
    pub id: i64,
    pub name: Option<String>,
    pub color: Option<u32>,
    /// Indexes into `Sidebar::guilds`.
    pub guilds: Vec<usize>,
}

/// One slot of the rail, top to bottom.
#[derive(Clone, Debug, PartialEq)]
pub enum RailEntry {
    /// Index into `Sidebar::guilds`.
    Guild(usize),
    /// Index into `Sidebar::folders`.
    Folder(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChannelRow {
    pub id: Id,
    pub kind: ChannelKind,
    pub label: String,
    pub path: String,
    pub clickable: bool,
    /// What the row's buttons may offer.
    pub can_invite: bool,
    pub can_edit: bool,
    pub unread: bool,
    pub mentions: u32,
    /// Closed to @everyone, though we can see it: a little lock on the icon.
    pub locked: bool,
    /// The server's rules channel, with its own icon.
    pub rules: bool,
    /// Voice: someone's in it, what its status says, and since when (ms).
    pub occupied: bool,
    pub status: Option<String>,
    pub started: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DmRow {
    /// How their name looks, and their nameplate.
    pub stops: Vec<gpui_kit::Hsla>,
    pub color: Option<gpui_kit::Hsla>,
    pub effect: u8,
    pub effect_color: Option<gpui_kit::Hsla>,
    pub nameplate: Option<String>,
    pub palette: Option<String>,
    pub nameplate_video: Option<String>,
    pub id: Id,
    /// The other person, in a one-to-one DM.
    pub user: Option<Id>,
    pub bot: bool,
    pub verified_bot: bool,
    /// Discord's own account.
    pub official: bool,
    pub friend: bool,
    pub label: String,
    pub path: String,
    pub group: bool,
    pub avatar_url: String,
    pub subtitle: String,
    pub mentions: u32,
    pub unread: bool,
    /// A one-to-one DM's status mark and what's under the name.
    pub status: Option<Status>,
    pub mobile: bool,
    pub line: PresenceLine,
    pub in_voice: bool,
    pub decoration: Option<String>,
    pub font: Option<&'static str>,
    pub tag: Option<(String, Option<String>)>,
    /// A group DM's own picture.
    pub group_icon: Option<String>,
    /// See [`group_faces`].
    pub faces: Vec<String>,
}

/// One line of the channel column.
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Category { id: Id, label: String, folded: bool, can_add: bool },
    Channel(ChannelRow),
    /// A joined thread or forum post, under its channel.
    Thread(ChannelRow),
    Participant(Participant),
    Dm(DmRow),
}

/// Cache revision, location and fold generation: what the sidebar is built from.
pub type SidebarKey = (u64, Location, u64);

/// Everything the rail and the channel column draw, built once per key.
#[derive(Clone, Debug)]
pub struct Sidebar {
    pub key: SidebarKey,
    /// Every server, in rail order (folders flattened).
    pub guilds: Vec<RailGuild>,
    pub folders: Vec<RailFolder>,
    pub rail: Vec<RailEntry>,
    pub dm_alerts: Vec<RailDm>,
    pub title: String,
    /// In a server (not Direct Messages).
    pub in_guild: bool,
    pub rows: Vec<Row>,
}

pub fn key(cache: &Cache, location: Location, collapsed: &Collapsed) -> SidebarKey {
    (cache.sidebar_revision(), location, collapsed.generation())
}

/// Builds the sidebar. Voice participants are looked up only for the voice
/// channels that end up visible.
pub fn build(cache: &Cache, location: Location, collapsed: &Collapsed) -> Sidebar {
    let (guilds, folders, rail) = rail(cache);
    let mut rows = Vec::new();
    let (title, in_guild) = match location.guild {
        None => {
            for channel in cache.private_channels() {
                let group = channel.kind == ChannelKind::GroupDm || channel.recipient_ids.len() > 1;
                let person = channel.recipient_ids.first().and_then(|id| cache.user(*id)).filter(|_| !group);
                let presence = person.and_then(|u| cache.presence(u.id));
                let look = person.map(|u| crate::badges::name_look(u, &[]));
                rows.push(Row::Dm(DmRow {
                    stops: look.as_ref().map(|l| l.stops.clone()).unwrap_or_default(),
                    color: look.as_ref().and_then(|l| l.color),
                    effect: look.as_ref().map_or(0, |l| l.effect),
                    effect_color: look.as_ref().and_then(|l| l.effect_color),
                    nameplate: person.and_then(|u| u.nameplate_url()),
                    palette: person.and_then(|u| u.nameplate_palette()).map(str::to_string),
                    nameplate_video: person.and_then(|u| u.nameplate_video_url()),
                    user: person.map(|u| u.id),
                    bot: person.is_some_and(|u| u.bot),
                    verified_bot: person.is_some_and(|u| u.is_verified_bot()),
                    official: person.is_some_and(|u| u.is_discord()),
                    friend: person.is_some_and(|u| cache.is_friend(u.id)),
                    status: person.map(|_| presence.map_or(Status::Offline, |p| p.status)),
                    mobile: presence.is_some_and(|p| p.mobile),
                    line: presence.map(|p| p.line.clone()).unwrap_or_default(),
                    in_voice: person.is_some_and(|u| cache.in_voice(u.id)),
                    decoration: person.and_then(|u| u.decoration_url()),
                    font: person.and_then(|u| crate::name_fonts::family(u.extras.display_name_styles.as_ref()?.font_id)),
                    tag: person.and_then(|u| u.server_tag().map(|(t, b)| (t.to_string(), b))),
                    faces: group_faces(cache, channel),
                    group_icon: channel.icon.as_ref().map(|hash| format!("{}/channel-icons/{}/{hash}.png?size=64", cdn::CDN, channel.id)),
                    id: channel.id,
                    label: nav::channel_label(cache, channel),
                    path: nav::channel_path(channel),
                    group: channel.kind == ChannelKind::GroupDm || channel.recipient_ids.len() > 1,
                    avatar_url: {
                        let first = channel.recipient_ids.first().and_then(|id| cache.user(*id));
                        let (id, hash) = first.map_or((channel.id, None), |u| (u.id, u.avatar.clone()));
                        cdn::avatar_url(id, hash.as_deref(), 64)
                    },
                    subtitle: dm_subtitle(cache, channel),
                    mentions: cache.mentions(channel.id),
                    unread: cache.is_unread(channel.id),
                }));
            }
            ("Direct Messages".to_string(), false)
        }
        Some(guild) => {
            for group in nav::channel_groups(cache, guild) {
                if let Some(category) = group.category {
                    rows.push(Row::Category {
                        id: category.id,
                        label: nav::channel_label(cache, category),
                        folded: collapsed.contains(category.id),
                        can_add: cache.has_permission(category.id, cord_core::permissions::MANAGE_CHANNELS),
                    });
                }
                let occupied = |id: Id| !cache.voice_participants(id).is_empty();
                for channel in collapsed.visible(group.category.map(|c| c.id), &group.channels, location.channel, occupied, |id| cache.is_unread(id)) {
                    rows.push(Row::Channel(ChannelRow {
                        id: channel.id,
                        kind: channel.kind,
                        label: nav::channel_label(cache, channel),
                        path: nav::channel_path(channel),
                        // A voice row itself is for joining, later; its chat opens from the icon.
                        clickable: channel.kind.is_text_like() || channel.kind == ChannelKind::Forum,
                        can_invite: cache.has_permission(channel.id, cord_core::permissions::CREATE_INSTANT_INVITE),
                        can_edit: cache.has_permission(channel.id, cord_core::permissions::MANAGE_CHANNELS),
                        unread: cache.is_unread(channel.id),
                        mentions: cache.mentions(channel.id),
                        rules: cache.is_rules_channel(channel),
                        locked: {
                            let closed = cord_core::permissions::VIEW_CHANNEL | CONNECT;
                            channel.permission_overwrites.iter().any(|o| o.id == guild && o.deny & closed != 0)
                        },
                        occupied: occupied(channel.id),
                        status: cache.voice_status(channel.id).map(str::to_string).filter(|_| occupied(channel.id)),
                        started: cache.voice_started(channel.id).filter(|_| occupied(channel.id)),
                    }));
                    if matches!(channel.kind, ChannelKind::Voice | ChannelKind::Stage) {
                        rows.extend(participants(cache, guild, channel.id).into_iter().map(Row::Participant));
                    }
                    let mut threads: Vec<&Channel> = cache
                        .guild_channels(guild)
                        .into_iter()
                        .filter(|t| t.kind == ChannelKind::Thread && t.parent_id == Some(channel.id))
                        .collect();
                    threads.sort_by_key(|t| t.id);
                    rows.extend(threads.into_iter().map(|t| {
                        Row::Thread(ChannelRow {
                            id: t.id,
                            kind: t.kind,
                            label: nav::channel_label(cache, t),
                            path: nav::channel_path(t),
                            clickable: true,
                            can_invite: false,
                            can_edit: false,
                            unread: cache.is_unread(t.id),
                            mentions: cache.mentions(t.id),
                            rules: false,
                            locked: false,
                            occupied: false,
                            status: None,
                            started: None,
                        })
                    }));
                }
            }
            (cache.guild(guild).map(|g| g.name.clone()).unwrap_or_default(), true)
        }
    };
    Sidebar { key: key(cache, location, collapsed), guilds, folders, rail, dm_alerts: dm_alerts(cache), title, in_guild, rows }
}

/// The rail in Discord's order: the saved folders (a folder without an id is
/// a lone server), with servers they don't list (joined since) on top.
fn rail(cache: &Cache) -> (Vec<RailGuild>, Vec<RailFolder>, Vec<RailEntry>) {
    let entry = |g: &cord_core::model::Guild| RailGuild {
        banner: g.banner.as_deref().map(|hash| cdn::guild_banner_url(g.id, hash, 480)),
        activity: cache.newest_unread_channel(g.id).map(|c| format!("New activity in #{}", nav::channel_label(cache, c))),
        id: g.id,
        name: g.name.clone(),
        icon: g.icon.clone(),
        target: match nav::first_text_channel(cache, g.id) {
            Some(channel) => format!("/channels/{}/{channel}", g.id),
            None => format!("/channels/{}", g.id),
        },
        unread: cache.guild_unread(g.id),
        mentions: cache.guild_mentions(g.id),
    };
    let listed: HashSet<Id> = cache.guild_folders().iter().flat_map(|f| f.guild_ids.iter().copied()).collect();
    let (mut guilds, mut folders, mut rail) = (Vec::new(), Vec::new(), Vec::new());
    for g in cache.guilds().filter(|g| !listed.contains(&g.id)) {
        rail.push(RailEntry::Guild(guilds.len()));
        guilds.push(entry(g));
    }
    for folder in cache.guild_folders() {
        let members: Vec<usize> = folder
            .guild_ids
            .iter()
            .filter_map(|id| cache.guild(*id))
            .map(|g| {
                guilds.push(entry(g));
                guilds.len() - 1
            })
            .collect();
        match (folder.id, members.as_slice()) {
            (_, []) => {}
            (None, [one]) => rail.push(RailEntry::Guild(*one)),
            (id, _) => {
                rail.push(RailEntry::Folder(folders.len()));
                folders.push(RailFolder { id: id.unwrap_or_default(), name: folder.name.clone(), color: folder.color, guilds: members });
            }
        }
    }
    (guilds, folders, rail)
}

/// The muted line under a group DM's name: its member count. A one-to-one
/// DM has none; its line comes from what the person is up to.
pub fn dm_subtitle(_cache: &Cache, channel: &Channel) -> String {
    if channel.kind == ChannelKind::GroupDm || channel.recipient_ids.len() > 1 {
        // The recipients plus us.
        let count = channel.recipient_ids.len() + 1;
        return format!("{count} Member{}", if count == 1 { "" } else { "s" });
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_time_reads_like_a_clock() {
        assert_eq!(elapsed_label(0, 1_499_000), "24:59");
        assert_eq!(elapsed_label(0, 8_252_000), "2:17:32");
    }
    use cord_core::event::CoreEvent;
    use cord_core::model::{Guild, Member, Ready, User, VoiceState};

    fn user(id: u64, name: &str, global: Option<&str>) -> User {
        User { id: Id(id), username: name.into(), global_name: global.map(Into::into), avatar: None, bot: false, extras: Default::default() }
    }

    fn ch(id: u64, kind: ChannelKind) -> Channel {
        Channel {
            owner_id: None,
            id: Id(id), kind, guild_id: Some(Id(300)), name: Some(format!("c{id}")), position: 0, parent_id: None,
            last_message_id: None, recipient_ids: vec![], recipients: vec![], icon: None, permission_overwrites: vec![], topic: None, thread: None, forum: None,
        }
    }

    #[test]
    fn collapsing_hides_all_but_the_selected_channel() {
        let (a, b, c) = (ch(1, ChannelKind::Text), ch(2, ChannelKind::Text), ch(3, ChannelKind::Voice));
        let all = [&a, &b, &c];
        let ids = |v: Vec<&Channel>| v.iter().map(|c| c.id.0).collect::<Vec<_>>();
        let mut collapsed = Collapsed::default();
        let nobody = |_: Id| false;
        assert_eq!(ids(collapsed.visible(Some(Id(10)), &all, None, nobody, nobody)), [1, 2, 3]);

        collapsed.toggle(Id(10));
        assert!(collapsed.contains(Id(10)));
        assert!(ids(collapsed.visible(Some(Id(10)), &all, None, nobody, nobody)).is_empty());
        assert_eq!(ids(collapsed.visible(Some(Id(10)), &all, None, nobody, |id| id == Id(3))), [3], "unread stays");
        assert_eq!(ids(collapsed.visible(Some(Id(10)), &all, Some(Id(2)), nobody, nobody)), [2]);
        // A selection elsewhere changes nothing; loose channels have no category to fold.
        assert!(ids(collapsed.visible(Some(Id(10)), &all, Some(Id(99)), nobody, nobody)).is_empty());
        assert_eq!(ids(collapsed.visible(None, &all, None, nobody, nobody)), [1, 2, 3]);
        // Another category is unaffected.
        assert_eq!(ids(collapsed.visible(Some(Id(11)), &all, None, nobody, nobody)), [1, 2, 3]);

        collapsed.toggle(Id(10));
        assert_eq!(ids(collapsed.visible(Some(Id(10)), &all, None, nobody, nobody)), [1, 2, 3]);
    }

    #[test]
    fn collapsed_categories_keep_occupied_voice_channels() {
        let (a, b, c) = (ch(1, ChannelKind::Text), ch(3, ChannelKind::Voice), ch(4, ChannelKind::Voice));
        let all = [&a, &b, &c];
        let ids = |v: Vec<&Channel>| v.iter().map(|c| c.id.0).collect::<Vec<_>>();
        let mut collapsed = Collapsed::default();
        collapsed.toggle(Id(10));
        let occupied = |id: Id| id == Id(4);
        assert_eq!(ids(collapsed.visible(Some(Id(10)), &all, None, occupied, |_| false)), [4]);
        assert_eq!(ids(collapsed.visible(Some(Id(10)), &all, Some(Id(1)), occupied, |_| false)), [1, 4]);
        // A text channel never counts as occupied.
        assert!(ids(collapsed.visible(Some(Id(10)), &all, None, |id| id == Id(1), |_| false)).is_empty());
    }

    #[test]
    fn icons_by_kind() {
        let path = |k| channel_icon(k).path().to_string();
        assert_eq!(path(ChannelKind::Text), IconName::Hash.path().to_string());
        assert_eq!(path(ChannelKind::Announcement), IconName::Megaphone.path().to_string());
        assert_eq!(path(ChannelKind::Voice), IconName::Volume2.path().to_string());
        assert_eq!(path(ChannelKind::Stage), IconName::Volume2.path().to_string());
        assert_eq!(path(ChannelKind::Forum), IconName::MessageSquare.path().to_string());
    }

    fn vs(user: u64, channel: u64, f: impl FnOnce(&mut VoiceState)) -> VoiceState {
        let mut v = VoiceState { session_id: None,
            guild_id: None, user_id: Id(user), channel_id: Some(Id(channel)), self_mute: false, self_deaf: false,
            self_video: false, self_stream: false, mute: false, deaf: false,
        };
        f(&mut v);
        v
    }

    fn cache() -> Cache {
        let mut c = Cache::default();
        c.apply(CoreEvent::Ready(Box::new(Ready {
            user: user(1, "me", None),
            session_id: "s".into(),
            resume_gateway_url: "u".into(), read_state: Default::default(), user_settings_proto: None, relationships: vec![], sessions: vec![], user_guild_settings: Default::default(),
            guilds: vec![Guild {
                id: Id(300), name: "g".into(), icon: None, unavailable: false,
                channels: vec![ch(20, ChannelKind::Voice)],
                roles: vec![], owner_id: None, emojis: vec![], premium_tier: 0, banner: None, rules_channel: None,
                voice_states: vec![
                    vs(2, 20, |v| v.self_mute = true),
                    vs(3, 20, |v| { v.self_stream = true; v.deaf = true }),
                    vs(4, 20, |_| {}),
                ],
            }],
            private_channels: vec![
                Channel { guild_id: None, recipient_ids: vec![Id(2)], ..ch(400, ChannelKind::Dm) },
                Channel { guild_id: None, recipient_ids: vec![Id(2), Id(3)], ..ch(401, ChannelKind::GroupDm) },
            ],
            users: vec![user(2, "lin", Some("Lin")), user(3, "kai", None), user(4, "amy", None)],
            merged_members: vec![],
        })));
        c.apply(CoreEvent::MemberUpsert {
            guild_id: Id(300),
            member: Member {
                user_id: Some(Id(4)), user: None, nick: Some("Zed".into()), avatar: None, roles: vec![],
                communication_disabled_until: None, joined_at: None,
            },
        });
        c
    }

    #[test]
    fn voice_participants_use_server_names_and_flags() {
        let c = cache();
        let list = participants(&c, Id(300), Id(20));
        let shape: Vec<(&str, bool, bool, bool)> =
            list.iter().map(|p| (p.name.as_str(), p.muted, p.deafened, p.streaming)).collect();
        assert_eq!(shape, [("kai", false, true, true), ("Lin", true, false, false), ("Zed", false, false, false)]);
        assert!(participants(&c, Id(300), Id(21)).is_empty());
    }

    #[test]
    fn building_lists_participants_only_for_visible_voice_channels() {
        let mut c = cache();
        let here = Location { guild: Some(Id(300)), channel: None };
        let mut collapsed = Collapsed::default();
        let participants = |s: &Sidebar| s.rows.iter().filter(|r| matches!(r, Row::Participant(_))).count();
        let side = build(&c, here, &collapsed);
        assert_eq!((side.in_guild, side.title.as_str(), participants(&side)), (true, "g", 3));
        assert_eq!(side.key, key(&c, here, &collapsed));
        // Folding the (loose) channels changes the key but loose channels stay.
        collapsed.toggle(Id(77));
        assert_ne!(build(&c, here, &collapsed).key, side.key);
        // A voice update changes the cache revision, so the key moves.
        c.apply(CoreEvent::VoiceStateUpdate { guild_id: Id(300), state: vs(2, 20, |_| {}), member: None });
        assert_ne!(key(&c, here, &collapsed), side.key);
        let dms = build(&c, Location { guild: None, channel: None }, &collapsed);
        assert!(!dms.in_guild);
        assert_eq!(dms.rows.len(), 2);
    }

    #[test]
    fn dm_second_lines() {
        let c = cache();
        assert_eq!(dm_subtitle(&c, c.channel(Id(400)).unwrap()), "", "a one-to-one DM shows what they're up to instead");
        assert_eq!(dm_subtitle(&c, c.channel(Id(401)).unwrap()), "3 Members");
    }
}
