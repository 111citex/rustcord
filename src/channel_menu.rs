//! Right-click menus for a server's channels and threads in the channel list, in
//! Discord's order. What the user may not do isn't shown; what rustcord doesn't do yet
//! is shown dimmed. Deleting asks first.

use std::rc::Rc;

use cord_core::Id;
use cord_core::cache::Cache;
use cord_core::model::{ChannelKind, Notify};
use cord_core::notify::{MUTE_FOR, ThreadChange};
use cord_core::permissions as perms;
use gpui_kit::assets::IconName;
use gpui_kit::{App, Entity};

use crate::context_menu::Entry;
use crate::store::Store;

/// What a menu entry does, worked out apart from doing it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Act {
    MarkRead,
    /// Mute for this many seconds, or until undone.
    Mute(Option<i64>),
    Unmute,
    Notify(Notify),
    /// Asks first.
    Delete,
    Thread(ThreadChange),
    Leave,
}

/// What a menu needs to know about a channel or thread.
#[derive(Clone, Debug, PartialEq)]
pub struct Facts {
    pub id: Id,
    pub guild: Id,
    pub name: String,
    pub voice: bool,
    /// A thread's state; `None` for a channel.
    pub thread: Option<ThreadFacts>,
    pub unread: bool,
    /// When its own mute ends: `Some(None)` muted until undone.
    pub muted: Option<Option<i64>>,
    pub notify: Notify,
    pub can_invite: bool,
    pub can_manage_channels: bool,
    pub can_manage_threads: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadFacts {
    pub archived: bool,
    pub locked: bool,
    pub joined: bool,
}

impl Facts {
    pub fn read(cache: &Cache, id: Id) -> Option<Facts> {
        let channel = cache.channel(id)?;
        let guild = channel.guild_id?;
        Some(Facts {
            id,
            guild,
            name: channel.name.clone().unwrap_or_default(),
            voice: matches!(channel.kind, ChannelKind::Voice | ChannelKind::Stage),
            thread: channel.thread.as_ref().map(|t| ThreadFacts { archived: t.archived, locked: t.locked, joined: t.joined }),
            unread: cache.is_unread(id) || cache.mentions(id) > 0,
            muted: cache.channel_muted_until(id),
            notify: cache.notify_level(id),
            can_invite: cache.has_permission(id, perms::CREATE_INSTANT_INVITE),
            can_manage_channels: cache.has_permission(id, perms::MANAGE_CHANNELS),
            can_manage_threads: cache.has_permission(id, perms::MANAGE_THREADS),
        })
    }
}

/// How a notification level reads in the menu.
pub fn notify_label(level: Notify) -> &'static str {
    match level {
        Notify::All => "All Messages",
        Notify::Mentions => "Only @mentions",
        Notify::Nothing => "Nothing",
        Notify::Inherit => "Use Category Default",
    }
}

/// "Muted until 5:30 PM", or until it's undone.
fn muted_hint(until: Option<i64>) -> String {
    match until {
        Some(ms) => format!("Muted until {}", crate::nav::short_time(ms.max(0) as u64, &chrono::Local)),
        None => "Muted".to_string(),
    }
}

type Do = Rc<dyn Fn(Act, &mut App)>;

fn act(label: &'static str, what: Act, run: &Do) -> Entry {
    let run = run.clone();
    Entry::item(label, move |_, cx| run(what, cx))
}

/// Mute Channel (or Thread): its lengths, or Unmute while it's muted.
fn mute_entries(noun: &str, facts: &Facts, run: &Do) -> Entry {
    match facts.muted {
        Some(until) => act(if noun == "Thread" { "Unmute Thread" } else { "Unmute Channel" }, Act::Unmute, run).hint(muted_hint(until)),
        None => Entry::submenu(format!("Mute {noun}"), MUTE_FOR.iter().map(|(label, seconds)| act(label, Act::Mute(*seconds), run)).collect()),
    }
}

fn notify_entries(facts: &Facts, run: &Do) -> Entry {
    let levels = [Notify::Inherit, Notify::All, Notify::Mentions, Notify::Nothing];
    let choices = levels
        .into_iter()
        .map(|level| {
            let run = run.clone();
            Entry::Check { label: notify_label(level).into(), on: Rc::new(std::cell::Cell::new(facts.notify == level)), danger: false, closes: true, action: Rc::new(move |_, cx| run(Act::Notify(level), cx)) }
        })
        .collect();
    Entry::submenu("Notification Settings", choices).hint(notify_label(facts.notify))
}

