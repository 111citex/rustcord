//! Discord's lazy member list. The client subscribes to index ranges of the
//! list for one channel; the server answers with a SYNC of those ranges and
//! then keeps them current with INSERT, UPDATE, DELETE and INVALIDATE ops.
//! The list interleaves group headers ("Online", a role, "Offline") with the
//! members under them, so every index addresses either one.

use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

use crate::Id;
use crate::model::{User, lenient_vec};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    Online,
    Idle,
    Dnd,
    #[default]
    Offline,
}

impl<'de> Deserialize<'de> for Status {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match Option::<String>::deserialize(d)?.as_deref() {
            Some("online") => Status::Online,
            Some("idle") => Status::Idle,
            Some("dnd") => Status::Dnd,
            _ => Status::Offline,
        })
    }
}

/// A group header. `id` is "online", "offline" or a role id.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Group {
    pub id: String,
    #[serde(default)]
    pub count: u32,
}

impl Group {
    /// The role this group stands for, if it is one.
    pub fn role(&self) -> Option<Id> {
        self.id.parse().ok().map(Id)
    }
}

/// A member as the list shows them.
#[derive(Clone, Debug, PartialEq)]
pub struct ListMember {
    pub user: User,
    pub nick: Option<String>,
    /// Server avatar hash.
    pub avatar: Option<String>,
    pub roles: Vec<Id>,
    pub status: Status,
    /// Custom status, else the first activity ("Playing Rust").
    pub activity: Option<String>,
    /// What kind of line `activity` is, for the icon drawn before it.
    pub activity_kind: Option<ActivityKind>,
    /// Whether the account is a bot ("APP" in Discord).
    pub bot: bool,
    /// The server tag they display ("RAH"), from `primary_guild`.
    pub tag: Option<String>,
    /// The game or music activity, for the "Activity" cards.
    pub card: Option<ActivityCard>,
    /// Online only on their phone: Discord draws the status as a phone.
    pub mobile: bool,
    /// Boosting the server (the pink gem after the name).
    pub booster: bool,
    /// The parts of the line under their name.
    pub line: PresenceLine,
}

/// A custom status's emoji: unicode (`id` none) or a custom one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatusEmoji {
    pub name: String,
    pub id: Option<Id>,
    pub animated: bool,
}

/// What someone is up to, split the way Discord draws it: game icon (and
/// how many more), then the custom status's emoji and text.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PresenceLine {
    pub game: Option<ActivityKind>,
    pub game_text: Option<String>,
    pub games: u32,
    pub emoji: Option<StatusEmoji>,
    pub custom: Option<String>,
}

impl PresenceLine {
    pub fn is_empty(&self) -> bool {
        self.game.is_none() && self.emoji.is_none() && self.custom.is_none()
    }
}

pub fn presence_line(activities: &[Value]) -> PresenceLine {
    let mut line = PresenceLine::default();
    let games: Vec<Value> = activities.iter().filter(|a| a["type"].as_u64() != Some(4)).cloned().collect();
    if let Some((kind, text)) = activity_line(&games) {
        line.game = Some(kind);
        line.game_text = Some(text);
        line.games = games.len() as u32;
    }
    if let Some(custom) = activities.iter().find(|a| a["type"].as_u64() == Some(4)) {
        line.custom = custom["state"].as_str().map(one_line).filter(|s| !s.is_empty());
        line.emoji = custom["emoji"]["name"].as_str().map(|name| StatusEmoji {
            name: name.to_string(),
            id: custom["emoji"]["id"].as_str().and_then(|id| id.parse().ok()).map(Id),
            animated: custom["emoji"]["animated"].as_bool().unwrap_or(false),
        });
    }
    line
}

/// Someone's presence, kept for people outside the open member list (friends).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UserPresence {
    pub status: Status,
    pub mobile: bool,
    pub line: PresenceLine,
    /// What they're playing or listening to, for their profile.
    pub activity: Option<ActivityCard>,
}

impl UserPresence {
    pub fn new(status: Status, activities: &[Value], client_status: &Value) -> Self {
        UserPresence { status, mobile: only_on_mobile(client_status), line: presence_line(activities), activity: activity_card(activities) }
    }
}

fn only_on_mobile(client_status: &Value) -> bool {
    client_status.as_object().is_some_and(|a| a.contains_key("mobile") && !a.contains_key("desktop") && !a.contains_key("web"))
}

/// A game or listening activity as the member list's activity cards show it.
#[derive(Clone, Debug, PartialEq)]
pub struct ActivityCard {
    pub kind: ActivityKind,
    /// "Roblox", "Spotify".
    pub name: String,
    /// Detail line when the activity has one (the song, say).
    pub details: Option<String>,
    /// When it began, in ms since the epoch.
    pub start_ms: Option<u64>,
    /// Large art, when it can be turned into a URL.
    pub image: Option<String>,
    /// The game's or app's id on Discord, which knows its icon.
    pub application_id: Option<String>,
}

