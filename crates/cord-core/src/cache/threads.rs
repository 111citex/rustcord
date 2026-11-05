//! Threads and forum posts in the cache: which we've joined, how they're
//! told apart from channels, and what the messages say about them.

use super::*;

impl Cache {
    /// Before a thread replaces its old self: a change that doesn't say whether
    /// we're a member (a rename, an archive) must not make us leave, and its
    /// last message can't go backwards.
    pub(super) fn keep_thread_state(&self, channel: &mut Channel) {
        if channel.guild_id.is_none() {
            channel.guild_id = channel.parent_id.and_then(|p| self.channels.get(&p)).and_then(|p| p.guild_id);
        }
        let (Some(info), Some(old)) = (channel.thread.as_mut(), self.channels.get(&channel.id)) else { return };
        if let Some(was) = old.thread.as_ref() {
            info.joined |= was.joined;
            if info.message_count == 0 {
                info.message_count = was.message_count;
            }
        }
        channel.last_message_id = channel.last_message_id.max(old.last_message_id);
    }

    /// Threads named by messages (a chip's thread, or the one a post is): kept so
    /// they can be shown and opened. Nothing in the sidebar changes for these.
    pub(super) fn note_message_threads(&mut self, messages: &[Message]) {
        for m in messages {
            let Some(thread) = m.thread.as_deref() else { continue };
            if thread.thread.is_none() {
                continue;
            }
            let mut thread = thread.clone();
            if thread.parent_id.is_none() {
                thread.parent_id = Some(m.channel_id);
            }
            self.insert_channel(thread);
        }
    }

    fn set_thread_joined(&mut self, id: Id, joined: bool) -> bool {
        let Some(info) = self.channels.get_mut(&id).and_then(|c| c.thread.as_mut()) else { return false };
        let changed = info.joined != joined;
        info.joined = joined;
        changed
    }

    pub(super) fn apply_thread_event(&mut self, event: CoreEvent) {
        let me = self.me.as_ref().map(|u| u.id);
        match event {
            CoreEvent::ThreadUpsert { mut thread, created } => {
                // Whoever starts a thread is in it.
                if created && thread.owner_id.is_some() && thread.owner_id == me
                    && let Some(info) = thread.thread.as_mut()
                {
                    info.joined = true;
                }
                self.insert_channel(thread);
                self.bump_sidebar();
            }
            CoreEvent::ThreadDelete { id } => {
                self.remove_channel(id);
                self.bump_sidebar();
            }
            CoreEvent::ThreadListSync { guild_id, parents, threads, joined } => {
                let listed: Vec<Id> = threads.iter().map(|t| t.id).collect();
                // Active threads it no longer lists have archived, gone, or stopped being ours.
                let stale: Vec<Id> = self
                    .channels
                    .values()
                    .filter(|c| c.guild_id == Some(guild_id) && c.thread.as_ref().is_some_and(|t| !t.archived && t.joined))
                    .filter(|c| parents.as_ref().is_none_or(|p| c.parent_id.is_some_and(|c| p.contains(&c))))
                    .filter(|c| !listed.contains(&c.id))
                    .map(|c| c.id)
                    .collect();
                for id in stale {
                    self.set_thread_joined(id, false);
                }
                for mut thread in threads {
                    thread.guild_id = Some(guild_id);
                    if let Some(info) = thread.thread.as_mut() {
                        info.joined |= joined.contains(&thread.id);
                    }
                    self.insert_channel(thread);
                }
                self.bump_sidebar();
            }
            CoreEvent::ThreadJoined { id, user_id } => {
                if user_id.is_none() || user_id == me {
                    self.set_thread_joined(id, true);
                    self.bump_sidebar();
                }
            }
            CoreEvent::ThreadMembers { id, count, added, removed } => {
                if let Some(info) = self.channels.get_mut(&id).and_then(|c| c.thread.as_mut())
                    && let Some(count) = count
                {
                    info.member_count = count;
                }
                if me.is_some_and(|me| added.contains(&me)) {
                    self.set_thread_joined(id, true);
                } else if me.is_some_and(|me| removed.contains(&me)) {
                    self.set_thread_joined(id, false);
                }
                self.bump_sidebar();
            }
            _ => {}
        }
    }

