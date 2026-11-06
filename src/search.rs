//! Message search: the box, its filters, and the results that take the member
//! list's place. One search lives app-wide so its results stay while jumping
//! between the channels of the server it's for.

use std::sync::Arc;

use chrono::{Local, NaiveDate, TimeZone};
use cord_core::cache::Cache;
use cord_core::search::{self, Answer, Page, Query, Sort};
use cord_core::{Core, Id};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::{App, AppContext as _, Context, Entity, Global, Subscription, Window};

/// A filter Discord's search box understands, typed as `key:value`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    From,
    Mentions,
    Has,
    Before,
    During,
    After,
    In,
    Pinned,
    AuthorType,
}

impl Filter {
    pub const ALL: [Filter; 9] = [Filter::From, Filter::Mentions, Filter::Has, Filter::Before, Filter::During, Filter::After, Filter::In, Filter::Pinned, Filter::AuthorType];

    pub fn key(self) -> &'static str {
        match self {
            Filter::From => "from",
            Filter::Mentions => "mentions",
            Filter::Has => "has",
            Filter::Before => "before",
            Filter::During => "during",
            Filter::After => "after",
            Filter::In => "in",
            Filter::Pinned => "pinned",
            Filter::AuthorType => "authorType",
        }
    }

    /// What the filter menu says about it, as Discord's does.
    pub fn hint(self) -> &'static str {
        match self {
            Filter::From => "user",
            Filter::Mentions => "user",
            Filter::Has => "link, embed or file",
            Filter::Before | Filter::During | Filter::After => "specific date",
            Filter::In => "channel",
            Filter::Pinned => "true or false",
            Filter::AuthorType => "user, bot or webhook",
        }
    }

    /// Whether it makes sense where we are: channels only in a server.
    pub fn applies(self, in_server: bool) -> bool {
        self != Filter::In || in_server
    }

    fn from_key(key: &str) -> Option<Filter> {
        Filter::ALL.into_iter().find(|f| f.key().eq_ignore_ascii_case(key))
    }
}

/// What was typed: the words, and each `key:value` filter in order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Typed {
    pub content: String,
    pub filters: Vec<(Filter, String)>,
}

/// Splits the box's text into words and filters. A value may be quoted to hold spaces.
pub fn parse(text: &str) -> Typed {
    let mut typed = Typed::default();
    let mut words = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..word_end];
        let filter = word.split_once(':').and_then(|(key, _)| Filter::from_key(key).map(|f| (f, key.len())));
        match filter {
            Some((filter, key_len)) => {
                let after = &rest[key_len + 1..];
                let (value, used) = if let Some(quoted) = after.strip_prefix('"') {
                    let end = quoted.find('"').unwrap_or(quoted.len());
                    (&quoted[..end], key_len + 1 + 1 + (end + 1).min(quoted.len()))
                } else {
                    let end = after.find(char::is_whitespace).unwrap_or(after.len());
                    (&after[..end], key_len + 1 + end)
                };
                if !value.is_empty() {
                    typed.filters.push((filter, value.to_string()));
                }
                rest = rest[used.min(rest.len())..].trim_start();
            }
            None => {
                words.push(word);
                rest = rest[word_end..].trim_start();
            }
        }
    }
    typed.content = words.join(" ");
    typed
}

/// Midnight starting `day`, in ms since 1970, local time.
fn day_start(day: NaiveDate) -> Option<u64> {
    let start = Local.from_local_datetime(&day.and_hms_opt(0, 0, 0)?).earliest()?;
    u64::try_from(start.timestamp_millis()).ok()
}

/// `2026-01-31`, `2026/01/31` or `31.01.2026`.
pub fn parse_day(text: &str) -> Option<NaiveDate> {
    ["%Y-%m-%d", "%Y/%m/%d", "%d.%m.%Y"].iter().find_map(|f| NaiveDate::parse_from_str(text.trim(), f).ok())
}

