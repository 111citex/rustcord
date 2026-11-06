//! The app's view of Discord: cord-core's cache plus the connection state.
//! cord-core events arrive over a channel and are applied here; views
//! observe the store and redraw.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use cord_core::{Core, Id};
use cord_core::http::HttpError;
use cord_core::member_list::{self, RangeTracker, Status, Want};
use cord_core::model::{Channel, Message, ReactionEmoji};
use cord_core::cache::Cache;
use cord_core::event::{ConnectionState, CoreEvent};
use gpui_kit::{App, Context, Entity, EventEmitter, Task};

use crate::toasts::{Toast, Toasts};

#[derive(Clone)]
pub struct Services {
    pub core: Arc<Core>,
    pub store: Entity<Store>,
    pub toasts: Entity<Toasts>,
    pub settings: Entity<crate::settings::SettingsStore>,
}

impl Services {
    pub fn toast(&self, toast: Toast, cx: &mut App) {
        self.toasts.update(cx, |_, cx| cx.emit(toast));
    }
}

pub const HISTORY_PAGE: u8 = 50;

#[derive(Clone, Debug, PartialEq)]
pub enum History {
    Loading,
    Loaded,
    /// 403: the account can't read this channel.
    Forbidden,
    Failed(String),
}

/// The member list the app is subscribed to: one channel's, with the rows
/// the panel has asked the server to keep current.
struct MemberSub {
    guild: Id,
    channel: Id,
    ranges: RangeTracker,
}

impl MemberSub {
    fn send(&self, core: &Core) {
        core.send_gateway(member_list::subscribe_payload(self.guild, self.channel, &self.ranges.ranges()));
    }
}

/// Whether a window of ours has focus.
pub static WINDOW_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// One of Discord's sounds: from disk if it's been fetched, else fetched and kept.
async fn sound_file(core: Arc<Core>, name: &'static str, background: gpui_kit::BackgroundExecutor) -> Option<cord_voice::Clip> {
    let file = cord_core::paths::cache_dir().join("sounds").join(format!("{name}.mp3"));
    let saved = file.clone();
    let bytes = match background.spawn(async move { std::fs::read(&saved).ok() }).await {
        Some(bytes) => Some(bytes),
        None => {
            let fetched = core.cdn_bytes(format!("https://discord.com/assets/{name}.mp3")).await.ok();
            if let Some(bytes) = &fetched {
                let (file, copy) = (file.clone(), bytes.clone());
                background
                    .spawn(async move {
                        if let Some(dir) = file.parent() {
                            let _ = std::fs::create_dir_all(dir);
                        }
                        let _ = std::fs::write(file, copy);
                    })
                    .detach();
            }
            fetched
        }
    };
    let Some(bytes) = bytes else {
        tracing::warn!(name, "couldn't get a sound");
        return None;
    };
    let clip = background.spawn(async move { cord_voice::decode_mp3(bytes) }).await;
    if clip.is_none() {
        tracing::warn!(name, "couldn't decode a sound");
    }
    clip
}

/// Discord's rule: nothing on Do Not Disturb, nothing for our own messages,
/// muted places, `@silent` messages, or what's already in front of us.
pub fn should_notify(status: Option<Status>, mine: bool, pinged: bool, muted: bool, flags: u64, looking: bool) -> bool {
    const SUPPRESS_NOTIFICATIONS: u64 = 1 << 12;
    pinged && !mine && !muted && !looking && flags & SUPPRESS_NOTIFICATIONS == 0 && status != Some(Status::Dnd)
}

/// Channels kept after leaving them, and for how long.
const KEPT_CHANNELS: usize = 8;
const CHANNEL_TTL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// A channel left a little while ago, its messages still held.
struct RecentChannel {
    channel: Id,
    left: std::time::Instant,
    /// False once the gateway may have missed something for it.
    trusted: bool,
}

/// Whether a kept channel can be shown without asking Discord: young enough,
/// fully loaded, no gap since leaving, and holding the newest message there is.
fn still_current(trusted: bool, age: std::time::Duration, loaded: bool, newest: Option<Id>, latest: Option<Id>) -> bool {
    trusted && loaded && age <= CHANNEL_TTL && newest.is_some() && newest >= latest
}

pub enum StoreEvent {
    SignedOut,
    /// The member list changed. Presence ticks arrive constantly, so they
    /// reach the members panel through this and not through `notify`, which
    /// would redraw every page observing the store.
    MemberList,
}

/// A call being joined or under way. Both of the gateway's answers are needed first.
struct VoiceJoin {
    guild: Id,
    channel: Id,
    session: Option<String>,
    server: Option<(String, String)>,
    call: Option<cord_voice::VoiceCall>,
}

/// How a call looks, for the panel.
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceStatus {
    pub channel: Id,
    pub guild: Id,
    pub connected: bool,
    pub ping_ms: Option<u32>,
    pub deaf: bool,
}

/// The colour band of a ping: Discord's green, yellow and red.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PingBand {
    Good,
    Fair,
    Bad,
}

pub fn ping_band(ms: u32) -> PingBand {
    match ms {
        0..150 => PingBand::Good,
        150..270 => PingBand::Fair,
        _ => PingBand::Bad,
    }
}

pub struct Store {
    /// The call we're in, or joining.
    voice: Option<VoiceJoin>,
    /// Who's making sound in our call.
    speaking: std::collections::HashSet<Id>,
    /// Plays with a desktop notification, once it's been fetched.
    message_sound: Option<cord_voice::Clip>,
    core: Arc<Core>,
    toasts: Entity<Toasts>,
    cache: Cache,
    connection: Option<ConnectionState>,
    events: Option<Task<()>>,
    open: Option<Id>,
    history: HashMap<Id, History>,
    /// Channels left recently, oldest first.
    recent: Vec<RecentChannel>,
    nonce_seq: u64,
    send_chains: HashMap<Id, Task<()>>,
    /// The channel whose older page is being fetched, if any.
    loading_older: Option<Id>,
    loading_newer: Option<Id>,
    members: Option<MemberSub>,
    /// When each channel was last acked to Discord.
    acked: HashMap<Id, std::time::Instant>,
    /// In the open channel: messages after this one arrived unread.
    new_after: Option<Id>,
    /// Channel → who is typing there and since when.
    typing: HashMap<Id, Vec<(Id, std::time::Instant)>>,
}

/// How long a TYPING_START counts, as in Discord.
const TYPING_FOR: std::time::Duration = std::time::Duration::from_secs(10);

impl EventEmitter<StoreEvent> for Store {}

