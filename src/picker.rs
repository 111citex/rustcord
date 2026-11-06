//! Discord's expression picker: the GIF and emoji tabs, from the message box or a
//! message's Add Reaction. Emoji come in categories with a search and the servers'
//! own; those Discord would only let Nitro use are shown locked, never usable here.
//! GIFs come from Discord's GIF search (Klipy behind it), with stars kept in the
//! account's settings so they're the same as in Discord's own client. Alt+click
//! makes an emoji a favourite, kept on this computer like Frequently Used.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use cord_core::Id;
use cord_core::cache::Cache;
use cord_core::gifs::{Gif, Kind, Trending, absolute, kind, unproxied};
use cord_core::settings_proto::FavoriteGif;
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, Global, InteractiveElement as _, IntoElement, ObjectFit, ParentElement as _, Pixels, Point,
    Render, ScrollStrategy, SharedString, StatefulInteractiveElement as _, Styled as _, StyledImage as _, Subscription, UniformListScrollHandle, Window,
    anchored, deferred, div, img, point, px, uniform_list,
};

use crate::smooth_scroll::{ScrollTarget, SmoothScroller};
use crate::store::Store;
use crate::transition::Showing;
use crate::ui::u;

/// Emoji to a row, and a cell's size, as in Discord's picker.
const PER_ROW: usize = 9;
const CELL: f32 = 40.;
const WIDTH: f32 = 498.;
const HEIGHT: f32 = 440.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Gifs,
    Emoji,
}

/// What was picked.
#[derive(Clone, Debug, PartialEq)]
pub enum Picked {
    Unicode(&'static str),
    Custom { name: String, id: Id, animated: bool },
    /// A GIF's page address, to send.
    Gif(String),
}

pub type OnPick = Rc<dyn Fn(Picked, &mut Window, &mut App)>;

#[derive(Clone, Debug, PartialEq)]
enum Cell {
    Unicode(&'static emojis::Emoji),
    Custom { id: Id, name: String, animated: bool, locked: bool },
}

impl Cell {
    fn name(&self) -> String {
        match self {
            Cell::Unicode(e) => e.shortcode().unwrap_or(e.name()).to_string(),
            Cell::Custom { name, .. } => name.clone(),
        }
    }

