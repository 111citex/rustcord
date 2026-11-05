//! Draws the output of `markdown`. Text without pictures is one piece of
//! styled text, so it wraps naturally; text with emoji flows word by word in
//! a wrapping row, because pictures can't sit inside a text run.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

use cord_core::Id;
use cord_core::cache::Cache;
use cord_core::model::Message;
use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, App, ElementId, FontStyle, FontWeight, HighlightStyle, Hsla, InteractiveText, InteractiveElement,
    IntoElement, ParentElement, SharedString, StatefulInteractiveElement, StrikethroughStyle, Styled, StyledImage,
    StyledText, UnderlineStyle, div, img, px,
};

use crate::ui::u;
use crate::markdown::{self, Block, Content, ListItem, Run};

/// Parsed markdown by source text. Parsing is the costly part of drawing a
/// message and the text rarely changes, but rows are drawn on every frame.
/// Bounded: when full it starts over, which only costs a re-parse of what is
/// on screen. Dropped with the page, so it goes when its channel does.
#[derive(Default)]
pub struct ParseCache {
    parsed: HashMap<String, Rc<Vec<Block>>>,
}

/// Most distinct texts held. A screenful is a few dozen.
const PARSE_CACHE_CAP: usize = 512;

impl ParseCache {
    pub fn parse(&mut self, text: &str) -> Rc<Vec<Block>> {
        if let Some(blocks) = self.parsed.get(text) {
            return blocks.clone();
        }
        if self.parsed.len() >= PARSE_CACHE_CAP {
            self.parsed.clear();
        }
        let blocks = Rc::new(markdown::parse(text));
        self.parsed.insert(text.to_string(), blocks.clone());
        blocks
    }
}

/// Shared by every row of a channel: what the page remembers and can do.
#[derive(Clone)]
pub struct Shared {
    /// Answers ticked on polls but not voted yet, and polls showing their results early.
    pub poll_ui: Rc<RefCell<PollUi>>,
    /// Voting on a poll of this channel: (message, answers; none takes it back).
    pub vote: Rc<dyn Fn(Id, Vec<u32>, &mut App)>,
    /// Spoilers the viewer has clicked, as (message, group).
    pub revealed: Rc<RefCell<HashSet<(Id, u16)>>>,
    pub reveal: Rc<dyn Fn(Id, u16, &mut App)>,
    /// Scrolls to a message of this channel, if it is loaded.
    pub jump: Rc<dyn Fn(Id, &mut App)>,
    /// Clicking a reaction pill: (message, emoji, whether to add or remove).
    pub react: Rc<dyn Fn(Id, cord_core::model::ReactionEmoji, bool, &mut App)>,
    /// Clicking a user mention: (user, where).
    pub open_user: Rc<dyn Fn(Id, gpui_kit::Point<gpui_kit::Pixels>, &mut gpui_kit::Window, &mut App)>,
    /// The pointer rests on a reaction pill (or left one, with `None`).
    pub who_reacted: Rc<dyn Fn(Option<(Id, cord_core::model::ReactionEmoji, u32)>, &mut App)>,
    /// A message's height changed from outside: it needs measuring again.
    pub remeasure: Rc<dyn Fn(Id, &mut App)>,
    /// A preview, like the line above a reply: nothing in it can be clicked, so
    /// the click goes to the preview itself.
    pub inert: bool,
    /// Inside an embed: a bot's text there, so `@everyone` looks the part without pinging.
    pub in_card: bool,
    /// The chat text size in px; headers and code are sized from it.
    pub text_size: f32,
    pub parsed: Rc<RefCell<ParseCache>>,
}

#[derive(Default)]
pub struct PollUi {
    pub picks: HashMap<Id, Vec<u32>>,
    pub showing: HashSet<Id>,
}

/// Where a name was last painted, so a card can open at its end.
pub type NameSpot = Rc<std::cell::Cell<Option<gpui_kit::Bounds<gpui_kit::Pixels>>>>;