/// The first game/music activity in `activities` (not custom statuses).
pub fn activity_card(activities: &[Value]) -> Option<ActivityCard> {
    let a = activities.iter().find(|a| matches!(a["type"].as_u64(), Some(0 | 2 | 5) | None) && a["name"].as_str().is_some_and(|n| !n.is_empty()))?;
    let kind = match a["type"].as_u64() {
        Some(2) => ActivityKind::Listening,
        Some(5) => ActivityKind::Competing,
        _ => ActivityKind::Playing,
    };
    let image = a["assets"]["large_image"].as_str().and_then(|id| asset_url(id, a["application_id"].as_str()));
    Some(ActivityCard {
        kind,
        name: one_line(a["name"].as_str().unwrap_or_default()),
        details: a["details"].as_str().filter(|d| !d.is_empty()).map(one_line),
        start_ms: a["timestamps"]["start"].as_u64(),
        image,
        application_id: a["application_id"].as_str().map(str::to_string),
    })
}

/// An activity asset key as an image URL: external `mp:` proxies, Spotify
/// covers, and the application's own uploaded art.
pub fn asset_url(key: &str, application_id: Option<&str>) -> Option<String> {
    if let Some(rest) = key.strip_prefix("mp:") {
        // "external/<hash>/https/host/path" → "https://host/path"
        let tail = rest.split_once("/https/").map(|(_, t)| format!("https://{t}"));
        let http = rest.split_once("/http/").map(|(_, t)| format!("http://{t}"));
        return tail.or(http);
    }
    if let Some(id) = key.strip_prefix("spotify:") {
        return Some(format!("https://i.scdn.co/image/{id}"));
    }
    if key.chars().all(|c| c.is_ascii_digit()) && !key.is_empty() {
        return application_id.map(|app| format!("https://cdn.discordapp.com/app-assets/{app}/{key}.png"));
    }
    None
}

/// The kind of the line under a member's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityKind {
    Custom,
    Playing,
    Streaming,
    Listening,
    Watching,
    Competing,
    Other,
}

#[derive(Deserialize)]
struct RawPresence {
    #[serde(default)]
    status: Status,
    #[serde(default)]
    activities: Vec<Value>,
    /// Which apps they're on: {"desktop": "online", "mobile": "idle", …}.
    #[serde(default)]
    client_status: Value,
}

#[derive(Deserialize)]
struct RawMember {
    user: User,
    #[serde(default)]
    nick: Option<String>,
    #[serde(default)]
    avatar: Option<String>,
    #[serde(default)]
    roles: Vec<Id>,
    #[serde(default)]
    presence: Option<RawPresence>,
    #[serde(default)]
    premium_since: Option<String>,
}