    /// The key it's counted under for Frequently Used.
    fn key(&self) -> String {
        match self {
            Cell::Unicode(e) => e.as_str().to_string(),
            Cell::Custom { id, name, animated, .. } => format!("custom:{id}:{name}:{animated}"),
        }
    }
}

/// Where a section's rail icon comes from.
#[derive(Clone, Debug, PartialEq)]
enum Mark {
    Icon(IconName),
    Picture(String),
}

#[derive(Clone, Debug, PartialEq)]
struct Section {
    title: String,
    mark: Mark,
    cells: Vec<Cell>,
}

#[derive(Clone, Debug, PartialEq)]
enum Row {
    Header(String),
    Cells(Vec<Cell>),
}

/// Sections as rows: a heading, then its emoji `PER_ROW` at a time. Also where each
/// section's heading is, for the rail to jump to.
fn rows_of(sections: &[Section]) -> (Vec<Row>, Vec<usize>) {
    let (mut rows, mut starts) = (Vec::new(), Vec::new());
    for section in sections.iter().filter(|s| !s.cells.is_empty()) {
        starts.push(rows.len());
        rows.push(Row::Header(section.title.clone()));
        rows.extend(section.cells.chunks(PER_ROW).map(|chunk| Row::Cells(chunk.to_vec())));
    }
    (rows, starts)
}

fn group_mark(group: emojis::Group) -> (&'static str, IconName) {
    use emojis::Group as G;
    match group {
        G::SmileysAndEmotion => ("Smileys & Emotion", IconName::FaceSlightlySmiling),
        G::PeopleAndBody => ("People", IconName::Users),
        G::AnimalsAndNature => ("Nature", IconName::Leaf),
        G::FoodAndDrink => ("Food & Drink", IconName::Coffee),
        G::TravelAndPlaces => ("Travel & Places", IconName::Plane),
        G::Activities => ("Activities", IconName::Gamepad2),
        G::Objects => ("Objects", IconName::Lightbulb),
        G::Symbols => ("Symbols", IconName::Heart),
        G::Flags => ("Flags", IconName::Flag),
    }
}

/// The standard emoji, by Discord's categories, without skin tone variants.
fn unicode_sections() -> Vec<Section> {
    emojis::Group::iter()
        .map(|group| {
            let (title, icon) = group_mark(group);
            Section { title: title.into(), mark: Mark::Icon(icon), cells: group.emojis().filter(|e| e.skin_tone().is_none_or(|t| t == emojis::SkinTone::Default)).map(Cell::Unicode).collect() }
        })
        .collect()
}

/// Every server's emoji, this channel's server first. Those Discord wouldn't let us
/// use here (another server's without Nitro, animated without Nitro) are locked.
fn server_sections(cache: &Cache, channel: Id) -> Vec<Section> {
    let usable: std::collections::HashSet<Id> = cache.usable_emojis(channel).into_iter().map(|e| e.id).collect();
    let here = cache.channel(channel).and_then(|c| c.guild_id);
    let mut guilds: Vec<&cord_core::model::Guild> = cache.guilds().filter(|g| !g.emojis.is_empty()).collect();
    guilds.sort_by_key(|g| Some(g.id) != here);
    guilds
        .into_iter()
        .map(|g| Section {
            title: g.name.clone(),
            mark: match &g.icon {
                Some(hash) => Mark::Picture(format!("{}/icons/{}/{hash}.png?size=64", cord_core::cdn::CDN, g.id)),
                None => Mark::Icon(IconName::Compass),
            },
            cells: server_order(g.emojis.iter().filter(|e| e.available).map(|e| Cell::Custom { id: e.id, name: e.name.clone(), animated: e.animated, locked: !usable.contains(&e.id) }).collect()),
        })
        .collect()
}

/// A server's emoji as Discord lists them: still ones A to Z, then animated ones A to Z.
fn server_order(mut cells: Vec<Cell>) -> Vec<Cell> {
    cells.sort_by_cached_key(|cell| match cell {
        Cell::Custom { name, animated, .. } => (*animated, name.to_lowercase()),
        Cell::Unicode(e) => (false, e.name().to_lowercase()),
    });
    cells
}

/// Cells whose name holds `query`, usable first.
fn search(sections: &[Section], query: &str) -> Vec<Cell> {
    let query = query.trim().to_lowercase().replace(' ', "_");
    let mut found: Vec<(bool, bool, Cell)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for cell in sections.iter().flat_map(|s| &s.cells) {
        let names: Vec<String> = match cell {
            Cell::Unicode(e) => e.shortcodes().map(str::to_string).chain([e.name().replace(' ', "_")]).collect(),
            Cell::Custom { name, .. } => vec![name.to_lowercase()],
        };
        if names.iter().any(|n| n.to_lowercase().contains(&query)) && seen.insert(cell.key()) {
            let starts = names.iter().any(|n| n.to_lowercase().starts_with(&query));
            let locked = matches!(cell, Cell::Custom { locked: true, .. });
            found.push((locked, !starts, cell.clone()));
        }
    }
    found.sort_by_key(|(locked, later, _)| (*locked, *later));
    found.into_iter().map(|(_, _, c)| c).collect()
}

// Frequently Used: counted here, on this computer.
thread_local! {
    static USED: RefCell<Option<HashMap<String, u32>>> = const { RefCell::new(None) };
}

fn used_path() -> std::path::PathBuf {
    cord_core::paths::config_dir().join("emoji_used.json")
}

fn with_used<R>(f: impl FnOnce(&mut HashMap<String, u32>) -> R) -> R {
    USED.with_borrow_mut(|held| f(held.get_or_insert_with(|| std::fs::read(used_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default())))
}

fn note_used(cell: &Cell) {
    let body = with_used(|used| {
        *used.entry(cell.key()).or_default() += 1;
        serde_json::to_vec(used).unwrap_or_default()
    });
    let _ = std::fs::write(used_path(), body);
}

// Favourites: picked with Alt+click, kept here, on this computer, in the order added.
thread_local! {
    static FAVORITES: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

fn favorites_path() -> std::path::PathBuf {
    cord_core::paths::config_dir().join("emoji_favorites.json")
}

fn with_favorites<R>(f: impl FnOnce(&mut Vec<String>) -> R) -> R {
    FAVORITES.with_borrow_mut(|held| f(held.get_or_insert_with(|| std::fs::read(favorites_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default())))
}

/// `keys` with `key` added at the end, or taken out if it was there.
fn toggled_key(mut keys: Vec<String>, key: String) -> Vec<String> {
    match keys.iter().position(|k| *k == key) {
        Some(at) => {
            keys.remove(at);
        }
        None => keys.push(key),
    }
    keys
}

fn toggle_favorite(cell: &Cell) {
    let body = with_favorites(|keys| {
        *keys = toggled_key(std::mem::take(keys), cell.key());
        serde_json::to_vec(keys).unwrap_or_default()
    });
    let _ = std::fs::write(favorites_path(), body);
}

/// A key as written by [`Cell::key`] back as a cell; a server's emoji only where it's usable.
fn cell_of_key(key: &str, usable: &std::collections::HashSet<Id>) -> Option<Cell> {
    match key.strip_prefix("custom:") {
        Some(rest) => {
            let mut parts = rest.splitn(3, ':');
            let id = Id(parts.next()?.parse().ok()?);
            let name = parts.next()?.to_string();
            let animated = parts.next()? == "true";
            usable.contains(&id).then_some(Cell::Custom { id, name, animated, locked: false })
        }
        None => emojis::get(key).map(Cell::Unicode),
    }
}

/// The favourites usable here, in the order they were added.
fn favorites(cache: &Cache, channel: Id) -> Vec<Cell> {
    let usable: std::collections::HashSet<Id> = cache.usable_emojis(channel).into_iter().map(|e| e.id).collect();
    with_favorites(|keys| keys.iter().filter_map(|key| cell_of_key(key, &usable)).collect())
}

fn is_favorite(cell: &Cell) -> bool {
    let key = cell.key();
    with_favorites(|keys| keys.contains(&key))
}

/// The most used, most first, as cells: unicode by itself, server emoji by their key.
fn frequent(cache: &Cache, channel: Id, limit: usize) -> Vec<Cell> {
    let usable: std::collections::HashSet<Id> = cache.usable_emojis(channel).into_iter().map(|e| e.id).collect();
    let mut used: Vec<(String, u32)> = with_used(|u| u.iter().map(|(k, n)| (k.clone(), *n)).collect());
    used.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    // Only where it can be used now.
    used.into_iter().filter_map(|(key, _)| cell_of_key(&key, &usable)).take(limit).collect()
}

/// The starred GIFs, once fetched: shared by every picker opened.
#[derive(Default)]
struct Stars {
    gifs: Option<Vec<FavoriteGif>>,
    asked: bool,
}

impl Global for Stars {}

#[derive(Clone, Debug, PartialEq)]
enum GifView {
    /// The tiles: favourites, trending, and trending searches.
    Home,
    Favorites,
    Trending,
    Search(String),
}

pub struct Picker {
    store: Entity<Store>,
    channel: Id,
    tab: Tab,
    input: Entity<InputState>,
    on_pick: OnPick,
    sections: Vec<Section>,
    rows: Rc<Vec<Row>>,
    starts: Vec<usize>,
    /// The rail's mark for each of `starts`.
    marks: Vec<Mark>,
    built_for: Option<String>,
    scroll: UniformListScrollHandle,
    emoji_scroller: SmoothScroller,
    gif_scroller: SmoothScroller,
    hovered: Option<Cell>,
    trending: Option<Trending>,
    gif_view: GifView,
    gifs: Option<Result<Vec<Gif>, String>>,
    gif_scroll: gpui_kit::ScrollHandle,
    _watching: Vec<Subscription>,
}

struct Open {
    picker: Entity<Picker>,
    at: Point<Pixels>,
    /// From the message box: it opens up from the button.
    above: bool,
    showing: Showing,
}

impl Global for Open {}

/// Open, and not on its way out.
pub fn is_open(cx: &App) -> bool {
    cx.try_global::<Open>().is_some_and(|open| !open.showing.closing)
}

pub fn close(cx: &mut App) {
    crate::transition::leave::<Open>(cx, |open| &mut open.showing);
}

/// Opens the picker on `tab` at `at`. `above`: it hangs above that point rather than below.
pub fn open(store: Entity<Store>, channel: Id, tab: Tab, at: Point<Pixels>, above: bool, on_pick: OnPick, window: &mut Window, cx: &mut App) {
    if cx.try_global::<Open>().is_some_and(|o| !o.showing.closing && o.picker.read(cx).tab == tab && o.picker.read(cx).channel == channel) {
        close(cx);
        return;
    }
    let picker = cx.new(|cx| {
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |input, cx| input.focus(window, cx));
        let typed = cx.subscribe(&input, |this: &mut Picker, _, event: &InputEvent, cx| match event {
            InputEvent::PressEnter { .. } if this.tab == Tab::Gifs => {
                let text = this.input.read(cx).value().trim().to_string();
                if !text.is_empty() {
                    this.show_gifs(GifView::Search(text), cx);
                }
            }
            InputEvent::PressEnter { .. } => {
                // Enter picks the first match.
                if let Some(Row::Cells(cells)) = this.rows.iter().find(|r| matches!(r, Row::Cells(_))).cloned()
                    && let Some(cell) = cells.into_iter().find(|c| !matches!(c, Cell::Custom { locked: true, .. }))
                {
                    this.pick_cell(cell, false, cx);
                }
            }
            _ => cx.notify(),
        });
        let keys = cx.intercept_keystrokes(|event, _, cx| {
            if event.keystroke.key == "escape" && is_open(cx) {
                close(cx);
                cx.stop_propagation();
            }
        });
        let scroll = UniformListScrollHandle::new();
        let emoji_scroller = SmoothScroller::new(ScrollTarget::Handle(scroll.0.borrow().base_handle.clone())).owned_by("picker");
        let gif_scroll = gpui_kit::ScrollHandle::new();
        let gif_scroller = SmoothScroller::new(ScrollTarget::Handle(gif_scroll.clone())).owned_by("picker");
        let mut picker = Picker {
            store,
            channel,
            tab,
            input,
            on_pick,
            sections: Vec::new(),
            rows: Rc::default(),
            starts: Vec::new(),
            marks: Vec::new(),
            built_for: None,
            scroll,
            emoji_scroller,
            gif_scroller,
            hovered: None,
            trending: None,
            gif_view: GifView::Home,
            gifs: None,
            gif_scroll,
            _watching: vec![typed, keys],
        };
        picker.set_tab(tab, window, cx);
        picker
    });
    cx.set_global(Open { picker, at, above, showing: Showing::new() });
    cx.refresh_windows();
}

pub fn overlay(window: &Window, cx: &App) -> Option<AnyElement> {
    let open = cx.try_global::<Open>()?;
    let viewport = window.viewport_size();
    let corner = if open.above { gpui_kit::Anchor::BottomRight } else { gpui_kit::Anchor::TopRight };
    let at = if open.above { point(open.at.x + px(20.), open.at.y - px(20.)) } else { point(open.at.x, open.at.y + px(8.)) };
    // A click outside closes it.
    let outside = div()
        .id("picker-outside")
        .w(viewport.width)
        .h(viewport.height)
        .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| close(cx))
        .child(anchored().anchor(corner).position(at).snap_to_window_with_margin(px(8.)).child(crate::transition::pop(div().relative().child(open.picker.clone()), "picker-card", open.showing)));
    Some(deferred(anchored().position(point(px(0.), px(0.))).child(outside)).with_priority(4).into_any_element())
}

impl Picker {
    fn set_tab(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        self.tab = tab;
        let placeholder = if tab == Tab::Gifs { GIF_SEARCH_PLACEHOLDER } else { "Find the perfect emoji" };
        self.input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.set_placeholder(placeholder, window, cx);
        });
        self.built_for = None;
        if tab == Tab::Gifs {
            self.gif_view = GifView::Home;
            self.load_trending(cx);
            self.load_stars(cx);
        }
        cx.notify();
    }