/// The people `name` could mean here: by id, or by any of their names.
fn find_user(cache: &Cache, guild: Option<Id>, channel: Id, name: &str) -> Option<Id> {
    let name = name.trim_start_matches('@');
    if let Ok(id) = name.parse::<u64>() {
        return Some(Id(id));
    }
    let mut pool: Vec<&cord_core::model::User> = cache.mention_candidates(guild, channel, name, 25);
    pool.extend(cache.channel(channel).into_iter().flat_map(|c| c.recipient_ids.iter()).filter_map(|id| cache.user(*id)));
    pool.extend(cache.me());
    let lower = name.to_lowercase();
    let exact = pool.iter().find(|u| {
        [Some(u.username.as_str()), u.global_name.as_deref(), Some(cache.display_name(guild, u))].into_iter().flatten().any(|n| n.to_lowercase() == lower)
    });
    exact.or_else(|| pool.iter().find(|u| u.username.to_lowercase().starts_with(&lower) || cache.display_name(guild, u).to_lowercase().starts_with(&lower))).map(|u| u.id)
}

fn find_channel(cache: &Cache, guild: Id, name: &str) -> Option<Id> {
    let name = name.trim_start_matches('#').to_lowercase();
    if let Ok(id) = name.parse::<u64>() {
        return Some(Id(id));
    }
    cache.guild_channels(guild).into_iter().filter(|c| c.kind.is_text_like() || c.kind == cord_core::model::ChannelKind::Voice).find(|c| c.name.as_deref().is_some_and(|n| n.to_lowercase() == name)).map(|c| c.id)
}

/// Turns what was typed into a query, with the filters that matched nothing listed.
pub fn resolve(typed: &Typed, guild: Option<Id>, channel: Id, cache: &Cache, sort: Sort) -> (Query, Vec<String>) {
    let mut query = Query { content: typed.content.clone(), sort, ..Default::default() };
    let mut unknown = Vec::new();
    let day_ms = 24 * 60 * 60 * 1000;
    for (filter, value) in &typed.filters {
        let found = match filter {
            Filter::From => find_user(cache, guild, channel, value).map(|id| query.authors.push(id)),
            Filter::Mentions => find_user(cache, guild, channel, value).map(|id| query.mentions.push(id)),
            Filter::Has => search::HAS.iter().find(|h| h.eq_ignore_ascii_case(value)).map(|h| query.has.push(h.to_string())),
            Filter::In => guild.and_then(|g| find_channel(cache, g, value)).map(|id| query.channels.push(id)),
            Filter::Pinned => match value.to_lowercase().as_str() {
                "true" | "yes" => {
                    query.pinned = true;
                    Some(())
                }
                "false" | "no" => Some(()),
                _ => None,
            },
            Filter::AuthorType => ["user", "bot", "webhook"].iter().find(|t| t.eq_ignore_ascii_case(value)).map(|t| query.author_type.push(t.to_string())),
            Filter::Before | Filter::After | Filter::During => parse_day(value).and_then(day_start).map(|start| match filter {
                Filter::Before => query.before = Some(Id::from_timestamp_ms(start)),
                Filter::After => query.after = Some(Id::from_timestamp_ms(start + day_ms)),
                _ => {
                    query.after = Some(Id::from_timestamp_ms(start));
                    query.before = Some(Id::from_timestamp_ms(start + day_ms));
                }
            }),
        };
        if found.is_none() {
            unknown.push(format!("{}:{value}", filter.key()));
        }
    }
    (query, unknown)
}

/// The filter being typed at the end of the box, and what's typed of its value so far.
pub fn typing_filter(text: &str) -> Option<(Filter, &str)> {
    let last = text.rsplit(char::is_whitespace).next()?;
    let (key, value) = last.split_once(':')?;
    Some((Filter::from_key(key)?, value))
}

/// Where a search looks: a whole server, or one DM or group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scope {
    pub guild: Option<Id>,
    /// The channel it was started from: the one searched outside a server.
    pub channel: Id,
}