impl From<RawMember> for ListMember {
    fn from(m: RawMember) -> Self {
        let (status, line, card, mobile, parts) = match m.presence {
            Some(p) => (
                p.status,
                activity_line(&p.activities),
                activity_card(&p.activities),
                only_on_mobile(&p.client_status),
                presence_line(&p.activities),
            ),
            None => (Status::Offline, None, None, false, PresenceLine::default()),
        };
        let (activity_kind, activity) = match line {
            Some((kind, text)) => (Some(kind), Some(text)),
            None => (None, None),
        };
        ListMember {
            user: m.user,
            nick: m.nick,
            avatar: m.avatar,
            roles: m.roles,
            status,
            activity,
            activity_kind,
            bot: false,
            tag: None,
            card,
            mobile,
            booster: m.premium_since.is_some(),
            line: parts,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ListItem {
    Group(Group),
    Member(ListMember),
    /// Something this client doesn't know. Kept so indexes stay right.
    Unknown,
}

impl<'de> Deserialize<'de> for ListItem {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        if let Some(group) = v.get("group").and_then(|g| serde_json::from_value::<Group>(g.clone()).ok()) {
            return Ok(ListItem::Group(group));
        }
        if let Some(member) = v.get("member").and_then(|m| serde_json::from_value::<RawMember>(m.clone()).ok()) {
            let mut member: ListMember = member.into();
            let user = &v["member"]["user"];
            member.bot = user["bot"].as_bool().unwrap_or(false);
            member.tag = server_tag(user);
            return Ok(ListItem::Member(member));
        }
        Ok(ListItem::Unknown)
    }
}

/// The server tag in a user's `primary_guild`, when they show it.
pub fn server_tag(user: &Value) -> Option<String> {
    let guild = &user["primary_guild"];
    if guild["identity_enabled"].as_bool() == Some(false) {
        return None;
    }
    guild["tag"].as_str().filter(|t| !t.is_empty()).map(str::to_string)
}

/// "Playing Rust", or the custom status text. Custom status wins, as it does
/// in Discord's own list.
pub fn activity_text(activities: &[Value]) -> Option<String> {
    activity_line(activities).map(|(_, text)| text)
}

/// Discord shows a status on one line: runs of whitespace, line breaks
/// included, become single spaces.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Like `activity_text`, with the kind of line it is.
pub fn activity_line(activities: &[Value]) -> Option<(ActivityKind, String)> {
    use ActivityKind::*;
    let text = |a: &Value| -> Option<(ActivityKind, String)> {
        let name = a["name"].as_str().unwrap_or_default();
        match a["type"].as_u64() {
            Some(4) => {
                // Custom emoji have an id and no text we can draw; unicode ones are their name.
                let emoji = a["emoji"]["name"].as_str().filter(|_| a["emoji"]["id"].is_null());
                let state = a["state"].as_str().filter(|s| !s.is_empty());
                match (emoji, state) {
                    (Some(e), Some(s)) => Some((Custom, format!("{e} {s}"))),
                    (Some(e), None) => Some((Custom, e.to_string())),
                    (None, Some(s)) => Some((Custom, s.to_string())),
                    (None, None) => None,
                }
            }
            _ if name.is_empty() => None,
            Some(0) | None => Some((Playing, format!("Playing {name}"))),
            Some(1) => Some((Streaming, format!("Streaming {name}"))),
            Some(2) => Some((Listening, format!("Listening to {name}"))),
            Some(3) => Some((Watching, format!("Watching {name}"))),
            Some(5) => Some((Competing, format!("Competing in {name}"))),
            Some(_) => Some((Other, name.to_string())),
        }
    };
    activities
        .iter()
        .find(|a| a["type"].as_u64() == Some(4))
        .and_then(text)
        .or_else(|| activities.iter().find_map(text))
        .map(|(kind, line)| (kind, one_line(&line)))
}

/// One change to the list. Indexes count group headers too.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "op")]
pub enum Op {
    /// The items at `range` (inclusive); fewer than the range at the end of the list.
    #[serde(rename = "SYNC")]
    Sync {
        range: [u32; 2],
        #[serde(default, deserialize_with = "lenient_vec")]
        items: Vec<ListItem>,
    },
    #[serde(rename = "INSERT")]
    Insert { index: u32, item: ListItem },
    #[serde(rename = "UPDATE")]
    Update { index: u32, item: ListItem },
    #[serde(rename = "DELETE")]
    Delete { index: u32 },
    /// The server no longer tracks `range`; what we hold there is stale.
    #[serde(rename = "INVALIDATE")]
    Invalidate { range: [u32; 2] },
}

/// GUILD_MEMBER_LIST_UPDATE.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct MemberListUpdate {
    /// Names the list: "everyone", or a hash of the permissions that decide who sees the channel.
    pub id: String,
    pub guild_id: Id,
    #[serde(default)]
    pub member_count: u32,
    #[serde(default)]
    pub online_count: u32,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub groups: Vec<Group>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub ops: Vec<Op>,
}

impl MemberListUpdate {
    /// Every member this update carries, for filling the member cache.
    pub fn members(&self) -> impl Iterator<Item = &ListMember> {
        self.ops.iter().flat_map(|op| {
            let items: &[ListItem] = match op {
                Op::Sync { items, .. } => items,
                Op::Insert { item, .. } | Op::Update { item, .. } => std::slice::from_ref(item),
                Op::Delete { .. } | Op::Invalidate { .. } => &[],
            };
            items.iter().filter_map(|i| match i {
                ListItem::Member(m) => Some(m),
                _ => None,
            })
        })
    }
}

/// The list as far as the client has been told. Rows outside the ranges the
/// server has synced are `None`.
#[derive(Clone, Debug, PartialEq)]
pub struct MemberListModel {
    pub guild_id: Id,
    pub list_id: String,
    pub member_count: u32,
    pub online_count: u32,
    items: Vec<Option<ListItem>>,
    /// Each group's total, from the latest update's `groups`. The group
    /// rows themselves carry only the id.
    groups: Vec<Group>,
    /// Changes whenever the rows do, so views know when to redraw.
    revision: u64,
}

/// The most rows a list may hold. Real lists top out well below this; the cap
/// keeps a hostile or broken index from allocating gigabytes.
pub const MAX_ROWS: usize = 100_000;

fn pad(items: &mut Vec<Option<ListItem>>, len: usize) {
    let len = len.min(MAX_ROWS);
    if items.len() < len {
        items.resize(len, None);
    }
}

impl MemberListModel {
    pub fn new(guild_id: Id, list_id: String) -> Self {
        MemberListModel { guild_id, list_id, member_count: 0, online_count: 0, items: Vec::new(), groups: Vec::new(), revision: 0 }
    }