/// `element`, noting where it's painted in `spot`.
pub fn spotted(spot: &NameSpot, element: impl IntoElement) -> gpui_kit::Div {
    let spot = spot.clone();
    div().flex_shrink_0().on_children_prepainted(move |bounds, _, _| spot.set(bounds.first().copied())).child(element)
}

/// Where a card opens for a name at `spot`: just past its end, level with its top.
pub fn card_at(spot: &NameSpot, click: gpui_kit::Point<gpui_kit::Pixels>) -> gpui_kit::Point<gpui_kit::Pixels> {
    spot.get().map_or(click, |b| gpui_kit::point(b.right() + gpui_kit::px(8.), b.top()))
}

/// Everything needed to draw one message's rich content.
pub struct Env<'a> {
    pub shared: &'a Shared,
    pub cache: &'a Cache,
    pub message: &'a Message,
    /// The channel's server. Messages fetched over REST carry no guild_id,
    /// so the page passes it in for nicknames and role colours.
    pub guild: Option<Id>,
}

impl Env<'_> {
    /// `text` as markdown blocks, parsed once while it stays on screen.
    pub fn parse(&self, text: &str) -> Rc<Vec<Block>> {
        if self.shared.in_card {
            return self.shared.parsed.borrow_mut().parse(&crate::message_text::with_shortcodes(text));
        }
        self.shared.parsed.borrow_mut().parse(text)
    }

    fn is_revealed(&self, group: u16) -> bool {
        self.shared.revealed.borrow().contains(&(self.message.id, group))
    }

    fn user_label(&self, id: Id) -> String {
        let guild = self.guild;
        let name = match self.cache.user(id).or_else(|| self.message.mentions.iter().find(|u| u.id == id)) {
            Some(user) => self.cache.display_name(guild, user).to_string(),
            None => "unknown-user".to_string(),
        };
        format!("@{name}")
    }

    fn role(&self, id: Id) -> Option<(&str, u32)> {
        let guild = self.cache.guild(self.guild?)?;
        guild.roles.iter().find(|r| r.id == id).map(|r| (r.name.as_str(), r.color))
    }

    fn channel_label(&self, id: Id) -> String {
        match self.cache.channel(id).and_then(|c| c.name.as_deref()) {
            Some(name) => format!("#{name}"),
            None => "#unknown-channel".into(),
        }
    }
}

/// What clicking part of a text does.
#[derive(Clone)]
enum Click {
    Open(String),
    Reveal(u16),
    /// A user mention: opens their profile card where it was clicked.
    User(Id),
}

/// How one run looks, resolved against the theme and cache.
#[derive(Default)]
struct Look {
    color: Option<Hsla>,
    background: Option<Hsla>,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    mono: bool,
    click: Option<Click>,
}

fn rgb(color: u32) -> Hsla {
    gpui_kit::rgb(color).into()
}

/// The words a text-like run shows, or `None` for emoji (drawn as pictures).
fn label(run: &Run, env: &Env, now: &chrono::DateTime<chrono::Local>) -> Option<String> {
    Some(match &run.content {
        Content::Text(t) => t.clone(),
        Content::Emoji { .. } | Content::CustomEmoji { .. } => return None,
        Content::User(id) => env.user_label(*id),
        Content::Role(id) => match env.role(*id) {
            Some((name, _)) => format!("@{name}"),
            None => "@deleted-role".into(),
        },
        Content::Channel(id) => env.channel_label(*id),
        Content::Command(name) => name.clone(),
        Content::Everyone(word) => word.to_string(),
        Content::Timestamp { unix, style } => markdown::format_timestamp(*unix, *style, now),
    })
}