impl Store {
    pub fn new(core: Arc<Core>, toasts: Entity<Toasts>) -> Self {
        Store { speaking: Default::default(), message_sound: None, voice: None, core, toasts, cache: Cache::default(), connection: None, events: None, open: None, history: HashMap::new(), recent: Vec::new(), nonce_seq: 0, send_chains: HashMap::new(), loading_older: None, loading_newer: None, members: None, acked: HashMap::new(), typing: HashMap::new(), new_after: None }
    }

    pub fn core(&self) -> &Arc<Core> {
        &self.core
    }

    pub fn cache(&self) -> &Cache {
        &self.cache
    }

    /// `None` while signed out.
    pub fn connection(&self) -> Option<ConnectionState> {
        self.connection
    }

    pub fn start(&mut self, token: String, cx: &mut Context<Self>) {
        let mut events = self.core.start(token);
        self.cache = Cache::default();
        // Last run's servers and channels, until the real READY replaces them.
        if let Some(kept) = self.core.kept_ready() {
            self.cache.apply(kept);
        }
        self.connection = Some(ConnectionState::Connecting);
        // Replacing the task drops the previous session's listener.
        self.events = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                if this.update(cx, |store, cx| store.apply(event, cx)).is_err() {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// Subscribes to the member list of `channel` in `guild`, dropping the
    /// previous channel's list. Discord sends the first rows right away.
    pub fn subscribe_members(&mut self, guild: Id, channel: Id, cx: &mut Context<Self>) {
        // Threads and forum posts have no list of their own to ask for here:
        // they show who can see the channel they're in.
        let channel = self.cache.channel(channel).filter(|c| c.kind == cord_core::model::ChannelKind::Thread).and_then(|c| c.parent_id).unwrap_or(channel);
        if self.members.as_ref().is_some_and(|m| (m.guild, m.channel) == (guild, channel)) {
            return;
        }
        tracing::info!(%guild, %channel, "member list: subscribing");
        // Subscriptions are per server: let go of the previous server's.
        if let Some(previous) = self.members.take().filter(|m| m.guild != guild) {
            self.core.send_gateway(member_list::unsubscribe_payload(previous.guild));
        }
        self.cache.subscribe_member_list_for(guild, Some(channel));
        let sub = MemberSub { guild, channel, ranges: RangeTracker::default() };
        sub.send(&self.core);
        self.members = Some(sub);
        cx.emit(StoreEvent::MemberList);
        // Discord only starts a list it thinks is new to us, and asking for the same
        // rows again gets nothing. If nothing came, different rows do get an answer.
        cx.spawn(async move |this, cx| {
            for ranges in [[[0u32, 99u32], [100, 199]].as_slice(), [[0u32, 99u32]].as_slice()] {
                cx.background_executor().timer(std::time::Duration::from_millis(1500)).await;
                let asked = this.update(cx, |store, _| {
                    let same = store.members.as_ref().is_some_and(|m| (m.guild, m.channel) == (guild, channel));
                    if same && !store.cache.member_list_arrived() {
                        tracing::info!(%channel, "member list: nothing came, asking again");
                        store.core.send_gateway(member_list::subscribe_payload(guild, channel, ranges));
                        true
                    } else {
                        false
                    }
                });
                if !matches!(asked, Ok(true)) {
                    return;
                }
            }
        })
        .detach();
    }

    /// Row `index` of the member list is on screen: have the server keep it current.
    pub fn want_member_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(sub) = &mut self.members else { return };
        if let Want::Changed { evicted } = sub.ranges.want(index) {
            if let Some([start, end]) = evicted {
                self.cache.forget_member_rows(start, end);
            }
            sub.send(&self.core);
            cx.emit(StoreEvent::MemberList);
        }
    }

    /// Not looking at a server channel any more: tell the server, and drop the
    /// list and anything still arriving for it.
    pub fn leave_members(&mut self, cx: &mut Context<Self>) {
        if let Some(sub) = self.members.take() {
            self.core.send_gateway(member_list::unsubscribe_payload(sub.guild));
            self.cache.unsubscribe_member_list();
            cx.emit(StoreEvent::MemberList);
        }
    }

    /// Adds a channel the app learned about over REST (a DM just opened).
    pub fn add_channel(&mut self, channel: Channel, cx: &mut Context<Self>) {
        self.cache.apply(CoreEvent::ChannelUpsert(channel));
        cx.notify();
    }

    pub fn history(&self, channel: Id) -> Option<&History> {
        self.history.get(&channel)
    }

    /// Shows `channel`: keeps its live messages and loads the latest page.
    /// The previously open channel's messages are dropped (only open
    /// channels keep messages).
    /// For three seconds after opening a chat, measures how long the main thread goes
    /// without being able to do anything: that's how long typing would freeze for.
    fn watch_chat_opening(&self, channel: Id, cx: &mut Context<Self>) {
        const TICK: std::time::Duration = std::time::Duration::from_millis(4);
        const WATCH_FOR: std::time::Duration = std::time::Duration::from_secs(3);
        static LATEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let this_one = LATEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let held_before = crate::perf::private_mb();
        cx.spawn(async move |_, cx| {
            let began = std::time::Instant::now();
            let (mut worst, mut blocked, mut over_16, mut over_50) = (0u128, 0u128, 0u32, 0u32);
            while began.elapsed() < WATCH_FOR {
                let asked = std::time::Instant::now();
                cx.background_executor().timer(TICK).await;
                if LATEST.load(std::sync::atomic::Ordering::Relaxed) != this_one {
                    return;
                }
                let late = asked.elapsed().saturating_sub(TICK).as_millis();
                worst = worst.max(late);
                if late >= 16 {
                    over_16 += 1;
                    blocked += late;
                }
                if late >= 50 {
                    over_50 += 1;
                }
            }
            tracing::info!(%channel, worst_ms = worst, blocked_ms = blocked, over_16, over_50, held_before, held_after = crate::perf::private_mb(), "chat open");
        })
        .detach();
    }

    pub fn open_channel(&mut self, channel: Id, cx: &mut Context<Self>) {
        if self.open == Some(channel) {
            return;
        }
        self.watch_chat_opening(channel, cx);
        let now = std::time::Instant::now();
        if let Some(previous) = self.open.replace(channel) {
            // Kept a while, and kept current by the gateway: coming back is instant.
            self.cache.trim_to_tail(previous);
            self.recent.retain(|r| r.channel != previous);
            self.recent.push(RecentChannel { channel: previous, left: now, trusted: true });
        }
        let back = self.recent.iter().position(|r| r.channel == channel).map(|i| self.recent.remove(i));
        let expired: Vec<Id> = self.recent.iter().filter(|r| now.duration_since(r.left) > CHANNEL_TTL).map(|r| r.channel).collect();
        let excess = self.recent.len().saturating_sub(KEPT_CHANNELS);
        let oldest: Vec<Id> = self.recent.iter().take(excess).map(|r| r.channel).collect();
        for gone in expired.into_iter().chain(oldest) {
            self.recent.retain(|r| r.channel != gone);
            self.cache.forget_messages(gone);
            self.history.remove(&gone);
        }
        self.loading_older = None;
        self.loading_newer = None;
        // Remember where the unread part began, for the NEW line, before reading it.
        self.new_after = self.cache.unread_after(channel);
        self.cache.open_messages(channel);
        let newest = self.cache.messages(channel).and_then(|m| m.iter().last().map(|m| m.message.id));
        let latest = self.cache.channel(channel).and_then(|c| c.last_message_id);
        let usable = back.is_some_and(|r| {
            still_current(r.trusted, now.duration_since(r.left), self.history.get(&channel) == Some(&History::Loaded), newest, latest)
        });
        if !usable {
            self.load_history(channel, cx);
        }
        self.mark_read(channel, cx);
    }

    /// We've seen `channel` to its end: clear its unread state here and on
    /// Discord (only when there is something to clear).
    pub fn mark_read(&mut self, channel: Id, cx: &mut Context<Self>) {
        let unread = self.cache.is_unread(channel) || self.cache.mentions(channel) > 0;
        let Some(last) = self.cache.channel(channel).and_then(|c| c.last_message_id) else { return };
        self.cache.mark_read(channel);
        // Busy channels would ack every message; Discord's client batches too.
        let now = std::time::Instant::now();
        if !unread || self.acked.get(&channel).is_some_and(|at| now.duration_since(*at).as_secs() < 5) {
            return;
        }
        self.acked.insert(channel, now);
        let core = self.core.clone();
        cx.spawn(async move |_, _| {
            if let Err(error) = core.ack(channel, last).await {
                tracing::warn!(%error, "couldn't mark the channel read");
            }
        })
        .detach();
        cx.notify();
    }

    /// "Mark Unread": `message` becomes the first one unread.
    pub fn mark_unread(&mut self, channel: Id, message: Id, cx: &mut Context<Self>) {
        let before = Id(message.0.saturating_sub(1));
        self.cache.mark_unread_after(channel, before);
        self.acked.remove(&channel);
        if self.open == Some(channel) {
            self.new_after = Some(before);
        }
        let core = self.core.clone();
        cx.spawn(async move |_, _| {
            if let Err(error) = core.mark_unread_from(channel, before).await {
                tracing::warn!(%error, "couldn't mark the message unread");
            }
        })
        .detach();
        cx.notify();
    }

    pub fn reload_channel(&mut self, channel: Id, cx: &mut Context<Self>) {
        if self.open == Some(channel) {
            self.load_history(channel, cx);
        }
    }

    fn load_history(&mut self, channel: Id, cx: &mut Context<Self>) {
        self.history.insert(channel, History::Loading);
        cx.notify();
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.messages(channel, HISTORY_PAGE).await;
            let _ = this.update(cx, |store, cx| {
                if store.open != Some(channel) {
                    return; // the user moved on
                }
                let state = match result {
                    Ok(messages) => {
                        store.cache.set_history(channel, messages, HISTORY_PAGE as usize);
                        History::Loaded
                    }
                    Err(HttpError::Forbidden(_)) => History::Forbidden,
                    Err(HttpError::Unauthorized) => return, // AuthFailed signs out
                    Err(error) => History::Failed(error.to_string()),
                };
                store.history.insert(channel, state);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn is_loading_older(&self, channel: Id) -> bool {
        self.loading_older == Some(channel)
    }

    /// Fetches the page above the oldest message held, when there is one and
    /// the channel may hold more. One page at a time.
    pub fn load_older(&mut self, channel: Id, cx: &mut Context<Self>) {
        if self.loading_older.is_some() || self.open != Some(channel) || self.history.get(&channel) != Some(&History::Loaded) {
            return;
        }
        let has_older = self.cache.messages(channel).is_some_and(|m| m.has_older);
        let Some(before) = self.cache.oldest(channel) else { return };
        if !has_older || self.cache.scrollback_full(channel) {
            return;
        }
        self.loading_older = Some(channel);
        cx.notify();
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.messages_before(channel, before, HISTORY_PAGE).await;
            let _ = this.update(cx, |store, cx| {
                store.loading_older = None;
                if store.open == Some(channel) {
                    match result {
                        Ok(messages) => store.cache.prepend_older(channel, messages, HISTORY_PAGE as usize),
                        Err(HttpError::Unauthorized) => {} // AuthFailed signs out
                        Err(error) => store.toasts.update(cx, |_, cx| {
                            cx.emit(Toast::error("Couldn't load older messages", error.to_string()))
                        }),
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Whether `channel` shows a window back in its history, not its end.
    pub fn is_detached(&self, channel: Id) -> bool {
        self.cache.has_newer(channel)
    }

    pub fn is_loading_newer(&self, channel: Id) -> bool {
        self.loading_newer == Some(channel)
    }

    /// Shows the messages around `target`, which is too far back to page to: one request
    /// instead of a page for every fifty in between. Done once it's replaced the view.
    pub fn load_around(&mut self, channel: Id, target: Id, cx: &mut Context<Self>) {
        if self.loading_older.is_some() || self.open != Some(channel) {
            return;
        }
        self.loading_older = Some(channel);
        cx.notify();
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.messages_around(channel, target, HISTORY_PAGE).await;
            let _ = this.update(cx, |store, cx| {
                store.loading_older = None;
                if store.open == Some(channel) {
                    match result {
                        Ok(messages) => store.cache.set_around(channel, messages, HISTORY_PAGE as usize),
                        Err(HttpError::Unauthorized) => {} // AuthFailed signs out
                        Err(error) => store.toasts.update(cx, |_, cx| cx.emit(Toast::error("Couldn't load that part of the channel", error.to_string()))),
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Fetches the page below the newest message held, in a window back in the history.
    pub fn load_newer(&mut self, channel: Id, cx: &mut Context<Self>) {
        if self.loading_newer.is_some() || self.loading_older.is_some() || self.open != Some(channel) || !self.cache.has_newer(channel) {
            return;
        }
        let Some(after) = self.cache.newest(channel) else { return };
        self.loading_newer = Some(channel);
        cx.notify();
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.messages_after(channel, after, HISTORY_PAGE).await;
            let _ = this.update(cx, |store, cx| {
                store.loading_newer = None;
                if store.open == Some(channel) {
                    match result {
                        Ok(messages) => store.cache.append_newer(channel, messages, HISTORY_PAGE as usize),
                        Err(HttpError::Unauthorized) => {}
                        Err(error) => store.toasts.update(cx, |_, cx| cx.emit(Toast::error("Couldn't load newer messages", error.to_string()))),
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Leaves a window back in the history for the newest messages again.
    pub fn return_to_present(&mut self, channel: Id, cx: &mut Context<Self>) {
        if self.open == Some(channel) && self.cache.has_newer(channel) {
            self.load_history(channel, cx);
        }
    }

    /// Back at the bottom of `channel`: let go of the scrollback.
    pub fn trim_channel(&mut self, channel: Id, cx: &mut Context<Self>) {
        let over = self.cache.messages(channel).is_some_and(|m| m.len() > cord_core::cache::MESSAGE_CAP);
        if over && self.loading_older.is_none() {
            self.cache.trim_to_tail(channel);
            cx.notify();
        }
    }

    /// Shows the message at once as pending, then sends it. On failure it
    /// stays in the chat marked failed (Retry/Delete arrive in plan 1b).
    pub fn send(&mut self, channel: Id, content: String, reply: Option<(Id, bool)>, files: Vec<cord_core::http::Upload>, cx: &mut Context<Self>) {
        let Some(me) = self.cache.me().cloned() else { return };
        // Sending from a window back in the history goes to the end of the channel, as Discord's does.
        self.return_to_present(channel, cx);
        // A snowflake for "now", made unique per send within the same millisecond.
        self.nonce_seq = self.nonce_seq.wrapping_add(1);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
        let local_id = Id(Id::from_timestamp_ms(now).0 | (self.nonce_seq & 0x3f_ffff));
        let nonce = local_id.to_string();
        // As it'll be once it's sent: who's being answered, and the @ if they're pinged.
        let answering = reply.and_then(|(id, ping)| {
            let original = self.cache.messages(channel)?.iter().find(|m| m.message.id == id)?.message.clone();
            Some((original, ping))
        });
        let pinged: Vec<_> = answering.iter().filter(|(_, ping)| *ping).map(|(original, _)| original.author.clone()).collect();
        let answered = answering.map(|(original, _)| Box::new(original));
        self.cache.add_pending(Message {
            pinned: false,
            id: local_id,
            channel_id: channel,
            guild_id: None,
            author: me,
            content: content.clone(),
            timestamp: String::new(),
            edited_timestamp: None, poll: None, interaction: None, interaction_metadata: None, message_snapshots: Vec::new(),
            nonce: Some(nonce.clone()),
            // No URL yet: these show as uploading until the sent message replaces them.
            attachments: files
                .iter()
                .enumerate()
                .map(|(i, file)| cord_core::rich::Attachment {
                    id: Id(i as u64),
                    filename: file.name.clone(),
                    url: String::new(),
                    proxy_url: String::new(),
                    content_type: Some(file.mime.clone()),
                    width: None,
                    height: None,
                    size: file.bytes.len() as u64,
                    duration_secs: None,
                    waveform: None,
                })
                .collect(),
            member: None,
            // A reply shows as one from the moment it's sent, header and all.
            kind: if reply.is_some() { 19 } else { 0 },
            flags: 0,
            embeds: Vec::new(),
            components: Vec::new(),
            call: None,
            message_reference: reply.map(|(id, _)| cord_core::rich::MessageReference { message_id: Some(id), channel_id: Some(channel), guild_id: None }),
            referenced_message: answered,
            sticker_items: Vec::new(),
            reactions: Vec::new(),
            mention_everyone: false,
            mention_roles: Vec::new(),
            mentions: pinged,
        });
        for (i, file) in files.iter().enumerate() {
            crate::uploads::track(local_id, i as u64, file.sent.clone());
        }
        cx.notify();

        let core = self.core.clone();
        // Sends within a channel run one after another so click order is server
        // order; other channels are independent.
        let previous = self.send_chains.remove(&channel);
        self.send_chains.insert(channel, cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let result = core.send_message(channel, content, nonce.clone(), reply, files).await;
            crate::uploads::finish(local_id, result.is_ok());
            let _ = this.update(cx, |store, cx| {
                match result {
                    // Same as the gateway echo; whichever comes second is ignored.
                    Ok(message) => store.cache.apply(CoreEvent::MessageCreate(message)),
                    Err(error) => {
                        store.cache.mark_failed(channel, &nonce);
                        if !matches!(error, HttpError::Unauthorized) {
                            store.toasts.update(cx, |_, cx| cx.emit(Toast::error("Message not sent", error.to_string())));
                        }
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Adds or removes the user's reaction: shown at once, then sent. If
    /// Discord refuses, the reaction goes back as it was and a toast says
    /// why. Clicks on a pill are ignored while its last change is in flight.
    /// Saves an edit. The message changes when the gateway says it has.
    pub fn edit_message(&mut self, channel: Id, message: Id, content: String, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.edit_message(channel, message, content).await;
            Self::report(this, result, "Couldn't edit the message", cx);
        })
        .detach();
    }

    /// Deletes a message. It goes when the gateway says it has.
    pub fn delete_message(&mut self, channel: Id, message: Id, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.delete_message(channel, message).await;
            Self::report(this, result, "Couldn't delete the message", cx);
        })
        .detach();
    }

    pub fn pin_message(&mut self, channel: Id, message: Id, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.pin_message(channel, message).await;
            Self::report(this, result, "Couldn't pin the message", cx);
        })
        .detach();
    }

    /// Dismisses an ephemeral message: nobody else has it, so it's just gone from here.
    pub fn dismiss_message(&mut self, channel: Id, message: Id, cx: &mut Context<Self>) {
        self.cache.apply(CoreEvent::MessageDelete { id: message, channel_id: channel });
        cx.notify();
    }

    /// Joins a voice channel, just listening so far.
    pub fn join_voice(&mut self, guild: Id, channel: Id, cx: &mut Context<Self>) {
        if self.voice.as_ref().is_some_and(|v| v.channel == channel) {
            return;
        }
        self.voice = Some(VoiceJoin { guild, channel, session: None, server: None, call: None });
        self.core.send_gateway(serde_json::json!({"op": 4, "d": {
            "guild_id": guild.to_string(),
            "channel_id": channel.to_string(),
            "self_mute": true,
            "self_deaf": false,
            "self_video": false,
        }}));
        // The panel's ping and state change without any event to say so.
        cx.spawn(async move |this, cx| {
            let mut last = None;
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(100)).await;
                let Ok(keep_going) = this.update(cx, |store, cx| {
                    // Only a change is worth a redraw.
                    let speaking: std::collections::HashSet<Id> = store.voice.as_ref().and_then(|v| v.call.as_ref()).map_or_else(Default::default, |call| call.speaking().into_iter().map(Id).collect());
                    if speaking != store.speaking {
                        store.speaking = speaking;
                        cx.notify();
                    }
                    // It ended for good, not by us: no panel for a call that's gone.
                    if store.voice.as_ref().and_then(|v| v.call.as_ref()).is_some_and(|c| c.stats().state() == cord_voice::State::Closed) {
                        store.voice = None;
                        cx.notify();
                    }
                    let now = store.voice_status();
                    if now != last {
                        last = now.clone();
                        cx.notify();
                    }
                    now.is_some()
                }) else {
                    return;
                };
                if !keep_going {
                    return;
                }
            }
        })
        .detach();
        cx.notify();
    }

    pub fn leave_voice(&mut self, cx: &mut Context<Self>) {
        let Some(voice) = self.voice.take() else { return };
        self.core.send_gateway(serde_json::json!({"op": 4, "d": {
            "guild_id": voice.guild.to_string(),
            "channel_id": null,
            "self_mute": false,
            "self_deaf": false,
            "self_video": false,
        }}));
        cx.notify();
    }

    /// How loud someone is for us, here and from now on.
    pub fn set_user_volume(&mut self, user: Id, volume: f32) {
        crate::places::set_volume(user, volume);
        self.apply_gain(user);
    }

    pub fn set_user_muted(&mut self, user: Id, muted: bool) {
        crate::places::set_muted(user, muted);
        self.apply_gain(user);
    }

    fn apply_gain(&self, user: Id) {
        if let Some(call) = self.voice.as_ref().and_then(|v| v.call.as_ref()) {
            call.set_user_gain(user.0, crate::places::gain_of(&crate::places::get(), user));
        }
    }

    /// Stops or starts hearing the call.
    pub fn toggle_deaf(&mut self, cx: &mut Context<Self>) {
        let Some(voice) = &self.voice else { return };
        let Some(call) = &voice.call else { return };
        let deaf = !call.is_deaf();
        call.set_deaf(deaf);
        call.play_sound(if deaf { cord_voice::Sound::Deafened } else { cord_voice::Sound::Undeafened });
        self.core.send_gateway(serde_json::json!({"op": 4, "d": {
            "guild_id": voice.guild.to_string(),
            "channel_id": voice.channel.to_string(),
            "self_mute": true,
            "self_deaf": deaf,
            "self_video": false,
        }}));
        cx.notify();
    }

    pub fn speaking(&self) -> &std::collections::HashSet<Id> {
        &self.speaking
    }

    pub fn voice_status(&self) -> Option<VoiceStatus> {
        let voice = self.voice.as_ref()?;
        let call = voice.call.as_ref();
        Some(VoiceStatus {
            channel: voice.channel,
            guild: voice.guild,
            connected: call.is_some_and(|c| c.stats().state() == cord_voice::State::Connected),
            ping_ms: call.and_then(|c| c.stats().ping_ms()),
            deaf: call.is_some_and(cord_voice::VoiceCall::is_deaf),
        })
    }

    /// Fetches Discord's own sounds for the call, the way its web app does: public
    /// files, once, then kept on disk. Until they're here, or if they can't be got,
    /// stand-ins play.
    fn load_call_sounds(&self, cx: &mut Context<Self>) {
        use cord_voice::Sound;
        const SOUNDS: [(Sound, &str); 7] = [
            (Sound::StreamStarted, "abe52a3c92953edb"),
            (Sound::StreamEnded, "cbff07b5a83e0321"),
            (Sound::Connected, "b135ff6c8e091b43"),
            (Sound::SomeoneJoined, "b135ff6c8e091b43"),
            (Sound::SomeoneLeft, "7b9a183742515fc2"),
            (Sound::Deafened, "529ff198eac567af"),
            (Sound::Undeafened, "b150f03c89944403"),
        ];
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            for (sound, name) in SOUNDS {
                let Some(clip) = sound_file(core.clone(), name, cx.background_executor().clone()).await else { continue };
                let still_here = this.update(cx, |store, _| {
                    if let Some(call) = store.voice.as_ref().and_then(|v| v.call.as_ref()) {
                        call.set_sound(sound, clip.clone());
                    }
                });
                if still_here.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// Starts the call, once the gateway's said both where it is and who we are.
    fn start_voice(&mut self, cx: &mut Context<Self>) {
        let Some(me) = self.cache.me().map(|u| u.id) else { return };
        let runtime = self.core.runtime().clone();
        let Some(voice) = &mut self.voice else { return };
        let (Some(session), Some((endpoint, token))) = (voice.session.clone(), voice.server.clone()) else { return };
        // A new server for the same call replaces the old connection.
        let call = cord_voice::VoiceCall::join(
            cord_voice::JoinInfo {
                endpoint,
                server_id: voice.guild.0,
                channel_id: voice.channel.0,
                user_id: me.0,
                session_id: session,
                token,
            },
            &runtime,
        );
        let places = crate::places::get();
        for user in places.volumes.keys().chain(&places.muted) {
            call.set_user_gain(user.0, crate::places::gain_of(&places, *user));
        }
        call.play_sound(cord_voice::Sound::Joining);
        voice.call = Some(call);
        self.load_call_sounds(cx);
    }

    /// One at a time, stopping at the first that fails.
    pub fn add_to_group(&mut self, channel: Id, users: Vec<Id>, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let mut result = Ok(());
            for user in users {
                result = core.add_recipient(channel, user).await;
                if result.is_err() {
                    break;
                }
            }
            Self::report(this, result, "Couldn't add them to the group", cx);
        })
        .detach();
    }

    pub fn add_friend(&mut self, user: Id, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.add_friend(user).await;
            Self::report(this, result, "Couldn't send the friend request", cx);
        })
        .detach();
    }

    pub fn remove_friend(&mut self, user: Id, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.remove_friend(user).await;
            Self::report(this, result, "Couldn't remove them", cx);
        })
        .detach();
    }

    pub fn kick_member(&mut self, guild: Id, user: Id, reason: String, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.kick_member(guild, user, reason).await;
            Self::report(this, result, "Couldn't kick them", cx);
        })
        .detach();
    }

    pub fn ban_member(&mut self, guild: Id, user: Id, delete_seconds: u32, reason: String, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.ban_member(guild, user, delete_seconds, reason).await;
            Self::report(this, result, "Couldn't ban them", cx);
        })
        .detach();
    }

    pub fn timeout_member(&mut self, guild: Id, user: Id, until: Option<String>, reason: String, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.timeout_member(guild, user, until, reason).await;
            Self::report(this, result, "Couldn't time them out", cx);
        })
        .detach();
    }

    pub fn set_nickname(&mut self, guild: Id, user: Id, me: bool, nick: String, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.set_nickname(guild, user, me, nick).await;
            Self::report(this, result, "Couldn't change the nickname", cx);
        })
        .detach();
    }

    pub fn block_user(&mut self, user: Id, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.block_user(user).await;
            Self::report(this, result, "Couldn't block them", cx);
        })
        .detach();
    }

    pub fn set_friend_nickname(&mut self, user: Id, nick: String, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.set_friend_nickname(user, nick).await;
            Self::report(this, result, "Couldn't change the nickname", cx);
        })
        .detach();
    }

    pub fn set_note(&mut self, user: Id, note: String, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.set_note(user, note).await;
            Self::report(this, result, "Couldn't save the note", cx);
        })
        .detach();
    }

    pub fn forward_message(&mut self, to: Id, from: Id, message: Id, guild: Option<Id>, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.forward_message(to, from, message, guild).await;
            Self::report(this, result, "Couldn't forward the message", cx);
        })
        .detach();
    }

    /// "Mute Conversation": shown at once, then told to Discord.
    pub fn set_conversation_muted(&mut self, channel: Id, muted: bool, cx: &mut Context<Self>) {
        self.cache.set_muted(channel, muted);
        cx.notify();
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.mute_conversation(channel, muted).await;
            Self::report(this, result, "Couldn't change the mute", cx);
        })
        .detach();
    }

    pub fn rename_group(&mut self, channel: Id, name: String, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.rename_group(channel, name).await;
            Self::report(this, result, "Couldn't rename the group", cx);
        })
        .detach();
    }

    pub fn create_thread(&mut self, channel: Id, message: Id, name: String, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.create_thread(channel, message, name).await;
            Self::report(this, result, "Couldn't start the thread", cx);
        })
        .detach();
    }

    pub fn remove_from_group(&mut self, channel: Id, user: Id, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.remove_recipient(channel, user).await;
            Self::report(this, result, "Couldn't remove them from the group", cx);
        })
        .detach();
    }

    pub fn make_group_owner(&mut self, channel: Id, user: Id, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.set_group_owner(channel, user).await;
            Self::report(this, result, "Couldn't change the group's owner", cx);
        })
        .detach();
    }

    pub fn unpin_message(&mut self, channel: Id, message: Id, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.unpin_message(channel, message).await;
            Self::report(this, result, "Couldn't unpin the message", cx);
        })
        .detach();
    }

    /// A failed action says so; a rejected token signs out on its own.
    fn report(this: gpui_kit::WeakEntity<Self>, result: Result<(), HttpError>, title: &'static str, cx: &mut gpui_kit::AsyncApp) {
        let Err(error) = result else { return };
        if matches!(error, HttpError::Unauthorized) {
            return;
        }
        let _ = this.update(cx, |store, cx| {
            store.toasts.update(cx, |_, cx| cx.emit(Toast::error(title, error.to_string())));
        });
    }

    pub fn set_reaction(&mut self, channel: Id, message: Id, emoji: ReactionEmoji, on: bool, cx: &mut Context<Self>) {
        if !self.cache.begin_my_reaction(channel, message, &emoji, on) {
            return;
        }
        cx.notify();
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.set_reaction(channel, message, &emoji, on).await;
            let _ = this.update(cx, |store, cx| {
                store.cache.settle_my_reaction(channel, message, &emoji, result.is_ok());
                if let Err(error) = result {
                    if !matches!(error, HttpError::Unauthorized) {
                        store.toasts.update(cx, |_, cx| cx.emit(Toast::error("Couldn't change the reaction", error.to_string())));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Bots aren't friends, so nothing tells us their status on its own. A server we
    /// share knows: one request per bot in our DMs, once per connection.
    fn ask_bot_presences(&self, cx: &mut Context<Self>) {
        const MOST: usize = 20;
        let bots: Vec<Id> = self
            .cache
            .private_channels()
            .into_iter()
            .filter(|c| c.recipient_ids.len() == 1)
            .filter_map(|c| c.recipient_ids.first().copied())
            .filter(|id| self.cache.user(*id).is_some_and(|u| u.bot) && self.cache.presence(*id).is_none())
            .take(MOST)
            .collect();
        if bots.is_empty() {
            return;
        }
        let core = self.core.clone();
        cx.spawn(async move |_, _| {
            for bot in bots {
                let Ok(profile) = core.user_profile(bot, None).await else { continue };
                if let Some(guild) = profile.mutual_guilds.as_ref().and_then(|g| g.first()) {
                    core.request_presences(*guild, &[bot]);
                }
            }
        })
        .detach();
    }

    /// "name (#channel, CATEGORY)", "name (group)", or just their name in a DM.
    fn notification_title(&self, name: &str, channel: Id) -> String {
        let Some(place) = self.cache.channel(channel) else { return name.to_string() };
        let label = crate::nav::channel_label(&self.cache, place);
        if place.guild_id.is_some() {
            return match place.parent_id.and_then(|id| self.cache.channel(id)).and_then(|parent| parent.name.as_deref()) {
                Some(category) => format!("{name} (#{label}, {category})"),
                None => format!("{name} (#{label})"),
            };
        }
        if place.kind == cord_core::model::ChannelKind::GroupDm { format!("{name} ({label})") } else { name.to_string() }
    }

    /// Someone put a new reaction on a message of ours. Adding to one already there
    /// isn't news. Only for messages we're holding: asking Discord for each would be a
    /// request per reaction.
    fn notify_of_reaction(&self, channel: Id, message: Id, user: Id, emoji: &cord_core::model::ReactionEmoji, cx: &mut Context<Self>) {
        let Some(me) = self.cache.me().map(|u| u.id) else { return };
        if user == me {
            return;
        }
        let Some(m) = self.cache.messages(channel).and_then(|list| list.iter().find(|m| m.message.id == message)) else { return };
        if m.message.author.id != me || m.message.reactions.iter().any(|r| r.emoji.id == emoji.id && r.emoji.name == emoji.name) {
            return;
        }
        let status = self.cache.presence(me).map(|p| p.status);
        let looking = self.open == Some(channel) && WINDOW_ACTIVE.load(std::sync::atomic::Ordering::Relaxed);
        if !should_notify(status, false, true, self.cache.is_muted(channel), 0, looking) {
            return;
        }
        let Some(who) = self.cache.user(user) else { return };
        let guild = self.cache.channel(channel).and_then(|c| c.guild_id);
        let name = self.cache.display_name(guild, who).to_string();
        let shown = match (emoji.id, emoji.name.as_deref()) {
            (Some(_), Some(name)) => format!(":{name}:"),
            (_, Some(name)) => name.to_string(),
            _ => "an emoji".to_string(),
        };
        let content = format!("Reacted {shown} to your \"{}\"", self.cache.readable_text(&m.message));
        let (title, picture, core) = (self.notification_title(&name, channel), self.cache.avatar_url(guild, who, 128), self.core.clone());
        if let Some(sound) = &self.message_sound {
            cord_voice::play_once(sound.clone());
        }
        cx.spawn(async move |_, cx| {
            let path = crate::notifications::picture_path(&picture);
            let icon = if path.exists() {
                Some(path)
            } else {
                let bytes = core.cdn_bytes(picture).await.ok();
                cx.background_executor().spawn(async move { crate::notifications::keep_picture(path, bytes?) }).await
            };
            crate::notifications::show(&title, &content, false, icon);
        })
        .detach();
    }

    /// A desktop notification for a message meant for us, if our status allows it.
    fn notify_of(&self, m: &Message, cx: &mut Context<Self>) {
        let me = self.cache.me().map(|u| u.id);
        let status = me.and_then(|me| self.cache.presence(me)).map(|p| p.status);
        let looking = self.open == Some(m.channel_id) && WINDOW_ACTIVE.load(std::sync::atomic::Ordering::Relaxed);
        // Private conversations are the ones people expect to hear from: say why not.
        if self.cache.channel(m.channel_id).is_some_and(|c| c.guild_id.is_none()) && me != Some(m.author.id) {
            tracing::info!(group = self.cache.channel(m.channel_id).is_some_and(|c| c.kind == cord_core::model::ChannelKind::GroupDm), muted = self.cache.is_muted(m.channel_id), pinged = self.cache.pings_me(m), looking, ?status, flags = m.flags, "notification decision");
        }
        if !should_notify(status, me == Some(m.author.id), self.cache.pings_me(m), self.cache.is_muted(m.channel_id), m.flags, looking) {
            return;
        }
        if let Some(sound) = &self.message_sound {
            cord_voice::play_once(sound.clone());
        }
        let channel = self.cache.channel(m.channel_id);
        let guild = channel.and_then(|c| c.guild_id);
        let name = self.cache.display_name(guild, &m.author);
        let group = channel.filter(|c| c.kind == cord_core::model::ChannelKind::GroupDm);
        let title = self.notification_title(&name, m.channel_id);
        // The group's picture in a group; else their server profile's, which the
        // message itself may be the first to tell us about; else their own.
        let picture = match (group.and_then(|c| Some((c.id, c.icon.as_ref()?))), guild.zip(m.member.as_ref().and_then(|member| member.avatar.as_deref()).filter(|a| !a.is_empty()))) {
            (Some((id, hash)), _) => format!("{}/channel-icons/{id}/{hash}.png?size=128", cord_core::cdn::CDN),
            (None, Some((guild, hash))) => cord_core::cdn::guild_avatar_url(guild, m.author.id, hash, 128),
            (None, None) => self.cache.avatar_url(guild, &m.author, 128),
        };
        let (core, content, attachment) = (self.core.clone(), self.cache.readable_text(m), !m.attachments.is_empty());
        cx.spawn(async move |_, cx| {
            let path = crate::notifications::picture_path(&picture);
            let icon = if path.exists() {
                Some(path)
            } else {
                let bytes = core.cdn_bytes(picture).await.ok();
                cx.background_executor().spawn(async move { crate::notifications::keep_picture(path, bytes?) }).await
            };
            crate::notifications::show(&title, &content, attachment, icon);
        })
        .detach();
    }

    /// Marks whole servers read, here and on Discord.
    pub fn mark_guilds_read(&mut self, guilds: &[Id], cx: &mut Context<Self>) {
        let channels: Vec<(Id, Id)> = guilds.iter().flat_map(|g| self.cache.unread_channels(*g)).collect();
        if channels.is_empty() {
            return;
        }
        for (channel, _) in &channels {
            self.cache.mark_read(*channel);
        }
        cx.notify();
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.ack_bulk(channels).await;
            Self::report(this, result, "Couldn't mark it as read", cx);
        })
        .detach();
    }

    pub fn close_dm(&mut self, channel: Id, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.close_dm(channel).await;
            Self::report(this, result, "Couldn't close the DM", cx);
        })
        .detach();
    }

    /// Votes on a poll, showing it at once and undoing it if Discord says no.
    pub fn vote_poll(&mut self, channel: Id, message: Id, answers: Vec<u32>, cx: &mut Context<Self>) {
        let Some(before) = self.cache.set_my_poll_votes(channel, message, &answers) else { return };
        cx.notify();
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let result = core.vote_poll(channel, message, answers).await;
            if result.is_err() {
                let _ = this.update(cx, |store, cx| {
                    store.cache.set_my_poll_votes(channel, message, &before);
                    cx.notify();
                });
            }
            Self::report(this, result, "Couldn't save your vote", cx);
        })
        .detach();
    }

    fn apply(&mut self, event: CoreEvent, cx: &mut Context<Self>) {
        if self.connection.is_none() {
            return; // late events from a session that was signed out
        }
        match event {
            CoreEvent::Connection(state) => self.connection = Some(state),
            CoreEvent::AuthFailed => {
                self.toasts.update(cx, |_, cx| {
                    cx.emit(Toast::warning("Signed out", "Discord no longer accepts this session. Log in again."))
                });
                self.sign_out(cx);
                return;
            }
            CoreEvent::Ready(ready) => {
                self.cache.apply(CoreEvent::Ready(ready));
                let (core, background) = (self.core.clone(), cx.background_executor().clone());
                cx.spawn(async move |_, _| crate::discord_icons::refresh(core, background).await).detach();
                self.ask_bot_presences(cx);
                if self.message_sound.is_none() {
                    let (core, background) = (self.core.clone(), cx.background_executor().clone());
                    cx.spawn(async move |this, cx| {
                        let clip = sound_file(core, "3ed22d14f3c30bc4", background).await;
                        let _ = this.update(cx, |store, _| store.message_sound = clip);
                    })
                    .detach();
                }
                // Anything may have been missed while away: fetch these again when opened.
                for kept in &mut self.recent {
                    kept.trusted = false;
                }
                // A fresh session starts with no subscriptions.
                if let Some(sub) = &self.members {
                    sub.send(&self.core);
                }
                // A fresh READY may follow a gap in the gateway session; backfill
                // what the open channel missed (`set_history` merges live and pending messages).
                if let Some(channel) = self.open {
                    self.load_history(channel, cx);
                }
            }
            CoreEvent::VoiceServer(server) => {
                if let (Some(voice), Some(endpoint)) = (&mut self.voice, server.endpoint) {
                    voice.server = Some((endpoint, server.token));
                    self.start_voice(cx);
                    cx.notify();
                }
                return;
            }
            CoreEvent::VoiceStateUpdate { guild_id, state, member } => {
                if self.cache.me().is_some_and(|me| me.id == state.user_id) {
                    match &mut self.voice {
                        // Moved, or removed from the call.
                        Some(voice) if state.channel_id.is_none() => {
                            let _ = voice;
                            self.voice = None;
                        }
                        Some(voice) => {
                            if let Some(channel) = state.channel_id {
                                voice.channel = channel;
                            }
                            if voice.session != state.session_id {
                                voice.session = state.session_id.clone();
                                if voice.call.is_none() {
                                    self.start_voice(cx);
                                }
                            }
                        }
                        None => {}
                    }
                }
                // Someone in our call starting or stopping a screen share.
                let ours = self.voice.as_ref().map(|v| v.channel);
                if ours.is_some() && state.channel_id == ours && self.cache.me().is_some_and(|me| me.id != state.user_id) {
                    let was = self.cache.voice_state(guild_id, state.user_id).is_some_and(|v| v.self_stream);
                    if was != state.self_stream
                        && let Some(call) = self.voice.as_ref().and_then(|v| v.call.as_ref())
                    {
                        call.play_sound(if state.self_stream { cord_voice::Sound::StreamStarted } else { cord_voice::Sound::StreamEnded });
                    }
                }
                self.cache.apply(CoreEvent::VoiceStateUpdate { guild_id, state, member });
            }
            CoreEvent::MemberListUpdate(update) => {
                self.cache.apply(CoreEvent::MemberListUpdate(update));
                cx.emit(StoreEvent::MemberList);
                return;
            }
            CoreEvent::TypingStart { channel_id, guild_id, user_id, member } => {
                let me = self.cache.me().map(|u| u.id);
                self.cache.apply(CoreEvent::TypingStart { channel_id, guild_id, user_id, member });
                if Some(user_id) == me {
                    return;
                }
                let list = self.typing.entry(channel_id).or_default();
                list.retain(|(u, _)| *u != user_id);
                list.push((user_id, std::time::Instant::now()));
                if self.open != Some(channel_id) {
                    return;
                }
                // Redraw when this one runs out.
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(TYPING_FOR).await;
                    let _ = this.update(cx, |_, cx| cx.notify());
                })
                .detach();
            }
            CoreEvent::MessageCreate(m) => {
                let channel = m.channel_id;
                if let Some(list) = self.typing.get_mut(&channel) {
                    list.retain(|(u, _)| *u != m.author.id);
                }
                self.notify_of(&m, cx);
                self.cache.apply(CoreEvent::MessageCreate(m));
                if self.open == Some(channel) {
                    self.mark_read(channel, cx);
                }
            }
            CoreEvent::ReactionAdd { channel_id, message_id, user_id, emoji, burst } => {
                // Before it's counted, so that "new" can be told.
                self.notify_of_reaction(channel_id, message_id, user_id, &emoji, cx);
                self.cache.apply(CoreEvent::ReactionAdd { channel_id, message_id, user_id, emoji, burst });
            }
            other => self.cache.apply(other),
        }
        cx.notify();
    }

    /// The last message read before the open channel was opened, when it had unread ones.
    pub fn new_after(&self) -> Option<Id> {
        self.new_after
    }

    /// Who is typing in `channel` right now, longest first.
    pub fn typing(&self, channel: Id) -> Vec<Id> {
        let now = std::time::Instant::now();
        self.typing
            .get(&channel)
            .map(|l| l.iter().filter(|(_, at)| now.duration_since(*at) < TYPING_FOR).map(|(u, _)| *u).collect())
            .unwrap_or_default()
    }

    pub fn sign_out(&mut self, cx: &mut Context<Self>) {
        tracing::info!("signing out");
        crate::drafts::forget_all();
        crate::image_disk::forget_all();
        crate::places::forget();
        self.voice = None;
        // Stops the gateway (which ends the event loop above) and clears the keyring.
        self.core.sign_out();
        self.cache = Cache::default();
        self.connection = None;
        self.open = None;
        self.history.clear();
        self.send_chains.clear();
        self.loading_older = None;
        self.members = None;
        cx.emit(StoreEvent::SignedOut);
        cx.notify();
    }
}