    /// A thread or post by its id (a thread made from a message has that message's id).
    pub fn thread(&self, id: Id) -> Option<&Channel> {
        self.channels.get(&id).filter(|c| c.thread.is_some())
    }

    /// The thread started from `message`, if we know of it.
    pub fn thread_of<'a>(&'a self, message: &'a Message) -> Option<&'a Channel> {
        self.thread(message.id)
            .filter(|t| t.parent_id == Some(message.channel_id))
            .or(message.thread.as_deref().filter(|t| t.thread.is_some()))
    }

    /// Whether we're a member of `id` (threads only).
    pub fn is_thread_joined(&self, id: Id) -> bool {
        self.thread(id).and_then(|t| t.thread.as_ref()).is_some_and(|t| t.joined)
    }

    /// Threads we've joined that are under `parent`, oldest first, for the channel list.
    /// `keep` is the thread being looked at: it stays listed whatever its state.
    pub fn listed_threads(&self, guild: Id, parent: Id, keep: Option<Id>) -> Vec<&Channel> {
        let mut found: Vec<&Channel> = self
            .channels
            .values()
            .filter(|c| c.guild_id == Some(guild) && c.parent_id == Some(parent))
            .filter(|c| c.thread.as_ref().is_some_and(|t| (t.joined && !t.archived) || Some(c.id) == keep))
            .collect();
        found.sort_by_key(|c| c.id);
        found
    }

    /// Every thread we know under `parent`, for the threads list.
    pub fn threads_under(&self, parent: Id) -> Vec<&Channel> {
        self.channels.values().filter(|c| c.parent_id == Some(parent) && c.thread.is_some()).collect()
    }

    /// Whether we may start a thread from a message in `channel`.
    pub fn can_start_thread(&self, channel: Id) -> bool {
        let Some(c) = self.channels.get(&channel) else { return false };
        matches!(c.kind, crate::model::ChannelKind::Text | crate::model::ChannelKind::Announcement)
            && self.has_permission(channel, permissions::CREATE_PUBLIC_THREADS)
    }

    /// Whether we may lock, archive and rename other people's threads here.
    pub fn can_manage_threads(&self, channel: Id) -> bool {
        self.has_permission(channel, permissions::MANAGE_THREADS)
    }

    /// Our own new thread, joined: what Discord answers a create with.
    pub fn add_thread(&mut self, mut thread: Channel) {
        if let Some(info) = thread.thread.as_mut() {
            info.joined = true;
        }
        self.insert_channel(thread);
        self.bump_sidebar();
    }

    /// We left or joined `id` (before or after Discord has confirmed).
    pub fn set_joined(&mut self, id: Id, joined: bool) {
        if self.set_thread_joined(id, joined) {
            self.bump_sidebar();
        }
    }