    fn rebuild_emoji(&mut self, query: &str, cx: &mut Context<Self>) {
        let cache = self.store.read(cx).cache();
        if self.sections.is_empty() {
            let mut sections = server_sections(cache, self.channel);
            sections.extend(unicode_sections());
            self.sections = sections;
        }
        let shown: Vec<Section> = if query.trim().is_empty() {
            let liked = Section { title: "Favorites".into(), mark: Mark::Icon(IconName::StarFill), cells: favorites(cache, self.channel) };
            let recent = Section { title: "Frequently Used".into(), mark: Mark::Icon(IconName::Clock), cells: frequent(cache, self.channel, PER_ROW * 2) };
            [liked, recent].into_iter().chain(self.sections.iter().cloned()).collect()
        } else {
            vec![Section { title: "Search Results".into(), mark: Mark::Icon(IconName::Search), cells: search(&self.sections, query) }]
        };
        let (rows, starts) = rows_of(&shown);
        self.rows = Rc::new(rows);
        self.starts = starts;
        // The same sections `rows_of` kept: those with anything in them.
        self.marks = shown.into_iter().filter(|s| !s.cells.is_empty()).map(|s| s.mark).collect();
    }

    /// Alt+click: in Favorites, or out again. A locked emoji can't be favourited either.
    fn toggle_favorite(&mut self, cell: Cell, cx: &mut Context<Self>) {
        if matches!(cell, Cell::Custom { locked: true, .. }) {
            return;
        }
        toggle_favorite(&cell);
        // Rebuilt where it is: the list doesn't jump back to the top.
        let query = self.input.read(cx).value().to_string();
        self.rebuild_emoji(&query, cx);
        cx.notify();
    }