    /// Whether `update` is for this list.
    pub fn is_for(&self, update: &MemberListUpdate) -> bool {
        self.guild_id == update.guild_id && self.list_id == update.id
    }

    /// How many members `group` ("online", "offline" or a role id) holds.
    pub fn group_count(&self, group: &str) -> Option<u32> {
        self.groups.iter().find(|g| g.id == group).map(|g| g.count)
    }

    pub fn apply(&mut self, update: &MemberListUpdate) {
        if !update.groups.is_empty() {
            self.groups = update.groups.clone();
        }
        for op in &update.ops {
            match op {
                Op::Sync { range: [start, end], items } => {
                    let (start, end) = ((*start as usize).min(MAX_ROWS), (*end as usize).min(MAX_ROWS));
                    pad(&mut self.items, start + items.len());
                    let stop = (end + 1).min(self.items.len());
                    for slot in &mut self.items[start.min(stop)..stop] {
                        *slot = None;
                    }
                    for (slot, item) in self.items[start.min(stop)..].iter_mut().zip(items) {
                        *slot = Some(item.clone());
                    }
                }
                Op::Insert { index, item } => {
                    let index = *index as usize;
                    if index < MAX_ROWS {
                        pad(&mut self.items, index);
                        self.items.insert(index, Some(item.clone()));
                        self.items.truncate(MAX_ROWS);
                    }
                }
                Op::Update { index, item } => {
                    let index = *index as usize;
                    if index < MAX_ROWS {
                        pad(&mut self.items, index + 1);
                        self.items[index] = Some(item.clone());
                    }
                }
                Op::Delete { index } => {
                    if (*index as usize) < self.items.len() {
                        self.items.remove(*index as usize);
                    }
                }
                Op::Invalidate { range: [start, end] } => self.forget(*start, *end),
            }
        }
        self.member_count = update.member_count;
        self.online_count = update.online_count;
        // The groups, when sent, say how long the list now is: each header plus its members.
        if !update.groups.is_empty() {
            let len = update.groups.iter().map(|g| g.count as usize + 1).fold(0usize, usize::saturating_add);
            self.items.resize(len.min(MAX_ROWS), None);
        }
        self.revision += 1;
    }

    /// Drops what's held for the inclusive range, as when we stop subscribing to it.
    pub fn forget(&mut self, start: u32, end: u32) {
        let stop = (end as usize + 1).min(self.items.len());
        for slot in &mut self.items[(start as usize).min(stop)..stop] {
            *slot = None;
        }
        self.revision += 1;
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&ListItem> {
        self.items.get(index)?.as_ref()
    }

    /// A member held anywhere in the list.
    pub fn find(&self, user: Id) -> Option<&ListMember> {
        self.items.iter().flatten().find_map(|i| match i {
            ListItem::Member(m) if m.user.id == user => Some(m),
            _ => None,
        })
    }
}

/// Rows per subscribed range.
pub const CHUNK: u32 = 100;
/// Chunks besides the first the server is asked to keep current.
const EXTRA_CHUNKS: usize = 3;

/// What the panel has asked the server to track: always the first chunk,
/// plus the few most recently scrolled to.
#[derive(Debug, Default)]
pub struct RangeTracker {
    /// Chunk numbers (not 0), the most recently wanted last.
    extra: Vec<u32>,
}

/// Result of [`RangeTracker::want`].
#[derive(Debug, PartialEq, Eq)]
pub enum Want {
    /// Already subscribed.
    Known,
    /// The subscription changed and must be re-sent. `evicted` is the range
    /// that fell out of it, whose rows are now stale.
    Changed { evicted: Option<[u32; 2]> },
}

fn chunk_range(chunk: u32) -> [u32; 2] {
    [chunk * CHUNK, chunk * CHUNK + CHUNK - 1]
}

impl RangeTracker {
    /// Notes that row `index` is on screen and needs to be tracked.
    pub fn want(&mut self, index: usize) -> Want {
        let chunk = index as u32 / CHUNK;
        if chunk == 0 {
            return Want::Known;
        }
        if let Some(at) = self.extra.iter().position(|c| *c == chunk) {
            // Wanted again: now the most recent, so it is the last to be evicted.
            let c = self.extra.remove(at);
            self.extra.push(c);
            return Want::Known;
        }
        self.extra.push(chunk);
        let evicted = (self.extra.len() > EXTRA_CHUNKS).then(|| chunk_range(self.extra.remove(0)));
        Want::Changed { evicted }
    }