fn look(run: &Run, env: &Env, cx: &App) -> Look {
    let theme = cx.theme();
    let style = &run.style;
    let mut look = Look {
        bold: style.bold,
        italic: style.italic,
        underline: style.underline,
        strike: style.strike,
        mono: style.code,
        ..Look::default()
    };
    // Inline code: Discord's faint pill, not a black box.
    if style.code {
        look.background = Some(theme.foreground.opacity(0.08));
    }
    if style.muted {
        look.color = Some(theme.muted_foreground);
    }
    match &run.content {
        // User and channel mentions are always Discord's blurple pill
        // (measured: #5865f2 at 24% behind light-blurple text).
        Content::User(_) | Content::Channel(_) | Content::Command(_) => {
            look.color = Some(rgb(MENTION_TEXT));
            look.background = Some(rgb(0x5865f2).opacity(0.24));
            if let Content::User(id) = &run.content {
                look.click = Some(Click::User(*id));
            }
            // In-app, voice channels included: this opens a page, it joins nothing.
            if let Content::Channel(id) = &run.content
                && let Some(channel) = env.cache.channel(*id)
            {
                look.click = Some(Click::Open(format!("https://discord.com{}", crate::nav::channel_path(channel))));
            }
        }
        Content::Everyone(_) if env.message.mention_everyone || env.shared.in_card => {
            look.color = Some(rgb(MENTION_TEXT));
            look.background = Some(rgb(0x5865f2).opacity(0.24));
        }
        Content::Role(id) => {
            let color = env.role(*id).map(|(_, c)| c).filter(|c| *c != 0).map(rgb).unwrap_or(theme.primary);
            look.color = Some(color);
            look.background = Some(color.opacity(0.18));
        }
        Content::Timestamp { .. } => look.background = Some(theme.muted),
        _ => {}
    }
    if let Some(url) = &style.link {
        look.color = Some(theme.link);
        look.click = Some(Click::Open(url.clone()));
    }
    if let Some(group) = style.spoiler {
        if env.is_revealed(group) {
            look.background = Some(theme.muted.opacity(0.7));
        } else {
            // Hidden: the same colour for text and box, until clicked.
            let cover = theme.muted_foreground.opacity(0.5);
            look.color = Some(cover);
            look.background = Some(cover);
            look.click = Some(Click::Reveal(group));
        }
    }
    if env.shared.inert {
        look.click = None;
    }
    look
}

fn run_click(click: &Click, message: Id, shared: &Shared) -> impl Fn(&mut App) + 'static {
    let (click, reveal) = (click.clone(), shared.reveal.clone());
    move |cx| match &click {
        Click::Open(url) => crate::links::open(url, cx),
        Click::Reveal(group) => reveal(message, *group, cx),
        // Handled where the click position is known (`pill_element`).
        Click::User(_) => {}
    }
}

/// A message body or any other run of markdown blocks.
pub fn blocks(env: &Env, parsed: &[Block], key: &str, cx: &App) -> AnyElement {
    // Only a message's own text goes big: not bios, embeds or quotes.
    let jumbo = key == "text" && markdown::is_jumbo(parsed);
    v_flex()
        .w_full()
        .min_w_0()
        .children(parsed.iter().enumerate().map(|(i, block)| self::block(env, block, &format!("{key}.{i}"), jumbo, cx)))
        .into_any_element()
}