    fn pick_cell(&mut self, cell: Cell, keep_open: bool, cx: &mut Context<Self>) {
        if matches!(cell, Cell::Custom { locked: true, .. }) {
            return;
        }
        note_used(&cell);
        let picked = match cell {
            Cell::Unicode(e) => Picked::Unicode(e.as_str()),
            Cell::Custom { id, name, animated, .. } => Picked::Custom { name, id, animated },
        };
        let on_pick = self.on_pick.clone();
        if !keep_open {
            close(cx);
        }
        // After this update ends: what's picked into may be the page that holds the picker open.
        cx.defer(move |cx| {
            if let Some(window) = cx.active_window() {
                let _ = window.update(cx, |_, window, cx| on_pick(picked, window, cx));
            }
        });
    }

    fn load_trending(&mut self, cx: &mut Context<Self>) {
        if self.trending.is_some() {
            return;
        }
        let core = self.store.read(cx).core().clone();
        cx.spawn(async move |this, cx| {
            let trending = core.gif_trending().await.unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                this.trending = Some(trending);
                cx.notify();
            });
        })
        .detach();
    }

    /// The account's starred GIFs, once per run.
    fn load_stars(&mut self, cx: &mut Context<Self>) {
        if cx.try_global::<Stars>().is_some_and(|s| s.asked) {
            return;
        }
        cx.set_global(Stars { gifs: None, asked: true });
        let core = self.store.read(cx).core().clone();
        cx.spawn(async move |this, cx| {
            let gifs = core.frecency_settings().await.map(|proto| cord_core::settings_proto::favorite_gifs(&proto));
            let _ = this.update(cx, |_, cx| {
                match gifs {
                    Ok(gifs) => cx.global_mut::<Stars>().gifs = Some(gifs),
                    // Asked again next time it's opened.
                    Err(error) => {
                        tracing::warn!(%error, "couldn't read the starred GIFs");
                        cx.global_mut::<Stars>().asked = false;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn show_gifs(&mut self, view: GifView, cx: &mut Context<Self>) {
        self.gif_view = view.clone();
        self.gifs = None;
        self.gif_scroller.stop();
        self.gif_scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
        let query = match &view {
            GifView::Search(q) => Some(q.clone()),
            GifView::Trending => None,
            GifView::Home | GifView::Favorites => return,
        };
        let core = self.store.read(cx).core().clone();
        cx.spawn(async move |this, cx| {
            let result = core.gif_search(query).await.map_err(|e| e.to_string());
            let _ = this.update(cx, |this, cx| {
                if this.gif_view == view {
                    this.gifs = Some(result);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn pick_gif(&mut self, url: String, cx: &mut Context<Self>) {
        let on_pick = self.on_pick.clone();
        close(cx);
        cx.defer(move |cx| {
            if let Some(window) = cx.active_window() {
                let _ = window.update(cx, |_, window, cx| on_pick(Picked::Gif(url), window, cx));
            }
        });
    }

    /// Stars or unstars a GIF: shown at once, then saved to the account's settings,
    /// read fresh first so nothing else in them is lost.
    fn toggle_star(&mut self, gif: FavoriteGif, cx: &mut Context<Self>) {
        let Some(mut stars) = cx.try_global::<Stars>().and_then(|s| s.gifs.clone()) else { return };
        let starring = !stars.iter().any(|s| s.url == gif.url);
        stars = toggled(stars, gif.clone());
        cx.global_mut::<Stars>().gifs = Some(stars);
        cx.notify();
        let core = self.store.read(cx).core().clone();
        let toasts = self.store.clone();
        cx.spawn(async move |this, cx| {
            let saved = async {
                let proto = core.frecency_settings().await?;
                let current = cord_core::settings_proto::favorite_gifs(&proto);
                let wanted = if starring == current.iter().any(|s| s.url == gif.url) { current } else { toggled(current, gif) };
                let Some(changed) = cord_core::settings_proto::with_favorite_gifs(&proto, &wanted) else {
                    return Err(cord_core::http::HttpError::Decode("unreadable settings".into()));
                };
                core.set_frecency_settings(changed).await?;
                Ok(wanted)
            }
            .await;
            let _ = this.update(cx, |_, cx| {
                match saved {
                    Ok(wanted) => cx.global_mut::<Stars>().gifs = Some(wanted),
                    Err(error) => {
                        toasts.update(cx, |store, cx| store.toast_error("Couldn't save the star", error.to_string(), cx));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

/// Discord's wording for the GIF search box.
const GIF_SEARCH_PLACEHOLDER: &str = "Search KLIPY";

/// What a GIF's tile shows.
#[derive(Clone, Debug, PartialEq)]
enum Tile {
    /// A picture, moving or still: drawn as it is.
    Picture(String),
    /// A video, played muted and looping.
    Video(String),
    Blank,
}

/// A search result's tile: the moving picture Discord sent for it (what's asked for), else
/// Tenor's small GIF of its video, else the GIF itself, else a still, else the video.
fn result_tile(gif: &Gif) -> Tile {
    let (src, gif_src, still) = (absolute(&gif.src), absolute(&gif.gif_src), absolute(&gif.preview));
    if kind(&src) == Kind::Picture {
        return Tile::Picture(src);
    }
    if let Some(tiny) = tenor_tiny(&src) {
        return Tile::Picture(tiny);
    }
    if let Some(picture) = [gif_src, still].into_iter().find(|u| !u.is_empty()) {
        return Tile::Picture(picture);
    }
    if src.is_empty() { Tile::Blank } else { Tile::Video(src) }
}

/// A starred GIF's tile, from what the account keeps of it: Discord keeps pictures now, and
/// kept videos (Tenor's, perhaps behind Discord's media proxy) in older stars.
fn favorite_tile(src: &str, format: u32) -> Tile {
    let src = absolute(src);
    if src.is_empty() {
        return Tile::Blank;
    }
    match kind(&src) {
        Kind::Picture => Tile::Picture(src),
        Kind::Video => tenor_tiny(&unproxied(&src)).map_or(Tile::Video(src), Tile::Picture),
        Kind::Unknown if format == 2 => Tile::Video(src),
        Kind::Unknown => Tile::Picture(src),
    }
}

/// What starring a result keeps, as Discord's client does: the GIF picture rather than
/// the video when there is one, and the format of what's kept.
fn star_for(gif: &Gif) -> FavoriteGif {
    let (src, gif_src) = (absolute(&gif.src), absolute(&gif.gif_src));
    let kept = if kind(&src) == Kind::Video && !gif_src.is_empty() { gif_src } else { src };
    let format = if kind(&kept) == Kind::Picture { 1 } else { 2 };
    FavoriteGif { url: gif.url.clone(), src: kept, format, width: gif.width, height: gif.height, order: 0 }
}

/// How many video tiles play at once: each is a decoder.
const MOST_PLAYING: usize = 4;

/// `https://media.tenor.com/<id>AAAPo/<name>.mp4` → the same `<id>AAAAM/<name>.gif`.
fn tenor_tiny(video: &str) -> Option<String> {
    let rest = video.strip_prefix("https://media.tenor.com/")?;
    let (code, file) = rest.split_once('/')?;
    let id = code.strip_suffix("AAAPo")?;
    let name = file.strip_suffix(".mp4")?;
    Some(format!("https://media.tenor.com/{id}AAAAM/{name}.gif"))
}

/// `stars` with `gif` added on top, or taken away if it was there.
fn toggled(mut stars: Vec<FavoriteGif>, mut gif: FavoriteGif) -> Vec<FavoriteGif> {
    if let Some(at) = stars.iter().position(|s| s.url == gif.url) {
        stars.remove(at);
        return stars;
    }
    gif.order = stars.iter().map(|s| s.order).max().unwrap_or(0) + 1;
    stars.insert(0, gif);
    stars
}

impl Render for Picker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, muted, popover) = (theme.border, theme.muted_foreground, theme.popover);
        let tab_button = |id: &'static str, label: &'static str, tab: Tab, now: Tab, cx: &mut Context<Self>| {
            div()
                .id(id)
                .px(u(8.))
                .h(u(24.))
                .flex()
                .items_center()
                .rounded(u(4.))
                .text_size(u(14.))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .when(tab == now, |d| d.bg(cx.theme().foreground.opacity(0.1)).text_color(cx.theme().foreground))
                .when(tab != now, |d| d.text_color(cx.theme().muted_foreground).hover(|d| d.text_color(cx.theme().foreground)))
                .on_click(cx.listener(move |this, _, window, cx| this.set_tab(tab, window, cx)))
                .child(label)
        };
        let now = self.tab;
        let tabs = h_flex().gap(u(8.)).px(u(16.)).pt(u(16.)).child(tab_button("tab-gifs", "GIFs", Tab::Gifs, now, cx)).child(tab_button("tab-emoji", "Emoji", Tab::Emoji, now, cx));
        let search = div().mx(u(16.)).my(u(12.)).h(u(32.)).px(u(8.)).flex().items_center().rounded(u(4.)).bg(cx.theme().background).child(Input::new(&self.input).appearance(false).small());
        let body = match self.tab {
            Tab::Emoji => self.emoji_body(cx),
            Tab::Gifs => self.gif_body(window, cx),
        };
        v_flex()
            .id("picker")
            .occlude()
            .w(u(WIDTH))
            .h(u(HEIGHT))
            .rounded(u(8.))
            .border_1()
            .border_color(border)
            .bg(popover)
            .shadow_lg()
            .text_color(cx.theme().foreground)
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(tabs)
            .child(search)
            .child(div().flex_1().min_h_0().border_t_1().border_color(border).text_color(muted).child(body))
    }
}

impl Picker {
    fn emoji_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let query = self.input.read(cx).value().to_string();
        if self.built_for.as_deref() != Some(query.as_str()) {
            self.rebuild_emoji(&query, cx);
            self.built_for = Some(query.clone());
            self.emoji_scroller.stop();
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
        let theme = cx.theme();
        let rows = self.rows.clone();
        let picker = cx.entity().downgrade();
        let hover_bg = theme.foreground.opacity(0.1);
        let (muted, foreground) = (theme.muted_foreground, theme.foreground);
        let list = uniform_list("emoji-rows", rows.len(), move |range, _, _| {
            range
                .map(|ix| match &rows[ix] {
                    Row::Header(title) => div()
                        .h(u(CELL))
                        .px(u(8.))
                        .flex()
                        .items_end()
                        .pb(u(6.))
                        .text_size(u(12.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(muted)
                        .child(SharedString::from(title.to_uppercase()))
                        .into_any_element(),
                    Row::Cells(cells) => h_flex()
                        .h(u(CELL))
                        .px(u(4.))
                        .children(cells.iter().enumerate().map(|(i, cell)| {
                            let (picker, hover_picker, chosen, hovered_cell) = (picker.clone(), picker.clone(), cell.clone(), cell.clone());
                            let locked = matches!(cell, Cell::Custom { locked: true, .. });
                            let picture: AnyElement = match cell {
                                Cell::Unicode(e) => crate::badges::emoji_image(e.as_str(), 32.).into_any_element(),
                                Cell::Custom { id, animated, .. } => img(crate::message_text::custom_emoji_url(id, *animated)).size(u(32.)).object_fit(ObjectFit::Contain).into_any_element(),
                            };
                            div()
                                .id(SharedString::from(format!("emoji-{ix}-{i}")))
                                .size(u(CELL))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(u(4.))
                                .when(!locked, |d| d.cursor_pointer().hover(move |d| d.bg(hover_bg)))
                                .when(locked, |d| d.opacity(0.3))
                                .on_hover(move |over, _, cx| {
                                    if *over {
                                        let cell = hovered_cell.clone();
                                        let _ = hover_picker.update(cx, |this, cx| {
                                            if this.hovered.as_ref() != Some(&cell) {
                                                this.hovered = Some(cell);
                                                cx.notify();
                                            }
                                        });
                                    }
                                })
                                .on_click(move |event, _, cx| {
                                    let (keep, favorite) = (event.modifiers().shift, event.modifiers().alt);
                                    let cell = chosen.clone();
                                    let _ = picker.update(cx, |this, cx| if favorite { this.toggle_favorite(cell, cx) } else { this.pick_cell(cell, keep, cx) });
                                })
                                .child(picture)
                        }))
                        .into_any_element(),
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.scroll)
        .flex_1()
        .h_full();
        // The rail: one mark per section, jumping to it.
        let marks: Vec<(usize, Mark)> = if query.trim().is_empty() { self.starts.iter().copied().zip(self.marks.iter().cloned()).collect() } else { Vec::new() };
        let rail = v_flex()
            .id("emoji-rail")
            .w(u(48.))
            .h_full()
            .flex_shrink_0()
            .py(u(4.))
            .gap(u(4.))
            .items_center()
            .overflow_y_scroll()
            .bg(theme.background.opacity(0.5))
            .children(marks.into_iter().enumerate().map(|(i, (start, mark))| {
                let (scroll, scroller) = (self.scroll.clone(), self.emoji_scroller.clone());
                div()
                    .id(("emoji-mark", i))
                    .size(u(32.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(u(4.))
                    .cursor_pointer()
                    .text_color(muted)
                    .hover(move |d| d.bg(hover_bg).text_color(foreground))
                    .on_click(move |_, _, _| {
                        scroller.stop();
                        scroll.scroll_to_item(start, ScrollStrategy::Top);
                    })
                    .child(match mark {
                        Mark::Icon(icon) => Icon::new(icon).size(u(20.)).into_any_element(),
                        Mark::Picture(url) => img(url).size(u(24.)).rounded_full().into_any_element(),
                    })
            }));
        // What's under the pointer, as Discord's bar along the bottom shows it.
        let preview = self.hovered.clone().map(|cell| {
            let locked = matches!(cell, Cell::Custom { locked: true, .. });
            let picture: AnyElement = match &cell {
                Cell::Unicode(e) => crate::badges::emoji_image(e.as_str(), 28.).into_any_element(),
                Cell::Custom { id, animated, .. } => img(crate::message_text::custom_emoji_url(id, *animated)).size(u(28.)).into_any_element(),
            };
            h_flex()
                .h(u(48.))
                .px(u(16.))
                .gap(u(10.))
                .flex_shrink_0()
                .items_center()
                .border_t_1()
                .border_color(theme.border)
                .child(picture)
                .child(div().text_size(u(15.)).font_weight(FontWeight::SEMIBOLD).text_color(foreground).child(format!(":{}:", cell.name())))
                .when(locked, |d| d.child(div().text_size(u(13.)).text_color(muted).child("Not usable here without Nitro")))
                .when(!locked, |d| d.child(div().flex_1()).child(div().text_size(u(12.)).text_color(muted).child(if is_favorite(&cell) { "Alt+click to unfavorite" } else { "Alt+click to favorite" })))
        });
        let empty = self.rows.is_empty();
        h_flex()
            .size_full()
            .child(rail)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(div().relative().flex_1().min_h_0().px(u(4.)).flex().when(empty, |d| d.items_center().justify_center().child("No emoji match that.")).when(!empty, |d| d.child(self.emoji_scroller.layer()).child(list)))
                    .children(preview),
            )
            .into_any_element()
    }

    fn gif_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let stars: Option<Vec<FavoriteGif>> = cx.try_global::<Stars>().and_then(|s| s.gifs.clone());
        let starred: std::collections::HashSet<String> = stars.iter().flatten().map(|s| s.url.clone()).collect();
        let note = |text: &str| div().p(u(24.)).text_center().text_size(u(14.)).child(text.to_string()).into_any_element();
        let back = (self.gif_view != GifView::Home).then(|| {
            let title = match &self.gif_view {
                GifView::Favorites => "Favorites".to_string(),
                GifView::Trending => "Trending GIFs".to_string(),
                GifView::Search(q) => q.clone(),
                GifView::Home => String::new(),
            };
            h_flex()
                .id("gif-back")
                .h(u(36.))
                .px(u(12.))
                .gap(u(8.))
                .items_center()
                .cursor_pointer()
                .text_color(theme.foreground)
                .on_click(cx.listener(|this, _, _, cx| this.show_gifs(GifView::Home, cx)))
                .child(Icon::new(IconName::ArrowLeft).size(u(18.)))
                .child(div().text_size(u(15.)).font_weight(FontWeight::SEMIBOLD).child(title))
        });
        // Tiles: (label, still, what it opens).
        let content: AnyElement = match &self.gif_view {
            GifView::Home => {
                let Some(trending) = &self.trending else { return note("Loading…") };
                let picture = |tile: Tile| match tile {
                    Tile::Picture(url) => Some(url),
                    Tile::Video(_) | Tile::Blank => None,
                };
                let first_star = stars.as_ref().and_then(|s| s.first()).and_then(|s| picture(favorite_tile(&s.src, s.format)));
                let mut tiles: Vec<(String, Option<String>, GifView)> = vec![
                    ("Favorites".into(), first_star, GifView::Favorites),
                    ("Trending GIFs".into(), trending.gifs.first().and_then(|g| picture(result_tile(g))), GifView::Trending),
                ];
                tiles.extend(trending.categories.iter().map(|c| (c.name.clone(), picture(favorite_tile(&c.src, 0)), GifView::Search(c.name.clone()))));
                let rows = tiles.chunks(2).enumerate().map(|(r, pair)| {
                    h_flex().gap(u(8.)).children(pair.iter().enumerate().map(|(i, (label, still, view))| {
                        let view = view.clone();
                        div()
                            .id(("gif-tile", r * 2 + i))
                            .relative()
                            .flex_1()
                            .h(u(110.))
                            .rounded(u(6.))
                            .overflow_hidden()
                            .bg(theme.foreground.opacity(0.08))
                            .cursor_pointer()
                            .hover(|d| d.opacity(0.85))
                            .on_click(cx.listener(move |this, _, _, cx| this.show_gifs(view.clone(), cx)))
                            .when_some(still.clone().filter(|s| !s.is_empty()), |d, url| d.child(img(url).absolute().size_full().object_fit(ObjectFit::Cover)))
                            .child(
                                div()
                                    .absolute()
                                    .size_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(gpui_kit::black().opacity(0.45))
                                    .text_color(gpui_kit::white())
                                    .text_size(u(16.))
                                    .font_weight(FontWeight::BOLD)
                                    .child(SharedString::from(label.clone())),
                            )
                    }))
                });
                v_flex().gap(u(8.)).p(u(12.)).children(rows).into_any_element()
            }
            GifView::Favorites => match &stars {
                None => note("Loading…"),
                Some(list) if list.is_empty() => note("Star a GIF to keep it here."),
                Some(list) => self.gif_grid(list.iter().map(|s| (s.url.clone(), favorite_tile(&s.src, s.format), s.width, s.height, s.clone())).collect(), &starred, window, cx),
            },
            GifView::Trending | GifView::Search(_) => match &self.gifs {
                None => note("Loading…"),
                Some(Err(error)) => note(&format!("Couldn't load GIFs: {error}")),
                Some(Ok(list)) if list.is_empty() => note("No GIFs found."),
                Some(Ok(list)) => {
                    let items = list.iter().map(|g| (g.url.clone(), result_tile(g), g.width, g.height, star_for(g))).collect();
                    self.gif_grid(items, &starred, window, cx)
                }
            },
        };
        v_flex()
            .size_full()
            .children(back)
            .child(div().relative().flex_1().min_h_0().flex().flex_col().child(self.gif_scroller.layer()).child(div().id("gif-scroll").flex_1().min_h_0().overflow_y_scroll().track_scroll(&self.gif_scroll).child(content)))
            .into_any_element()
    }

    /// Two columns, each GIF its own shape, each with its star.
    fn gif_grid(&self, items: Vec<(String, Tile, u32, u32, FavoriteGif)>, starred: &std::collections::HashSet<String>, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        const COLUMN: f32 = (WIDTH - 12. * 2. - 8.) / 2.;
        let tile_bg = cx.theme().foreground.opacity(0.08);
        let mut columns: [Vec<AnyElement>; 2] = [Vec::new(), Vec::new()];
        let mut heights = [0f32; 2];
        let mut playing = 0;
        for (ix, (url, shown, width, height, star)) in items.into_iter().enumerate() {
            // A video plays in a player of its own, for the first few: the rest wait as blanks.
            let player = match &shown {
                Tile::Video(video) if playing < MOST_PLAYING => {
                    playing += 1;
                    crate::gifs::warm_url(video, cx);
                    crate::gifs::player(video, cx)
                }
                _ => None,
            };
            if player.as_ref().is_some_and(|p| !p.read(cx).is_playing()) {
                // Its first frames come in after this: look again then.
                window.request_animation_frame();
            }
            let tall = if width > 0 && height > 0 { COLUMN * height as f32 / width as f32 } else { COLUMN * 0.6 };
            let column = usize::from(heights[1] < heights[0]);
            heights[column] += tall + 8.;
            let is_starred = starred.contains(&url);
            let send_url = url.clone();
            columns[column].push(
                div()
                    .id(("gif", ix))
                    .group(SharedString::from(format!("gif-{ix}")))
                    .relative()
                    .w_full()
                    .h(u(tall))
                    .rounded(u(6.))
                    .overflow_hidden()
                    .bg(tile_bg)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.pick_gif(send_url.clone(), cx)))
                    .map(|d| match (shown, player) {
                        (Tile::Picture(url), _) => d.child(img(url).size_full().object_fit(ObjectFit::Cover)),
                        (Tile::Video(_), Some(player)) => d.child(alhazen_gpui::video_view(player).object_fit(ObjectFit::Cover).size_full()),
                        _ => d,
                    })
                    .child(
                        div()
                            .id(("gif-star", ix))
                            .absolute()
                            .top(u(6.))
                            .right(u(6.))
                            .size(u(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(u(6.))
                            .bg(gpui_kit::black().opacity(0.5))
                            .text_color(if is_starred { gpui_kit::Hsla::from(gpui_kit::rgb(0xf0b232)) } else { gpui_kit::white() })
                            .when(!is_starred, |d| d.opacity(0.).group_hover(SharedString::from(format!("gif-{ix}")), |d| d.opacity(1.)))
                            .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(if is_starred { "Remove from Favorites" } else { "Add to Favorites" }).build(window, cx))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.toggle_star(star.clone(), cx);
                            }))
                            .child(Icon::new(if is_starred { IconName::StarFill } else { IconName::Star }).size(u(16.))),
                    )
                    .into_any_element(),
            );
        }
        let [left, right] = columns;
        h_flex()
            .p(u(12.))
            .gap(u(8.))
            .items_start()
            .child(v_flex().flex_1().gap(u(8.)).children(left))
            .child(v_flex().flex_1().gap(u(8.)).children(right))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unicode(e: &str) -> Cell {
        Cell::Unicode(emojis::get(e).unwrap())
    }

    #[test]
    fn sections_become_rows_with_headings() {
        let sections = vec![
            Section { title: "A".into(), mark: Mark::Icon(IconName::Clock), cells: vec![unicode("😀"); 10] },
            Section { title: "Empty".into(), mark: Mark::Icon(IconName::Clock), cells: vec![] },
            Section { title: "B".into(), mark: Mark::Icon(IconName::Clock), cells: vec![unicode("🐱")] },
        ];
        let (rows, starts) = rows_of(&sections);
        assert_eq!(starts, vec![0, 3], "an empty section has no heading");
        assert_eq!(rows.len(), 5);
        assert!(matches!(&rows[1], Row::Cells(c) if c.len() == PER_ROW));
        assert!(matches!(&rows[2], Row::Cells(c) if c.len() == 1));
    }

    #[test]
    fn the_standard_emoji_have_no_skin_tone_copies() {
        let sections = unicode_sections();
        assert_eq!(sections.len(), 9);
        let thumbs = sections.iter().flat_map(|s| &s.cells).filter(|c| c.name().starts_with("+1") || c.name() == "thumbsup").count();
        assert_eq!(thumbs, 1);
    }

    #[test]
    fn search_finds_by_name_and_puts_locked_last() {
        let sections = vec![Section {
            title: "S".into(),
            mark: Mark::Icon(IconName::Clock),
            cells: vec![
                Cell::Custom { id: Id(1), name: "catjam".into(), animated: true, locked: true },
                Cell::Custom { id: Id(2), name: "happycat".into(), animated: false, locked: false },
                unicode("🐱"),
            ],
        }];
        let found = search(&sections, "cat");
        assert_eq!(found.len(), 3);
        assert_eq!(found.last().map(Cell::name), Some("catjam".to_string()), "locked last");
        assert!(search(&sections, "zzz").is_empty());
        assert_eq!(search(&sections, "cat face").len(), 1, "words match the name with underscores");
    }

    #[test]
    fn tiles_use_tenors_small_gif() {
        assert_eq!(tenor_tiny("https://media.tenor.com/abcAAAPo/cat-dance.mp4").as_deref(), Some("https://media.tenor.com/abcAAAAM/cat-dance.gif"));
        assert_eq!(tenor_tiny("https://example.com/x.mp4"), None);
    }

    fn gif(src: &str, gif_src: &str, preview: &str) -> Gif {
        Gif { url: "https://example.com/view/1".into(), src: src.into(), gif_src: gif_src.into(), preview: preview.into(), width: 200, height: 100, ..Default::default() }
    }

    #[test]
    fn a_result_shows_the_moving_picture_discord_sent() {
        assert_eq!(result_tile(&gif("//static.example.com/a.webp", "", "")), Tile::Picture("https://static.example.com/a.webp".into()));
        assert_eq!(result_tile(&gif("https://example.com/a.webm", "https://example.com/a.gif", "still.png")), Tile::Picture("https://example.com/a.gif".into()));
        assert_eq!(result_tile(&gif("https://example.com/a.mp4", "", "")), Tile::Video("https://example.com/a.mp4".into()));
        assert_eq!(result_tile(&gif("", "", "")), Tile::Blank);
    }

    #[test]
    fn every_kind_of_star_has_a_picture() {
        // What blanked them before: no scheme, or not one of Tenor's videos.
        assert_eq!(favorite_tile("//media.example.com/a.gif?format=webp&animated=true", 1), Tile::Picture("https://media.example.com/a.gif?format=webp&animated=true".into()));
        assert_eq!(
            favorite_tile("https://media.discordapp.net/external/sig/https/media.tenor.com/abcAAAPo/cat.mp4", 2),
            Tile::Picture("https://media.tenor.com/abcAAAAM/cat.gif".into()),
            "Tenor behind the media proxy"
        );
        assert_eq!(favorite_tile("https://static.example.com/b.webm", 2), Tile::Video("https://static.example.com/b.webm".into()));
        assert_eq!(favorite_tile("https://example.com/no-ending", 2), Tile::Video("https://example.com/no-ending".into()));
        assert_eq!(favorite_tile("https://example.com/no-ending", 1), Tile::Picture("https://example.com/no-ending".into()));
        assert_eq!(favorite_tile("", 1), Tile::Blank);
    }

    #[test]
    fn a_star_keeps_a_picture_like_discords() {
        let video = star_for(&gif("https://example.com/a.mp4", "//example.com/a.gif", ""));
        assert_eq!((video.src.as_str(), video.format), ("https://example.com/a.gif", 1));
        let picture = star_for(&gif("https://example.com/a.webp", "https://example.com/a.gif", ""));
        assert_eq!((picture.src.as_str(), picture.format), ("https://example.com/a.webp", 1));
        let only_video = star_for(&gif("https://example.com/a.mp4", "", ""));
        assert_eq!((only_video.src.as_str(), only_video.format), ("https://example.com/a.mp4", 2));
    }

    #[test]
    fn server_emoji_go_still_then_animated_each_a_to_z() {
        let custom = |id, name: &str, animated| Cell::Custom { id: Id(id), name: name.into(), animated, locked: false };
        let ordered = server_order(vec![custom(1, "zed", false), custom(2, "Bop", true), custom(3, "apple", true), custom(4, "Mango", false)]);
        assert_eq!(ordered.iter().map(Cell::name).collect::<Vec<_>>(), ["Mango", "zed", "apple", "Bop"]);
    }

    #[test]
    fn favourites_toggle_and_keep_their_order() {
        let keys = toggled_key(Vec::new(), "a".into());
        let keys = toggled_key(keys, "b".into());
        assert_eq!(keys, ["a", "b"]);
        assert_eq!(toggled_key(keys, "a".into()), ["b"]);
    }

    #[test]
    fn keys_come_back_as_cells_only_where_usable() {
        let usable: std::collections::HashSet<Id> = [Id(7)].into();
        assert_eq!(cell_of_key("custom:7:wave:false", &usable), Some(Cell::Custom { id: Id(7), name: "wave".into(), animated: false, locked: false }));
        assert_eq!(cell_of_key("custom:8:other:true", &usable), None, "another server's, without Nitro");
        assert_eq!(cell_of_key("😀", &usable), Some(unicode("😀")));
    }

    #[test]
    fn starring_twice_unstars_and_new_stars_go_on_top() {
        let gif = |url: &str| FavoriteGif { url: url.into(), ..Default::default() };
        let one = toggled(Vec::new(), gif("a"));
        let two = toggled(one, gif("b"));
        assert_eq!(two.iter().map(|g| (g.url.as_str(), g.order)).collect::<Vec<_>>(), vec![("b", 2), ("a", 1)]);
        assert_eq!(toggled(two, gif("b")).len(), 1);
    }
}