#[cfg(test)]
mod notify_tests {
    use super::*;

    #[test]
    fn kept_channels_are_used_only_while_current() {
        let young = std::time::Duration::from_secs(30);
        let (a, b) = (Some(Id(5)), Some(Id(6)));
        assert!(still_current(true, young, true, a, a));
        assert!(still_current(true, young, true, b, a), "we hold newer than it knows of");
        assert!(!still_current(false, young, true, a, a), "reconnected since");
        assert!(!still_current(true, CHANNEL_TTL * 2, true, a, a), "too old");
        assert!(!still_current(true, young, false, a, a), "never finished loading");
        assert!(!still_current(true, young, true, a, b), "a message came in unseen");
        assert!(!still_current(true, young, true, None, None), "nothing held");
    }

    #[test]
    fn notifies_by_status_and_place() {
        let ask = |status, mine, pinged, muted, flags, looking| should_notify(status, mine, pinged, muted, flags, looking);
        assert!(ask(Some(Status::Online), false, true, false, 0, false));
        assert!(ask(Some(Status::Idle), false, true, false, 0, false));
        assert!(ask(None, false, true, false, 0, false));
        assert!(!ask(Some(Status::Dnd), false, true, false, 0, false), "do not disturb");
        assert!(!ask(Some(Status::Online), true, true, false, 0, false), "our own");
        assert!(!ask(Some(Status::Online), false, false, false, 0, false), "not for us");
        assert!(!ask(Some(Status::Online), false, true, true, 0, false), "muted");
        assert_eq!(ping_band(84), PingBand::Good);
        assert_eq!(ping_band(149), PingBand::Good);
        assert_eq!(ping_band(150), PingBand::Fair);
        assert_eq!(ping_band(269), PingBand::Fair);
        assert_eq!(ping_band(270), PingBand::Bad);
        assert!(!ask(Some(Status::Online), false, true, false, 1 << 12, false), "@silent");
        assert!(!ask(Some(Status::Online), false, true, false, 0, true), "already looking");
    }
}