fn mark_read(facts: &Facts, run: &Do) -> Entry {
    if facts.unread { act("Mark As Read", Act::MarkRead, run) } else { Entry::later("Mark As Read") }
}

fn link(facts: &Facts) -> String {
    format!("https://discord.com/channels/{}/{}", facts.guild, facts.id)
}

/// A server channel's menu (409).
pub fn channel_entries(facts: &Facts, run: &Do) -> Vec<Entry> {
    let mut entries = vec![mark_read(facts, run), Entry::Separator, Entry::later("Add to Favorites").icon(IconName::Star), Entry::Separator];
    if facts.can_invite {
        entries.push(Entry::later("Invite to Channel"));
        if facts.voice {
            entries.push(Entry::later("Invite to Voice"));
        }
    }
    entries.push(Entry::later("Pin Channel to Top"));
    entries.push(Entry::copy("Copy Link", link(facts)));
    entries.push(Entry::Separator);
    entries.push(mute_entries("Channel", facts, run));
    entries.push(notify_entries(facts, run));
    if facts.can_manage_channels {
        entries.extend([Entry::Separator, Entry::later("Move to..."), Entry::Separator]);
        entries.extend([Entry::later("Edit Channel"), Entry::later("Duplicate Channel"), Entry::later("Create Text Channel")]);
        entries.push(act("Delete Channel", Act::Delete, run).danger());
    }
    entries.push(Entry::Separator);
    entries.push(Entry::copy("Copy Channel ID", facts.id.to_string()).icon(IconName::Copy));
    entries
}

/// A thread's menu (410).
pub fn thread_entries(facts: &Facts, run: &Do) -> Vec<Entry> {
    let state = facts.thread.unwrap_or(ThreadFacts { archived: false, locked: false, joined: false });
    let mut entries = vec![mark_read(facts, run), Entry::Separator, Entry::later("Add to Favorites").icon(IconName::Star), Entry::Separator];
    if facts.can_manage_threads {
        entries.push(act(if state.archived { "Open Thread" } else { "Close Thread" }, Act::Thread(ThreadChange::Archived(!state.archived)), run));
        entries.push(act(if state.locked { "Unlock Thread" } else { "Lock Thread" }, Act::Thread(ThreadChange::Locked(!state.locked)), run));
        entries.push(Entry::later("Edit Thread"));
    }
    entries.push(Entry::copy("Copy Link", link(facts)));
    entries.push(Entry::Separator);
    entries.push(mute_entries("Thread", facts, run));
    entries.push(notify_entries(facts, run));
    if facts.can_manage_threads {
        entries.push(Entry::Separator);
        entries.push(act("Delete Thread", Act::Delete, run).danger());
    }
    if state.joined {
        entries.push(Entry::Separator);
        entries.push(act("Leave Thread", Act::Leave, run));
    }
    entries.push(Entry::Separator);
    entries.push(Entry::copy("Copy Thread ID", facts.id.to_string()).icon(IconName::Copy));
    entries
}

/// Does `what` to the channel `facts` describes.
fn run(store: &Entity<Store>, facts: &Facts, what: Act, cx: &mut App) {
    let channel = facts.id;
    match what {
        Act::MarkRead => store.update(cx, |store, cx| store.mark_read(channel, cx)),
        Act::Mute(seconds) => store.update(cx, |store, cx| store.mute_channel(channel, true, seconds, cx)),
        Act::Unmute => store.update(cx, |store, cx| store.mute_channel(channel, false, None, cx)),
        Act::Notify(level) => store.update(cx, |store, cx| store.set_channel_notifications(channel, level, cx)),
        Act::Thread(change) => store.update(cx, |store, cx| store.set_thread_state(channel, change, cx)),
        Act::Leave => store.update(cx, |store, cx| store.leave_thread(channel, cx)),
        Act::Delete => {
            let thread = facts.thread.is_some();
            let (title, noun) = if thread { ("Delete Thread", "thread") } else { ("Delete Channel", "channel") };
            let body = format!("Are you sure you want to delete the {noun} {}? This cannot be undone.", if thread { facts.name.clone() } else { format!("#{}", facts.name) });
            let store = store.clone();
            crate::confirm::ask(title.into(), body, title, move |cx| store.update(cx, |store, cx| store.delete_channel(channel, cx)), cx);
        }
    }
}