fn block(env: &Env, block: &Block, key: &str, jumbo: bool, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let size = env.shared.text_size;
    match block {
        Block::Paragraph(runs) => runs_element(env, key, runs, jumbo, cx),
        Block::Heading { level, runs } => {
            let scale = match level {
                1 => 1.6,
                2 => 1.3,
                _ => 1.1,
            };
            // A card's padding is the room above its first line.
            let first_in_card = env.shared.in_card && key.ends_with(".0");
            div()
                .when(!first_in_card, |d| d.mt(u(4.)))
                .font_weight(FontWeight::BOLD)
                .text_size(u(size * scale))
                .child(runs_element(env, key, runs, false, cx))
                .into_any_element()
        }
        Block::Subtext(runs) => div()
            .text_size(u(size * 0.8))
            .text_color(theme.muted_foreground)
            .child(runs_element(env, key, runs, false, cx))
            .into_any_element(),
        // Discord's: a 4px rounded bar, 12px to the text, no space around.
        Block::Quote(inner) => h_flex()
            .items_stretch()
            .gap(u(12.))
            .pr(u(8.))
            .child(div().w(u(4.)).flex_shrink_0().rounded(u(4.)).bg(theme.muted_foreground.opacity(0.5)))
            .child(div().flex_1().min_w_0().child(blocks(env, inner, key, cx)))
            .into_any_element(),
        Block::Code { text, .. } => div()
            .w_full()
            .my(u(2.))
            .px(u(10.))
            .py(u(8.))
            .rounded(u(4.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.muted.opacity(0.6))
            .font_family(theme.mono_font_family.clone())
            .text_size(u(size * 0.9))
            .child(text.clone())
            .into_any_element(),
        Block::List(items) => v_flex()
            .my(u(2.))
            .children(items.iter().enumerate().map(|(i, item)| list_item(env, item, &format!("{key}.{i}"), cx)))
            .into_any_element(),
    }
}

fn list_item(env: &Env, item: &ListItem, key: &str, cx: &App) -> AnyElement {
    let marker = match item.number {
        Some(n) => format!("{n}."),
        None if item.depth == 0 => "\u{2022}".to_string(),
        None => "\u{25e6}".to_string(),
    };
    h_flex()
        .items_start()
        .pl(u(2. + 16. * item.depth as f32))
        .py(u(2.))
        .gap(u(6.))
        .child(div().flex_shrink_0().min_w(u(12.)).child(marker))
        .child(div().flex_1().min_w_0().child(runs_element(env, key, &item.runs, false, cx)))
        .into_any_element()
}

/// Styled inline runs as one wrapping element.
pub fn runs_element(env: &Env, key: &str, runs: &[Run], jumbo: bool, cx: &App) -> AnyElement {
    let has_pictures = runs.iter().any(|r| matches!(r.content, Content::Emoji { .. } | Content::CustomEmoji { .. }));
    // Mentions get Discord's rounded, padded pill only as their own elements;
    // in one styled text they're a flat highlight. Pills break long text into
    // boxes that wrap apart, so only short lines (most pings) take that path.
    // The "(edited)" mark is smaller than its line, which a single text can't be.
    let has_pills = runs.iter().any(|r| !matches!(r.content, Content::Text(_)) || r.style.muted);
    let short = runs.iter().map(|r| if let Content::Text(t) = &r.content { t.len() } else { 12 }).sum::<usize>() <= SHORT_LINE;
    if has_pictures || (has_pills && short) { flow(env, key, runs, jumbo, true, cx) } else { text(env, key, runs, cx) }
}

/// Runs kept to a single line, whatever they hold: a reply's preview.
pub fn runs_line(env: &Env, key: &str, runs: &[Run], cx: &App) -> AnyElement {
    if runs.iter().any(|r| !matches!(r.content, Content::Text(_))) {
        flow(env, key, runs, false, false, cx)
    } else {
        text(env, key, runs, cx)
    }
}

/// How long (in bytes) a line can be and still be drawn as separate pieces.
const SHORT_LINE: usize = 90;

thread_local! {
    /// The clickable bit of text under the pointer: which text, and which bit of it.
    static HOVERED_CLICK: std::cell::Cell<Option<(u64, usize)>> = const { std::cell::Cell::new(None) };
}

fn hover_key(message: Id, key: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (message.0, key).hash(&mut hasher);
    hasher.finish()
}

fn text(env: &Env, key: &str, runs: &[Run], cx: &App) -> AnyElement {
    let now = chrono::Local::now();
    let mut content = String::new();
    let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let mut monospace: Vec<(Range<usize>, SharedString)> = Vec::new();
    let mut clicks: Vec<(Range<usize>, Click)> = Vec::new();
    let mut edited_mark: Option<Range<usize>> = None;
    // What hovering shows: where a link goes.
    let mut tips: Vec<(Range<usize>, SharedString)> = Vec::new();
    let hover_key = hover_key(env.message.id, key);
    let hovering = HOVERED_CLICK.get().filter(|(k, _)| *k == hover_key).map(|(_, i)| i);
    let mono: SharedString = cx.theme().mono_font_family.clone();
    for run in runs {
        let Some(mut label) = label(run, env, &now) else { continue };
        let look = look(run, env, cx);
        // A flat highlight can't be padded: hair spaces inside it stand in.
        if !matches!(run.content, Content::Text(_)) {
            label = format!("\u{200A}{label}\u{200A}");
        }
        let range = content.len()..content.len() + label.len();
        content.push_str(&label);
        if range.is_empty() {
            continue;
        }
        let under = |color: Option<Hsla>| UnderlineStyle { thickness: px(1.), color, wavy: false };
        highlights.push((
            range.clone(),
            HighlightStyle {
                color: look.color,
                font_weight: look.bold.then_some(FontWeight::BOLD),
                font_style: look.italic.then_some(FontStyle::Italic),
                background_color: look.background,
                underline: (look.underline || (look.click.is_some() && hovering == Some(clicks.len()))).then(|| under(None)),
                strikethrough: look.strike.then(|| StrikethroughStyle { thickness: px(1.), color: None }),
                ..HighlightStyle::default()
            },
        ));
        if look.mono {
            monospace.push((range.clone(), mono.clone()));
        }
        if run.style.muted {
            edited_mark = Some(range.clone());
        }
        if let Some(url) = &run.style.link {
            tips.push((range.clone(), url.clone().into()));
        }
        if let Some(click) = look.click {
            clicks.push((range, click));
        }
    }
    if content.is_empty() {
        return div().into_any_element();
    }
    let styled = StyledText::new(content).with_highlights(highlights).with_font_family_overrides(monospace);
    // Hovering "(edited)" says when.
    if let Some((mark, when)) = edited_mark.zip(crate::nav::edited_at(env.message, &chrono::Local)) {
        tips.push((mark, when.into()));
    }
    if clicks.is_empty() && tips.is_empty() {
        return div().child(styled).into_any_element();
    }
    let ranges: Vec<Range<usize>> = clicks.iter().map(|(r, _)| r.clone()).collect();
    let actions: Vec<_> = clicks.iter().map(|(_, c)| run_click(c, env.message.id, env.shared)).collect();
    let users: Vec<Option<Id>> = clicks.iter().map(|(_, c)| if let Click::User(id) = c { Some(*id) } else { None }).collect();
    let open_user = env.shared.open_user.clone();
    let id = ElementId::from((ElementId::from(("md", env.message.id.0)), key.to_string()));
    let mut interactive = InteractiveText::new(id, styled);
    if !ranges.is_empty() {
        // Whatever can be clicked underlines under the pointer.
        let spans = ranges.clone();
        interactive = interactive.on_hover(move |at, _, _, cx| {
            let now = at.and_then(|at| spans.iter().position(|r| r.contains(&at))).map(|i| (hover_key, i));
            let was = HOVERED_CLICK.get();
            if now != was && (now.is_some() || was.is_some_and(|(k, _)| k == hover_key)) {
                HOVERED_CLICK.set(now);
                crate::ui::redraw_chat(cx);
            }
        });
        interactive = interactive.on_click(ranges, move |i, window, cx| match users[i] {
            Some(user) => open_user(user, window.mouse_position(), window, cx),
            None => actions[i](cx),
        });
    }
    if !tips.is_empty() {
        interactive = interactive.tooltip(move |ix, window, cx| {
            let (_, tip) = tips.iter().find(|(range, _)| range.contains(&ix))?;
            // A long address wraps, instead of running off the screen.
            let tip = tip.clone();
            let widest = window.viewport_size().width * 0.6;
            Some(gpui_kit::component::tooltip::Tooltip::element(move |_, _| div().max_w(widest).child(tip.clone())).build(window, cx))
        });
    }
    // Its own hover only speaks while the pointer's over the text: this says it left.
    div()
        .id(ElementId::from((ElementId::from(("md-hover", env.message.id.0)), key.to_string())))
        .on_hover(move |over, _, cx| {
            if !over && HOVERED_CLICK.get().is_some_and(|(k, _)| k == hover_key) {
                HOVERED_CLICK.set(None);
                crate::ui::redraw_chat(cx);
            }
        })
        .child(interactive)
        .into_any_element()
}

/// Text runs cut into words, each keeping the spaces after it. Styled runs
/// touching with no space between (`**15**th`) stay together as one word.
fn split_words(runs: &[Run]) -> Vec<Vec<Run>> {
    let mut words = Vec::new();
    let mut word: Vec<Run> = Vec::new();
    for run in runs {
        let Content::Text(text) = &run.content else { continue };
        // A link is one thing to hover and click, however many words it has.
        if run.style.link.is_some() {
            word.push(run.clone());
            if text.ends_with(' ') {
                words.push(std::mem::take(&mut word));
            }
            continue;
        }
        let mut rest = text.as_str();
        while !rest.is_empty() {
            // Through the next run of spaces, or to the end.
            let body = rest.find(' ').unwrap_or(rest.len());
            let end = body + rest[body..].chars().take_while(|c| *c == ' ').count();
            word.push(Run { content: Content::Text(rest[..end].to_string()), style: run.style.clone() });
            if end > body {
                words.push(std::mem::take(&mut word));
            }
            rest = &rest[end..];
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// `runs` cut at their line breaks.
fn split_lines(runs: &[Run]) -> Vec<Vec<Run>> {
    let mut lines = vec![Vec::new()];
    for run in runs {
        let Content::Text(text) = &run.content else {
            lines.last_mut().expect("starts with one").push(run.clone());
            continue;
        };
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                lines.push(Vec::new());
            }
            if !part.is_empty() {
                lines.last_mut().expect("starts with one").push(Run { content: Content::Text(part.to_string()), style: run.style.clone() });
            }
        }
    }
    lines
}

/// Text, emoji and mentions in a wrapping row. Each stretch of plain text
/// between pictures and mention pills is one styled text, not an element per
/// word, so a long message with one emoji stays a handful of elements.
/// What a custom emoji's card is about. Its address has its ID in it.
fn custom_subject(name: &str, url: &str) -> crate::emoji_card::Subject {
    let id = url.rsplit("/emojis/").next().and_then(|tail| tail.split(['.', '?']).next()).and_then(|id| id.parse().ok()).unwrap_or(0);
    crate::emoji_card::Subject::Emoji { id: Id(id), name: name.to_string(), picture: url.to_string() }
}

/// An emoji that opens its card when clicked. Not where the row itself is the target.
fn card_on_click(env: &Env, element: AnyElement, (key, n): (&str, usize), subject: crate::emoji_card::Subject) -> AnyElement {
    if env.shared.inert {
        return element;
    }
    let id = SharedString::from(format!("emoji-{}-{key}-{n}", env.message.id));
    crate::emoji_card::clickable(element, id, subject, env.guild)
}

fn flow(env: &Env, key: &str, runs: &[Run], jumbo: bool, wrap: bool, cx: &App) -> AnyElement {
    // Line breaks don't exist in a row of pieces: each line gets its own, or
    // what follows one would sit beside the picture before it.
    let lines = split_lines(runs);
    if lines.len() > 1 {
        let height = u(env.shared.text_size * 22. / 16.);
        return v_flex()
            .w_full()
            .min_w_0()
            .children(lines.iter().enumerate().map(|(i, line)| {
                if line.is_empty() {
                    div().h(height).into_any_element()
                } else {
                    runs_element(env, &format!("{key}.l{i}"), line, jumbo, cx)
                }
            }))
            .into_any_element();
    }
    let now = chrono::Local::now();
    // Inline emoji are 1.375em, whatever size the text around them is.
    let size = u(if jumbo { 48. } else { env.shared.text_size * 1.375 });
    let mono = cx.theme().mono_font_family.clone();
    let mut parts: Vec<AnyElement> = Vec::new();
    let mut n = 0;
    while n < runs.len() {
        let run = &runs[n];
        match &run.content {
            Content::Emoji { grapheme, url } => {
                let name = emojis::get(grapheme).and_then(|e| e.shortcode()).unwrap_or("emoji").to_string();
                let subject = crate::emoji_card::Subject::Standard { name, picture: url.clone() };
                let grapheme = grapheme.clone();
                parts.push(card_on_click(
                    env,
                    img(url.clone())
                        .size(size)
                        .mx(u(1.))
                        .with_fallback(move || div().child(grapheme.clone()).into_any_element())
                        .into_any_element(),
                    (key, n),
                    subject,
                ));
                n += 1;
            }
            Content::CustomEmoji { name, url } if url.contains(".gif") => {
                let subject = custom_subject(name, url);
                parts.push(card_on_click(env, crate::anim::emoji(url.clone(), f32::from(env.shared.text_size) * if jumbo { 48. / env.shared.text_size } else { 1.375 }), (key, n), subject));
                n += 1;
            }
            Content::CustomEmoji { name, url } => {
                let subject = custom_subject(name, url);
                let name = name.clone();
                parts.push(card_on_click(
                    env,
                    // No margin: bots draw progress bars out of custom emoji.
                    img(url.clone())
                        .size(size)
                        .with_fallback(move || div().child(format!(":{name}:")).into_any_element())
                        .into_any_element(),
                    (key, n),
                    subject,
                ));
                n += 1;
            }
            Content::Text(_) => {
                let stretch = runs[n..].iter().take_while(|r| matches!(r.content, Content::Text(_))).count();
                if wrap {
                    // Word by word: a row wraps its pieces whole, so one long piece
                    // would drop to a line of its own instead of flowing on.
                    for (i, word) in split_words(&runs[n..n + stretch]).iter().enumerate() {
                        let mark = word.iter().all(|r| r.style.muted);
                        let size = env.shared.text_size * 0.625;
                        parts.push(div().max_w_full().min_w_0().when(mark, |d| d.text_size(u(size))).child(text(env, &format!("{key}.{n}.{i}"), word, cx)).into_any_element());
                    }
                } else {
                    parts.push(div().child(text(env, &format!("{key}.{n}"), &runs[n..n + stretch], cx)).into_any_element());
                }
                n += stretch;
            }
            _ => {
                if let Some(text) = label(run, env, &now) {
                    parts.push(pill_element(env, &look(run, env, cx), &mono, text, (key, n)));
                }
                n += 1;
            }
        }
    }
    let row = h_flex().w_full().min_w_0().when(wrap, |d| d.flex_wrap()).items_center().children(parts);
    // A wrapping row says it only needs its longest word, so a card sized by its content
    // came out as narrow as its title. This line, taking up no height, says what the
    // text would like in one line.
    if wrap && env.shared.in_card {
        let plain: String = runs
            .iter()
            .map(|run| match &run.content {
                Content::Text(text) => text.to_string(),
                Content::Emoji { .. } | Content::CustomEmoji { .. } => "   ".to_string(),
                _ => label(run, env, &now).map(|l| l.to_string()).unwrap_or_default(),
            })
            .collect();
        return v_flex().w_full().min_w_0().child(div().h_0().overflow_hidden().whitespace_nowrap().child(plain)).child(row).into_any_element();
    }
    row.into_any_element()
}

const MENTION_TEXT: u32 = 0xa9b4ff;

/// A mention or timestamp: one piece with a rounded background.
fn pill_element(env: &Env, look: &Look, mono: &SharedString, text: String, id: (&str, usize)) -> AnyElement {
    let (key, run) = id;
    let click = look.click.as_ref().map(|c| run_click(c, env.message.id, env.shared));
    let styled = div()
        .whitespace_nowrap()
        .when(look.bold, |d| d.font_weight(FontWeight::BOLD))
        .when(look.italic, Styled::italic)
        .when(look.underline, |d| d.underline())
        .when(look.strike, |d| d.line_through())
        .when(look.mono, |d| d.font_family(mono.clone()))
        .when_some(look.color, |d, c| d.text_color(c))
        .when_some(look.background, |d, c| d.bg(c))
        .px(u(2.))
        .rounded(u(3.))
        .child(text);
    let id = ElementId::from((ElementId::from(("mdw", env.message.id.0)), format!("{key}.{run}")));
    match (&look.click, click) {
        (Some(Click::User(user)), _) => {
            let (open, user, spot) = (env.shared.open_user.clone(), *user, NameSpot::default());
            let target = spot.clone();
            spotted(&spot, styled.id(id).cursor_pointer().hover(|d| d.underline()).on_click(move |e, window, cx| open(user, card_at(&target, e.position()), window, cx))).into_any_element()
        }
        (_, Some(click)) => styled.id(id).cursor_pointer().hover(|d| d.underline()).on_click(move |_, _, cx| click(cx)).into_any_element(),
        _ => styled.into_any_element(),
    }
}

#[cfg(test)]
mod split_tests {
    use super::*;
    use crate::markdown::Style;

    fn run(text: &str, bold: bool) -> Run {
        Run { content: Content::Text(text.into()), style: Style { bold, ..Style::default() } }
    }

    fn shape(words: &[Vec<Run>]) -> Vec<String> {
        words
            .iter()
            .map(|w| w.iter().map(|r| if let Content::Text(t) = &r.content { t.as_str() } else { "" }).collect())
            .collect()
    }

    #[test]
    fn a_link_stays_whole() {
        let link = Run { content: Content::Text("So High".into()), style: Style { link: Some("https://x".into()), ..Style::default() } };
        let runs = [run("added ", false), link, run(" by me", false)];
        assert_eq!(shape(&split_words(&runs)), ["added ", "So High ", "by ", "me"]);
    }

    #[test]
    fn words_keep_their_spaces_and_touching_styles() {
        let runs = [run(" you reached your ", false), run("15", true), run("th  level!", false)];
        assert_eq!(shape(&split_words(&runs)), [" ", "you ", "reached ", "your ", "15th  ", "level!"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_split_at_breaks_and_keep_their_pieces() {
        let text = |t: &str| Run { content: Content::Text(t.into()), style: Default::default() };
        let emoji = Run { content: Content::User(Id(1)), style: Default::default() };
        let lines = split_lines(&[text("one "), emoji.clone(), text("\ntwo\n\nfour")]);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].len(), 2);
        assert!(lines[2].is_empty());
    }

    #[test]
    fn parses_are_shared_until_the_cache_fills() {
        let mut cache = ParseCache::default();
        let a = cache.parse("**hi**");
        assert!(Rc::ptr_eq(&a, &cache.parse("**hi**")), "the second call reuses the first parse");
        assert!(!Rc::ptr_eq(&a, &cache.parse("hi")));
        for i in 0..PARSE_CACHE_CAP * 2 {
            cache.parse(&format!("message {i}"));
        }
        assert!(cache.parsed.len() <= PARSE_CACHE_CAP);
    }
}