    /// The ranges to subscribe to, in order.
    pub fn ranges(&self) -> Vec<[u32; 2]> {
        let mut chunks = self.extra.clone();
        chunks.sort_unstable();
        std::iter::once(0).chain(chunks).map(chunk_range).collect()
    }
}

/// Opcode 14: subscribe to `ranges` of the member list of `channel` in `guild`.
pub fn subscribe_payload(guild: Id, channel: Id, ranges: &[[u32; 2]]) -> Value {
    subscribe_payload_for(guild, [(channel.to_string(), json!(ranges))].into_iter().collect())
}

fn subscribe_payload_for(guild: Id, channels: serde_json::Map<String, Value>) -> Value {
    json!({"op": 14, "d": {
        "guild_id": guild.to_string(),
        "typing": true,
        "activities": true,
        "threads": false,
        "channels": channels,
    }})
}

/// Opcode 14 with no channels: stop receiving the member list of `guild`.
pub fn unsubscribe_payload(guild: Id) -> Value {
    subscribe_payload_for(guild, serde_json::Map::new())
}

/// Whether to take `update` while subscribed to the list of `subscribed`'s
/// guild and holding `held`. Updates for another guild are leftovers. Until
/// the first SYNC after subscribing nothing is held: a leftover from the
/// previous channel's list in the same guild looks just like ours, but only a
/// SYNC starts a list, so incremental ops are ignored until then. Once a list
/// is held, other lists' updates are ignored.
pub fn accepts(subscribed: Option<Id>, held: Option<&MemberListModel>, update: &MemberListUpdate) -> bool {
    if subscribed != Some(update.guild_id) {
        return false;
    }
    match held {
        Some(list) => list.is_for(update),
        None => update.ops.iter().any(|op| matches!(op, Op::Sync { .. })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member_json(id: u64, name: &str, status: &str) -> Value {
        json!({"member": {
            "user": {"id": id.to_string(), "username": name},
            "roles": ["301"], "nick": null, "avatar": null,
            "presence": {"status": status, "activities": []}
        }})
    }

    fn group_json(id: &str, count: u32) -> Value {
        json!({"group": {"id": id, "count": count}})
    }

    fn update(ops: Value, groups: Value) -> MemberListUpdate {
        serde_json::from_value(json!({
            "id": "everyone", "guild_id": "300", "member_count": 9, "online_count": 4,
            "groups": groups, "ops": ops
        }))
        .unwrap()
    }

    fn model() -> MemberListModel {
        MemberListModel::new(Id(300), "everyone".into())
    }

    #[test]
    fn group_totals_come_from_the_update_not_the_rows() {
        // Row items carry the group id; the counts arrive in `groups`.
        let mut m = model();
        m.apply(&update(
            json!([{"op": "SYNC", "range": [0, 0], "items": [{"group": {"id": "online"}}]}]),
            json!([{"id": "online", "count": 4}, {"id": "offline", "count": 5}]),
        ));
        assert_eq!(m.group_count("online"), Some(4));
        assert_eq!(m.group_count("offline"), Some(5));
        assert_eq!(m.group_count("301"), None);
        // An update without groups keeps the last totals.
        m.apply(&update(json!([]), json!([])));
        assert_eq!(m.group_count("online"), Some(4));
    }

    /// The list as text: group ids, user names, or "." for rows not held.
    fn shape(m: &MemberListModel) -> Vec<String> {
        (0..m.len())
            .map(|i| match m.get(i) {
                Some(ListItem::Group(g)) => format!("#{}", g.id),
                Some(ListItem::Member(u)) => u.user.username.clone(),
                Some(ListItem::Unknown) => "?".into(),
                None => ".".into(),
            })
            .collect()
    }

    fn synced() -> MemberListModel {
        let mut m = model();
        m.apply(&update(
            json!([{"op": "SYNC", "range": [0, 99], "items": [
                group_json("online", 2), member_json(1, "ann", "online"), member_json(2, "bob", "idle"),
                group_json("offline", 1), member_json(3, "cy", "offline"),
            ]}]),
            json!([{"id": "online", "count": 2}, {"id": "offline", "count": 1}]),
        ));
        m
    }

    #[test]
    fn decodes_a_full_update() {
        let u = update(
            json!([
                {"op": "SYNC", "range": [0, 99], "items": [group_json("online", 1), member_json(1, "ann", "dnd")]},
                {"op": "INSERT", "index": 1, "item": member_json(2, "bob", "online")},
                {"op": "UPDATE", "index": 2, "item": member_json(1, "ann", "idle")},
                {"op": "DELETE", "index": 2},
                {"op": "INVALIDATE", "range": [100, 199]},
                {"op": "FUTURE_OP", "whatever": 1},
            ]),
            json!([{"id": "online", "count": 1}]),
        );
        assert_eq!((u.member_count, u.online_count), (9, 4));
        assert_eq!(u.groups, [Group { id: "online".into(), count: 1 }]);
        assert_eq!(u.ops.len(), 5, "an unknown op is skipped");
        let Op::Sync { range, items } = &u.ops[0] else { panic!() };
        assert_eq!(*range, [0, 99]);
        let ListItem::Member(ann) = &items[1] else { panic!() };
        assert_eq!((ann.user.username.as_str(), ann.status, ann.roles.clone()), ("ann", Status::Dnd, vec![Id(301)]));
        assert_eq!(u.members().count(), 3, "SYNC's one, INSERT's and UPDATE's");
    }

    #[test]
    fn member_details_and_unknown_items() {
        let u = update(
            json!([{"op": "SYNC", "range": [0, 2], "items": [
                {"member": {"user": {"id": "1", "username": "ann"}, "nick": "Annie", "avatar": "av",
                    "presence": {"status": "online", "activities": [{"type": 4, "state": "busy", "name": "Custom Status"}]}}},
                {"member": {"user": {"id": "2", "username": "bob"}}},
                {"something": "else"},
            ]}]),
            json!([]),
        );
        let Op::Sync { items, .. } = &u.ops[0] else { panic!() };
        let ListItem::Member(ann) = &items[0] else { panic!() };
        assert_eq!((ann.nick.as_deref(), ann.avatar.as_deref()), (Some("Annie"), Some("av")));
        assert_eq!(ann.activity.as_deref(), Some("busy"));
        let ListItem::Member(bob) = &items[1] else { panic!() };
        assert_eq!((bob.status, bob.activity.clone()), (Status::Offline, None), "no presence means offline");
        assert_eq!(items[2], ListItem::Unknown);
    }

    #[test]
    fn activity_cards() {
        let acts = [
            json!({"type": 4, "state": "hi"}),
            json!({"type": 0, "name": "Roblox", "application_id": "123", "assets": {"large_image": "456"}, "timestamps": {"start": 1700000000000u64}}),
        ];
        let c = activity_card(&acts).unwrap();
        assert_eq!((c.kind, c.name.as_str(), c.start_ms), (ActivityKind::Playing, "Roblox", Some(1700000000000)));
        assert_eq!(c.image.as_deref(), Some("https://cdn.discordapp.com/app-assets/123/456.png"));
        assert!(activity_card(&[json!({"type": 4, "state": "hi"})]).is_none());
        let s = activity_card(&[json!({"type": 2, "name": "Spotify", "assets": {"large_image": "spotify:abc"}})]).unwrap();
        assert_eq!(s.kind, ActivityKind::Listening);
        assert_eq!(s.image.as_deref(), Some("https://i.scdn.co/image/abc"));
    }

    #[test]
    fn asset_urls() {
        assert_eq!(asset_url("mp:external/xx/https/media.x.com/a.png", None).as_deref(), Some("https://media.x.com/a.png"));
        assert_eq!(asset_url("abc", Some("1")), None);
        assert_eq!(asset_url("5", None), None);
    }

    #[test]
    fn activity_lines() {
        let a = |v: Value| activity_text(&[v]);
        assert_eq!(a(json!({"type": 0, "name": "Rust"})).as_deref(), Some("Playing Rust"));
        assert_eq!(a(json!({"type": 1, "name": "Chess"})).as_deref(), Some("Streaming Chess"));
        assert_eq!(a(json!({"type": 2, "name": "Spotify"})).as_deref(), Some("Listening to Spotify"));
        assert_eq!(a(json!({"type": 3, "name": "Anime"})).as_deref(), Some("Watching Anime"));
        assert_eq!(a(json!({"type": 5, "name": "Cup"})).as_deref(), Some("Competing in Cup"));
        assert_eq!(a(json!({"type": 4, "state": "hi", "emoji": {"name": "👋"}})).as_deref(), Some("👋 hi"));
        assert_eq!(a(json!({"type": 4, "emoji": {"name": "👋"}})).as_deref(), Some("👋"));
        assert_eq!(a(json!({"type": 4, "state": "hi", "emoji": {"name": "x", "id": "5"}})).as_deref(), Some("hi"));
        assert_eq!(a(json!({"type": 4})), None);
        // Discord shows statuses on one line: line breaks become spaces.
        assert_eq!(a(json!({"type": 4, "state": "#somestatus\nat /testguild"})).as_deref(), Some("#somestatus at /testguild"));
        assert_eq!(a(json!({"type": 0, "name": "Two\r\n  Lines"})).as_deref(), Some("Playing Two Lines"));
        assert_eq!(activity_text(&[]), None);
        // The custom status wins over an earlier activity.
        let both = [json!({"type": 0, "name": "Rust"}), json!({"type": 4, "state": "afk"})];
        assert_eq!(activity_text(&both).as_deref(), Some("afk"));
    }

    #[test]
    fn sync_fills_the_list() {
        let m = synced();
        assert_eq!(shape(&m), ["#online", "ann", "bob", "#offline", "cy"]);
        assert_eq!((m.member_count, m.online_count), (9, 4));
        assert_eq!(m.find(Id(2)).unwrap().status, Status::Idle);
        assert!(m.find(Id(99)).is_none());
    }

    #[test]
    fn sync_of_a_later_range_leaves_a_gap() {
        let mut m = model();
        m.apply(&update(
            json!([{"op": "SYNC", "range": [3, 4], "items": [member_json(3, "cy", "online"), member_json(4, "di", "online")]}]),
            json!([{"id": "online", "count": 5}]),
        ));
        assert_eq!(shape(&m), [".", ".", ".", "cy", "di", "."]);
    }

    #[test]
    fn sync_replaces_what_was_in_its_range() {
        let mut m = synced();
        m.apply(&update(
            json!([{"op": "SYNC", "range": [0, 4], "items": [group_json("online", 1), member_json(7, "gus", "online")]}]),
            json!([{"id": "online", "count": 1}, {"id": "offline", "count": 1}]),
        ));
        // The rows of the range the SYNC didn't fill are cleared, not left stale.
        assert_eq!(shape(&m), ["#online", "gus", ".", "."]);
    }

    #[test]
    fn insert_shifts_the_rows_below() {
        let mut m = synced();
        m.apply(&update(
            json!([{"op": "INSERT", "index": 2, "item": member_json(4, "di", "online")}]),
            json!([{"id": "online", "count": 3}, {"id": "offline", "count": 1}]),
        ));
        assert_eq!(shape(&m), ["#online", "ann", "di", "bob", "#offline", "cy"]);
    }

    #[test]
    fn delete_closes_the_gap() {
        let mut m = synced();
        m.apply(&update(
            json!([{"op": "DELETE", "index": 1}]),
            json!([{"id": "online", "count": 1}, {"id": "offline", "count": 1}]),
        ));
        assert_eq!(shape(&m), ["#online", "bob", "#offline", "cy"]);
    }

    #[test]
    fn update_replaces_in_place() {
        let mut m = synced();
        m.apply(&update(
            json!([{"op": "UPDATE", "index": 2, "item": member_json(2, "bobby", "dnd")}]),
            json!([{"id": "online", "count": 2}, {"id": "offline", "count": 1}]),
        ));
        assert_eq!(shape(&m), ["#online", "ann", "bobby", "#offline", "cy"]);
        assert_eq!(m.find(Id(2)).unwrap().status, Status::Dnd);
    }

    #[test]
    fn invalidate_forgets_a_range() {
        let mut m = synced();
        m.apply(&update(
            json!([{"op": "INVALIDATE", "range": [1, 3]}]),
            json!([{"id": "online", "count": 2}, {"id": "offline", "count": 1}]),
        ));
        assert_eq!(shape(&m), ["#online", ".", ".", ".", "cy"]);
        // A range past the end is harmless.
        m.forget(50, 99);
        assert_eq!(m.len(), 5);
    }

    #[test]
    fn move_is_a_delete_then_an_insert() {
        // bob goes offline: out of "online", into "offline".
        let mut m = synced();
        m.apply(&update(
            json!([
                {"op": "DELETE", "index": 2},
                {"op": "INSERT", "index": 3, "item": member_json(2, "bob", "offline")},
            ]),
            json!([{"id": "online", "count": 1}, {"id": "offline", "count": 2}]),
        ));
        assert_eq!(shape(&m), ["#online", "ann", "#offline", "bob", "cy"]);
    }

    #[test]
    fn groups_decide_the_length() {
        let mut m = synced();
        // Members left: the list shrinks to what the groups add up to.
        m.apply(&update(json!([]), json!([{"id": "online", "count": 1}])));
        assert_eq!(shape(&m), ["#online", "ann"]);
        // And grows with unknown rows when more exist than we hold.
        m.apply(&update(json!([]), json!([{"id": "online", "count": 3}])));
        assert_eq!(shape(&m), ["#online", "ann", ".", "."]);
    }

    #[test]
    fn revision_changes_with_every_change() {
        let mut m = synced();
        let before = m.revision();
        m.forget(1, 1);
        assert!(m.revision() > before);
        let before = m.revision();
        m.apply(&update(json!([]), json!([{"id": "online", "count": 2}, {"id": "offline", "count": 1}])));
        assert!(m.revision() > before);
    }

    #[test]
    fn group_ids_name_roles() {
        assert_eq!(Group { id: "301".into(), count: 1 }.role(), Some(Id(301)));
        assert_eq!(Group { id: "online".into(), count: 1 }.role(), None);
    }

    #[test]
    fn tracker_always_has_the_first_chunk() {
        let t = RangeTracker::default();
        assert_eq!(t.ranges(), [[0, 99]]);
    }

    #[test]
    fn tracker_adds_chunks_and_evicts_the_oldest() {
        let mut t = RangeTracker::default();
        assert_eq!(t.want(5), Want::Known, "the first chunk is always tracked");
        assert_eq!(t.want(150), Want::Changed { evicted: None });
        assert_eq!(t.want(199), Want::Known);
        assert_eq!(t.ranges(), [[0, 99], [100, 199]]);
        assert_eq!(t.want(250), Want::Changed { evicted: None });
        assert_eq!(t.want(350), Want::Changed { evicted: None });
        assert_eq!(t.ranges(), [[0, 99], [100, 199], [200, 299], [300, 399]]);
        // A fifth chunk pushes the oldest out.
        assert_eq!(t.want(1000), Want::Changed { evicted: Some([100, 199]) });
        assert_eq!(t.ranges(), [[0, 99], [200, 299], [300, 399], [1000, 1099]]);
    }

    #[test]
    fn subscribe_payload_shape() {
        assert_eq!(
            subscribe_payload(Id(300), Id(311), &[[0, 99], [100, 199]]),
            json!({"op": 14, "d": {
                "guild_id": "300", "typing": true, "activities": true, "threads": false,
                "channels": {"311": [[0, 99], [100, 199]]}
            }})
        );
    }

    #[test]
    fn huge_indexes_do_not_allocate() {
        let mut m = model();
        m.apply(&update(
            json!([
                {"op": "SYNC", "range": [4_000_000_000u64, 4_000_000_001u64], "items": [member_json(1, "ann", "online")]},
                {"op": "INSERT", "index": 4_000_000_000u64, "item": member_json(2, "bob", "online")},
                {"op": "UPDATE", "index": 4_000_000_000u64, "item": member_json(3, "cy", "online")},
            ]),
            json!([{"id": "online", "count": 4_000_000_000u64}]),
        ));
        assert!(m.len() <= MAX_ROWS);
    }

    #[test]
    fn groups_only_resize_when_present() {
        let mut m = synced();
        m.apply(&update(json!([]), json!([])));
        assert_eq!(m.len(), 5, "an update without groups leaves the length alone");
    }

    #[test]
    fn tracker_refreshes_a_chunk_that_is_wanted_again() {
        let mut t = RangeTracker::default();
        for row in [150, 250, 350] {
            t.want(row);
        }
        // Chunk 1 is touched again, so chunk 2 is now the least recent.
        assert_eq!(t.want(150), Want::Known);
        assert_eq!(t.want(1000), Want::Changed { evicted: Some([200, 299]) });
    }

    #[test]
    fn unsubscribe_payload_shape() {
        assert_eq!(
            unsubscribe_payload(Id(300)),
            json!({"op": 14, "d": {
                "guild_id": "300", "typing": true, "activities": true, "threads": false, "channels": {}
            }})
        );
    }

    #[test]
    fn gate_accepts_only_the_subscribed_guild() {
        let sync = update(json!([{"op": "SYNC", "range": [0, 99], "items": []}]), json!([]));
        assert!(!accepts(None, None, &sync), "nothing subscribed");
        assert!(!accepts(Some(Id(999)), None, &sync), "another guild");
        assert!(accepts(Some(Id(300)), None, &sync));
    }

    #[test]
    fn gate_waits_for_a_sync_before_taking_a_list() {
        let live = update(json!([{"op": "UPDATE", "index": 1, "item": member_json(1, "ann", "idle")}]), json!([]));
        let sync = update(json!([{"op": "SYNC", "range": [0, 99], "items": []}]), json!([]));
        // Right after subscribing, a leftover update of the previous channel's list is no list.
        assert!(!accepts(Some(Id(300)), None, &live));
        assert!(accepts(Some(Id(300)), None, &sync));
        // Once held, only that list's updates count, even a different list's SYNC.
        let held = synced();
        assert!(accepts(Some(Id(300)), Some(&held), &live));
        let mut other = sync.clone();
        other.id = "hash".into();
        assert!(!accepts(Some(Id(300)), Some(&held), &other));
    }

    #[test]
    fn kinds_bots_and_tags() {
        let l = activity_line(&[json!({"type": 2, "name": "Spotify"})]).unwrap();
        assert_eq!(l, (ActivityKind::Listening, "Listening to Spotify".to_string()));
        assert_eq!(activity_line(&[json!({"type": 4, "state": "hi"})]).unwrap().0, ActivityKind::Custom);
        let item: ListItem = serde_json::from_value(json!({"member": {
            "user": {"id": "5", "username": "b", "bot": true, "primary_guild": {"tag": "RAH", "identity_enabled": true}},
            "presence": {"status": "online", "activities": [{"type": 0, "name": "Rust"}]}
        }}))
        .unwrap();
        let ListItem::Member(m) = item else { panic!() };
        assert!(m.bot);
        assert_eq!(m.tag.as_deref(), Some("RAH"));
        assert_eq!(m.activity_kind, Some(ActivityKind::Playing));
        assert_eq!(server_tag(&json!({"primary_guild": {"tag": "X", "identity_enabled": false}})), None);
        assert_eq!(server_tag(&json!({})), None);
    }
}