impl Scope {
    /// Whether a page of `guild`/`channel` shows this search's results.
    pub fn covers(&self, guild: Option<Id>, channel: Id) -> bool {
        match self.guild {
            Some(g) => guild == Some(g),
            None => guild.is_none() && channel == self.channel,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Results {
    Loading,
    Done(Page),
    Failed(String),
}

pub struct Search {
    pub input: Entity<InputState>,
    core: Arc<Core>,
    /// The search shown, while there is one.
    pub scope: Option<Scope>,
    pub query: Query,
    pub unknown: Vec<String>,
    pub results: Results,
    pub sort: Sort,
    /// The box has focus: the filter menu shows under it.
    pub focused: bool,
    /// Counts searches, so a late answer to an old one is dropped.
    asked: u64,
    pub scroll: gpui_kit::ScrollHandle,
    /// What the box's placeholder says now.
    pub placeholder: String,
    _watching: Vec<Subscription>,
}

/// Something in the search changed for the page to act on.
pub enum SearchEvent {
    /// Enter was pressed: search for what's typed.
    Submit,
}

impl gpui_kit::EventEmitter<SearchEvent> for Search {}

struct Shared(Entity<Search>);
impl Global for Shared {}

/// The app's one search, made the first time a page asks for it.
pub fn get(core: &Arc<Core>, window: &mut Window, cx: &mut App) -> Entity<Search> {
    if let Some(shared) = cx.try_global::<Shared>() {
        return shared.0.clone();
    }
    let core = core.clone();
    let search = cx.new(|cx| {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let watching = vec![cx.subscribe(&input, |search: &mut Search, _, event: &InputEvent, cx| match event {
            // The menu goes once searched; typing again brings it back.
            InputEvent::PressEnter { .. } => {
                search.focused = false;
                cx.emit(SearchEvent::Submit);
                cx.notify();
            }
            InputEvent::Focus => {
                search.focused = true;
                cx.notify();
            }
            InputEvent::Blur => {
                search.focused = false;
                cx.notify();
            }
            InputEvent::Change => {
                search.focused = true;
                cx.notify();
            }
        })];
        Search { input, core, scope: None, query: Query::default(), unknown: Vec::new(), results: Results::Loading, sort: Sort::Newest, focused: false, asked: 0, scroll: gpui_kit::ScrollHandle::new(), placeholder: "Search".into(), _watching: watching }
    });
    cx.set_global(Shared(search.clone()));
    search
}

impl Search {
    /// Searches `scope` for what's typed in the box.
    pub fn run(&mut self, scope: Scope, store: &Entity<crate::store::Store>, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        let (query, unknown) = resolve(&parse(&text), scope.guild, scope.channel, store.read(cx).cache(), self.sort);
        if query.is_empty() {
            return;
        }
        self.scope = Some(scope);
        self.query = query;
        self.unknown = unknown;
        self.fetch(cx);
    }

    /// The page at `offset` of the same search.
    pub fn go_to(&mut self, offset: u32, cx: &mut Context<Self>) {
        self.query.offset = offset;
        self.fetch(cx);
    }

    pub fn set_sort(&mut self, sort: Sort, cx: &mut Context<Self>) {
        self.sort = sort;
        if self.scope.is_some() {
            self.query.sort = sort;
            self.query.offset = 0;
            self.fetch(cx);
        }
    }

    /// Closes the results, leaving the box empty.
    pub fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.scope = None;
        self.asked += 1;
        self.input.update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }

    fn fetch(&mut self, cx: &mut Context<Self>) {
        let Some(scope) = self.scope else { return };
        self.asked += 1;
        let (asked, core, query) = (self.asked, self.core.clone(), self.query.clone());
        self.results = Results::Loading;
        self.scroll.set_offset(gpui_kit::point(gpui_kit::px(0.), gpui_kit::px(0.)));
        cx.notify();
        cx.spawn(async move |this, cx| {
            // A place not indexed yet answers "try again": a few times, as asked.
            for _ in 0..4 {
                let answer = core.search(scope.guild, scope.channel, &query).await;
                let wait = match answer {
                    Ok(Answer::Indexing(wait)) => wait,
                    other => {
                        let _ = this.update(cx, |search, cx| {
                            if search.asked == asked {
                                search.results = match other {
                                    Ok(Answer::Page(page)) => Results::Done(page),
                                    Err(error) => Results::Failed(error.to_string()),
                                    Ok(Answer::Indexing(_)) => unreachable!(),
                                };
                                cx.notify();
                            }
                        });
                        return;
                    }
                };
                cx.background_executor().timer(std::time::Duration::from_secs_f32(wait)).await;
                if this.read_with(cx, |search, _| search.asked != asked).unwrap_or(true) {
                    return;
                }
            }
            let _ = this.update(cx, |search, cx| {
                if search.asked == asked {
                    search.results = Results::Failed("Discord is still indexing this place. Try again in a moment.".into());
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

/// "Page 2 of 7" in numbers: the page shown and how many there are.
pub fn pages(offset: u32, total: u32) -> (u32, u32) {
    (offset / search::PAGE + 1, total.div_ceil(search::PAGE).max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_words_from_filters() {
        let typed = parse("cats from:kit  has:image are \"great\" in:#general");
        assert_eq!(typed.content, "cats are \"great\"");
        assert_eq!(typed.filters, vec![(Filter::From, "kit".into()), (Filter::Has, "image".into()), (Filter::In, "#general".into())]);
        let quoted = parse("from:\"Kit Cat\" hello");
        assert_eq!(quoted.filters, vec![(Filter::From, "Kit Cat".into())]);
        assert_eq!(quoted.content, "hello");
        assert_eq!(parse("time: 5pm").content, "time: 5pm", "not a filter");
        assert_eq!(parse("from:").filters, vec![], "nothing chosen yet");
        assert_eq!(parse("FROM:kit").filters, vec![(Filter::From, "kit".into())]);
    }

    #[test]
    fn notices_a_filter_being_typed() {
        assert_eq!(typing_filter("cats from:ki"), Some((Filter::From, "ki")));
        assert_eq!(typing_filter("has:"), Some((Filter::Has, "")));
        assert_eq!(typing_filter("cats"), None);
        assert_eq!(typing_filter("from:kit "), None, "finished");
    }

    #[test]
    fn reads_dates() {
        assert_eq!(parse_day("2026-01-31"), NaiveDate::from_ymd_opt(2026, 1, 31));
        assert_eq!(parse_day("31.01.2026"), NaiveDate::from_ymd_opt(2026, 1, 31));
        assert_eq!(parse_day("yesterday"), None);
    }

    #[test]
    fn resolves_what_it_can_and_lists_the_rest() {
        let cache = Cache::default();
        let typed = parse("hi has:IMAGE has:hats pinned:true during:2026-01-31 in:general from:123");
        let (query, unknown) = resolve(&typed, None, Id(5), &cache, Sort::Oldest);
        assert_eq!(query.has, vec!["image".to_string()]);
        assert!(query.pinned);
        assert_eq!(query.authors, vec![Id(123)], "an id needs no lookup");
        let (after, before) = (query.after.unwrap().timestamp_ms(), query.before.unwrap().timestamp_ms());
        assert_eq!(before - after, 24 * 60 * 60 * 1000, "a whole day");
        assert_eq!(query.sort, Sort::Oldest);
        assert_eq!(unknown, vec!["has:hats".to_string(), "in:general".to_string()], "no channels outside a server");
    }

    #[test]
    fn scopes_and_pages() {
        let server = Scope { guild: Some(Id(1)), channel: Id(5) };
        assert!(server.covers(Some(Id(1)), Id(6)), "any channel of the server");
        assert!(!server.covers(Some(Id(2)), Id(5)));
        let dm = Scope { guild: None, channel: Id(5) };
        assert!(dm.covers(None, Id(5)) && !dm.covers(None, Id(6)));
        assert_eq!(pages(0, 0), (1, 1));
        assert_eq!(pages(25, 51), (2, 3));
    }
}