    /// Files a page of threads from a listing (forum posts, archived threads).
    pub fn add_threads(&mut self, threads: Vec<Channel>, joined: &[Id]) {
        for mut thread in threads {
            if let Some(info) = thread.thread.as_mut() {
                info.joined |= joined.contains(&thread.id);
            }
            self.insert_channel(thread);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ChannelKind;

    fn channel(v: serde_json::Value) -> Channel {
        serde_json::from_value(v).unwrap()
    }

    fn parent() -> Channel {
        channel(serde_json::json!({"id": "10", "type": 0, "guild_id": "1", "name": "general"}))
    }

    fn thread(id: u64, member: bool) -> Channel {
        let mut v = serde_json::json!({
            "id": id.to_string(), "type": 11, "parent_id": "10", "guild_id": "1", "name": "t", "owner_id": "7",
            "message_count": 3, "member_count": 2, "last_message_id": "500",
            "thread_metadata": {"archived": false, "locked": false, "auto_archive_duration": 1440},
        });
        if member {
            v["member"] = serde_json::json!({"user_id": "7", "join_timestamp": "x", "flags": 0});
        }
        channel(v)
    }

    fn me(id: u64) -> User {
        serde_json::from_value(serde_json::json!({"id": id.to_string(), "username": "someone"})).unwrap()
    }

    fn cache() -> Cache {
        let mut c = Cache { me: Some(me(7)), ..Default::default() };
        c.insert_channel(parent());
        c
    }

    #[test]
    fn a_thread_decodes_with_its_state() {
        let t = thread(20, true);
        assert_eq!(t.kind, ChannelKind::Thread);
        let info = t.thread.unwrap();
        assert!(info.joined && !info.archived && !info.private);
        assert_eq!((info.message_count, info.member_count, info.auto_archive), (3, 2, 1440));
        let private = channel(serde_json::json!({"id": "21", "type": 12, "parent_id": "10"}));
        assert!(private.thread.unwrap().private);
    }

    #[test]
    fn an_update_without_a_member_keeps_us_in() {
        let mut c = cache();
        c.insert_channel(thread(20, true));
        c.apply(CoreEvent::ThreadUpsert { thread: thread(20, false), created: false });
        assert!(c.is_thread_joined(Id(20)));
        c.apply(CoreEvent::ThreadMembers { id: Id(20), count: Some(1), added: vec![], removed: vec![Id(7)] });
        assert!(!c.is_thread_joined(Id(20)));
        assert_eq!(c.thread(Id(20)).unwrap().thread.as_ref().unwrap().member_count, 1);
    }

    #[test]
    fn joining_comes_from_the_gateway() {
        let mut c = cache();
        c.insert_channel(thread(20, false));
        c.apply(CoreEvent::ThreadJoined { id: Id(20), user_id: Some(Id(99)) });
        assert!(!c.is_thread_joined(Id(20)));
        c.apply(CoreEvent::ThreadJoined { id: Id(20), user_id: None });
        assert!(c.is_thread_joined(Id(20)));
        c.apply(CoreEvent::ThreadMembers { id: Id(20), count: None, added: vec![Id(7)], removed: vec![] });
        assert!(c.is_thread_joined(Id(20)));
    }

    #[test]
    fn making_a_thread_joins_its_maker() {
        let mut c = cache();
        c.apply(CoreEvent::ThreadUpsert { thread: thread(20, false), created: true });
        assert!(c.is_thread_joined(Id(20)));
        c.apply(CoreEvent::ThreadUpsert { thread: thread(21, false), created: false });
        assert!(!c.is_thread_joined(Id(21)));
    }

    #[test]
    fn deleting_removes_it() {
        let mut c = cache();
        c.insert_channel(thread(20, true));
        c.apply(CoreEvent::ThreadDelete { id: Id(20) });
        assert!(c.thread(Id(20)).is_none());
    }

    #[test]
    fn a_list_sync_drops_the_threads_it_no_longer_lists() {
        let mut c = cache();
        c.insert_channel(thread(20, true));
        c.insert_channel(thread(21, true));
        c.apply(CoreEvent::ThreadListSync { guild_id: Id(1), parents: Some(vec![Id(10)]), threads: vec![thread(21, false), thread(22, false)], joined: vec![Id(22)] });
        assert!(!c.is_thread_joined(Id(20)));
        assert!(c.is_thread_joined(Id(21)));
        assert!(c.is_thread_joined(Id(22)));
        // Other channels' threads are none of its business.
        c.insert_channel(channel(serde_json::json!({"id": "11", "type": 0, "guild_id": "1"})));
        let mut other = thread(30, true);
        other.parent_id = Some(Id(11));
        c.insert_channel(other);
        c.apply(CoreEvent::ThreadListSync { guild_id: Id(1), parents: Some(vec![Id(10)]), threads: vec![], joined: vec![] });
        assert!(c.is_thread_joined(Id(30)));
        assert!(!c.is_thread_joined(Id(21)));
    }

    #[test]
    fn the_list_shows_joined_threads_and_the_open_one() {
        let mut c = cache();
        c.insert_channel(thread(20, true));
        c.insert_channel(thread(21, false));
        let mut archived = thread(22, true);
        archived.thread.as_mut().unwrap().archived = true;
        c.insert_channel(archived);
        let ids = |keep| c.listed_threads(Id(1), Id(10), keep).iter().map(|t| t.id.0).collect::<Vec<_>>();
        assert_eq!(ids(None), vec![20]);
        assert_eq!(ids(Some(Id(21))), vec![20, 21]);
        assert_eq!(ids(Some(Id(22))), vec![20, 22]);
    }

    #[test]
    fn a_threads_message_count_follows_its_messages() {
        let mut c = cache();
        c.insert_channel(thread(20, true));
        let say = |id: u64, kind: u8| {
            let mut m = super::tests_support::message(id, 20);
            m.kind = kind;
            CoreEvent::MessageCreate(m)
        };
        // A post's first message and the starter message are not counted.
        c.apply(say(20, 0));
        c.apply(say(600, 0));
        c.apply(say(601, 0));
        c.apply(say(602, 21));
        let t = c.thread(Id(20)).unwrap();
        assert_eq!(t.thread.as_ref().unwrap().message_count, 5);
        assert_eq!(t.last_message_id, Some(Id(602)));
    }

    #[test]
    fn a_message_brings_its_thread_along() {
        let mut c = cache();
        let mut m = super::tests_support::message(20, 10);
        m.thread = Some(Box::new(thread(20, false)));
        c.apply(CoreEvent::MessageCreate(m.clone()));
        assert_eq!(c.thread_of(&m).map(|t| t.id), Some(Id(20)));
        // A thread made from a message has the message's id: no payload needed.
        m.thread = None;
        assert_eq!(c.thread_of(&m).map(|t| t.id), Some(Id(20)));
        let other = super::tests_support::message(21, 10);
        assert!(c.thread_of(&other).is_none());
    }

    #[test]
    fn a_thread_follows_its_channels_permissions() {
        use crate::model::{OverwriteKind, PermissionOverwrite, Role};
        let mut c = cache();
        let role = |id: u64, permissions: u64| Role { id: Id(id), name: String::new(), color: 0, position: 0, permissions, hoist: false, icon: None, unicode_emoji: None, colors: None, mentionable: false };
        let everyone = permissions::VIEW_CHANNEL | permissions::SEND_MESSAGES;
        c.insert_guild(Guild {
            id: Id(1), name: "g".into(), icon: None, unavailable: false, channels: vec![], roles: vec![role(1, everyone)], owner_id: Some(Id(99)),
            voice_states: vec![], emojis: vec![], premium_tier: 0, banner: None, rules_channel: None,
        });
        c.insert_member(Id(1), super::tests_support::member(7));
        c.insert_channel(thread(20, true));
        assert!(c.can_view(Id(20)));
        // No Send Messages in Threads: the thread's composer is closed, the channel's isn't.
        assert!(!c.can_send(Id(20)));
        assert!(c.can_send(Id(10)));
        let mut hidden = parent();
        hidden.permission_overwrites = vec![PermissionOverwrite { id: Id(1), kind: OverwriteKind::Role, allow: 0, deny: permissions::VIEW_CHANNEL }];
        c.insert_channel(hidden);
        assert!(!c.can_view(Id(20)));
    }
}

#[cfg(test)]
mod tests_support {
    use super::*;

    pub fn message(id: u64, channel: u64) -> Message {
        serde_json::from_value(serde_json::json!({
            "id": id.to_string(), "channel_id": channel.to_string(), "timestamp": "2026-01-01T00:00:00Z",
            "author": {"id": "5", "username": "other"},
        }))
        .unwrap()
    }

    pub fn member(user: u64) -> Member {
        serde_json::from_value(serde_json::json!({"user": {"id": user.to_string(), "username": "someone"}, "roles": []})).unwrap()
    }
}