/// The menu for channel or thread `id`, empty if it isn't a server's.
pub fn entries(store: &Entity<Store>, id: Id, cx: &App) -> Vec<Entry> {
    let Some(facts) = Facts::read(store.read(cx).cache(), id) else { return Vec::new() };
    let doer: Do = {
        let (store, facts) = (store.clone(), facts.clone());
        Rc::new(move |what, cx| run(&store, &facts, what, cx))
    };
    if facts.thread.is_some() { thread_entries(&facts, &doer) } else { channel_entries(&facts, &doer) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            id: Id(20),
            guild: Id(10),
            name: "general".into(),
            voice: false,
            thread: None,
            unread: false,
            muted: None,
            notify: Notify::Inherit,
            can_invite: false,
            can_manage_channels: false,
            can_manage_threads: false,
        }
    }

    fn labels(entries: &[Entry]) -> Vec<String> {
        entries
            .iter()
            .filter_map(|e| match e {
                Entry::Item { label, action, .. } => Some(format!("{label}{}", if action.is_some() { "" } else { " (later)" })),
                Entry::Submenu { label, .. } => Some(format!("{label} >")),
                _ => None,
            })
            .collect()
    }

    fn nothing() -> Do {
        Rc::new(|_, _| {})
    }

    #[test]
    fn a_member_sees_only_what_they_may_do() {
        let shown = labels(&channel_entries(&facts(), &nothing()));
        assert_eq!(shown, ["Mark As Read (later)", "Add to Favorites (later)", "Pin Channel to Top (later)", "Copy Link", "Mute Channel >", "Notification Settings >", "Copy Channel ID"]);
    }

    #[test]
    fn a_manager_gets_the_rest_and_voice_gets_invite_to_voice() {
        let manager = Facts { can_invite: true, can_manage_channels: true, voice: true, unread: true, ..facts() };
        let shown = labels(&channel_entries(&manager, &nothing()));
        assert_eq!(shown[0], "Mark As Read", "something to read: it works");
        for label in ["Invite to Channel (later)", "Invite to Voice (later)", "Move to... (later)", "Edit Channel (later)", "Delete Channel"] {
            assert!(shown.iter().any(|s| s == label), "{label} in {shown:?}");
        }
        let delete = channel_entries(&manager, &nothing()).into_iter().find(|e| matches!(e, Entry::Item { label, .. } if label.as_ref() == "Delete Channel"));
        assert!(matches!(delete, Some(Entry::Item { danger: true, .. })));
    }

    #[test]
    fn a_muted_channel_offers_unmute() {
        let muted = Facts { muted: Some(None), ..facts() };
        let shown = labels(&channel_entries(&muted, &nothing()));
        assert!(shown.contains(&"Unmute Channel".to_string()));
        assert!(!shown.contains(&"Mute Channel >".to_string()));
    }

    #[test]
    fn notification_settings_name_the_current_level() {
        let entries = channel_entries(&Facts { notify: Notify::Mentions, ..facts() }, &nothing());
        let hint = entries.iter().find_map(|e| match e {
            Entry::Submenu { label, hint, entries } if label.as_ref() == "Notification Settings" => Some((hint.clone(), entries.len())),
            _ => None,
        });
        assert_eq!(hint.map(|(h, n)| (h.map(|h| h.to_string()), n)), Some((Some("Only @mentions".to_string()), 4)));
    }

    #[test]
    fn threads_need_manage_threads_to_close_lock_or_delete() {
        let thread = Facts { thread: Some(ThreadFacts { archived: false, locked: false, joined: true }), ..facts() };
        let shown = labels(&thread_entries(&thread, &nothing()));
        assert!(!shown.iter().any(|s| s.contains("Close") || s.contains("Lock") || s.contains("Delete")));
        assert!(shown.contains(&"Leave Thread".to_string()));
        assert_eq!(shown.last().map(String::as_str), Some("Copy Thread ID"));
        let moderator = Facts { can_manage_threads: true, thread: Some(ThreadFacts { archived: true, locked: false, joined: false }), ..thread };
        let shown = labels(&thread_entries(&moderator, &nothing()));
        for label in ["Open Thread", "Lock Thread", "Delete Thread", "Mute Thread >"] {
            assert!(shown.iter().any(|s| s == label), "{label} in {shown:?}");
        }
        assert!(!shown.contains(&"Leave Thread".to_string()), "not in it");
    }

    #[test]
    fn links_point_at_the_channel() {
        assert_eq!(link(&facts()), "https://discord.com/channels/10/20");
    }
}
