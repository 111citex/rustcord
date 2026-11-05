//! A forum or media channel: its posts as cards, how they're sorted and
//! filtered, the channel's guidelines, and the form that makes a new post.

use std::collections::HashMap;

use cord_core::forum;
use cord_core::model::Message;
use cord_core::{Id, NewPost, ThreadSearch};
use gpui_kit::assets::IconName;
use gpui_kit::component::avatar::Avatar;
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, Hsla, InteractiveElement as _, IntoElement, ObjectFit, ParentElement as _, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Window, div, img,
};

use crate::nav;
use crate::router::{RouteContext, navigate};
use crate::smooth_scroll::{ScrollTarget, SmoothScroller};
use crate::threads::{self as wording, CardTag, PostCard};
use crate::toasts::Toast;
use crate::ui::u;
use crate::uploads::Pending;

const BLURPLE: u32 = 0x5865f2;
/// The top bar's height, as in the chat page.
const BAR: f32 = 49.;

#[derive(Clone, Debug, PartialEq)]
enum Fetch {
    Idle,
    Loading,
    /// All asked for so far; whether Discord has more.
    Done(bool),
    Failed(String),
}

/// A post being written.
struct Draft {
    title: Entity<InputState>,
    body: Entity<TextareaState>,
    tags: Vec<Id>,
    files: Vec<Pending>,
    /// Posting is under way, or the last try was refused: the form says why.
    posting: bool,
    problem: Option<String>,
}

pub struct ForumView {
    ctx: RouteContext,
    channel: Id,
    by_creation: bool,
    /// Posts must carry one of these (none chosen: every post).
    tags: Vec<Id>,
    /// Each post's opening message, from the listings.
    firsts: HashMap<Id, Message>,
    fetch: Fetch,
    asked: u32,
    /// Bumped when sort or tags change, so a late answer to the old question is dropped.
    generation: u32,
    draft: Option<Draft>,
    scroll: ScrollHandle,
    scroller: SmoothScroller,
}

impl ForumView {
    pub fn new(ctx: RouteContext, channel: Id, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = ctx.services.store.clone();
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        let by_creation = store.read(cx).cache().channel(channel).and_then(|c| c.forum.as_ref()).is_some_and(|f| f.sort_order == 1);
        let scroll = ScrollHandle::new();
        let mut view = ForumView {
            ctx,
            channel,
            by_creation,
            tags: Vec::new(),
            firsts: HashMap::new(),
            fetch: Fetch::Idle,
            asked: 0,
            generation: 0,
            draft: None,
            scroller: SmoothScroller::new(ScrollTarget::Handle(scroll.clone())).calm_hover().layered(),
            scroll,
        };
        view.fetch_more(cx);
        view
    }

    /// Asks Discord for the next page of posts under the current sort and tags.
    fn fetch_more(&mut self, cx: &mut Context<Self>) {
        if matches!(self.fetch, Fetch::Loading) {
            return;
        }
        self.fetch = Fetch::Loading;
        let search = ThreadSearch { by_creation: self.by_creation, tags: self.tags.clone(), offset: self.asked, ..Default::default() };
        let (core, store, channel, generation) = (self.ctx.services.core.clone(), self.ctx.services.store.clone(), self.channel, self.generation);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = core.search_threads(channel, search).await;
            let _ = this.update(cx, |view, cx| {
                if view.generation != generation {
                    return;
                }
                match result {
                    Ok(page) => {
                        view.asked += page.threads.len() as u32;
                        view.fetch = Fetch::Done(page.has_more && !page.threads.is_empty());
                        for first in page.first_messages {
                            view.firsts.insert(first.channel_id, first);
                        }
                        store.update(cx, |store, cx| store.add_threads(page.threads, &page.joined, cx));
                    }
                    Err(error) => view.fetch = Fetch::Failed(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Sort or tags changed: ask again from the top.
    fn refetch(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.asked = 0;
        self.fetch = Fetch::Idle;
        self.fetch_more(cx);
    }

    fn toggle_tag(&mut self, tag: Id, cx: &mut Context<Self>) {
        match self.tags.iter().position(|t| *t == tag) {
            Some(at) => {
                self.tags.remove(at);
            }
            None => self.tags.push(tag),
        }
        self.refetch(cx);
    }

    fn sort_by(&mut self, creation: bool, cx: &mut Context<Self>) {
        if self.by_creation != creation {
            self.by_creation = creation;
            self.refetch(cx);
        }
    }

    fn open_post(&mut self, post: Id, cx: &mut Context<Self>) {
        let Some(path) = self.ctx.services.store.read(cx).cache().channel(post).map(nav::channel_path) else { return };
        navigate(&self.ctx.router, path, cx);
    }

    fn start_post(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let template = self.ctx.services.store.read(cx).cache().channel(self.channel).and_then(|c| c.forum.as_ref()).and_then(|f| f.template.clone());
        let title = cx.new(|cx| InputState::new(window, cx).placeholder("Post title"));
        let body = cx.new(|cx| TextareaState::new(window, cx).auto_grow(6, 16).placeholder("Write something to start your post"));
        if let Some(template) = template {
            body.update(cx, |body, cx| body.set_value(template, window, cx));
        }
        title.update(cx, |title, cx| title.focus(window, cx));
        self.draft = Some(Draft { title, body, tags: Vec::new(), files: Vec::new(), posting: false, problem: None });
        cx.notify();
    }

    fn cancel_post(&mut self, cx: &mut Context<Self>) {
        if let Some(draft) = self.draft.take() {
            for file in &draft.files {
                file.discard();
            }
        }
        cx.notify();
    }

    fn toggle_draft_tag(&mut self, tag: Id, cx: &mut Context<Self>) {
        if let Some(draft) = &mut self.draft {
            match draft.tags.iter().position(|t| *t == tag) {
                Some(at) => {
                    draft.tags.remove(at);
                }
                None => draft.tags.push(tag),
            }
            draft.problem = None;
            cx.notify();
        }
    }

    fn pick_files(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(gpui_kit::PathPromptOptions { files: true, directories: false, multiple: true, prompt: None });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let found: Vec<Pending> = paths.iter().map(|p| Pending::from_path(p)).collect();
            let _ = this.update(cx, |view, cx| view.attach(found, cx));
        })
        .detach();
    }

    fn attach(&mut self, found: Vec<Pending>, cx: &mut Context<Self>) {
        let limit = self.ctx.services.store.read(cx).cache().upload_limit(self.channel);
        let Some(draft) = &mut self.draft else { return };
        for file in found {
            let size = std::fs::metadata(&file.path).map_or(0, |m| m.len());
            if size > limit {
                let says = crate::uploads::too_large(&file.name, size, limit);
                file.discard();
                self.ctx.services.toast(Toast::warning("File too large", says), cx);
                continue;
            }
            if draft.files.len() >= crate::uploads::MAX_FILES {
                file.discard();
                self.ctx.services.toast(Toast::warning("Too many files", format!("A post can carry {} at most.", crate::uploads::MAX_FILES)), cx);
                break;
            }
            draft.files.push(file);
        }
        draft.problem = None;
        cx.notify();
    }

    /// The form's Post button: checks the post against the forum's rules, then makes it.
    fn submit_post(&mut self, cx: &mut Context<Self>) {
        let store = self.ctx.services.store.clone();
        let channel = self.channel;
        let Some(draft) = &mut self.draft else { return };
        if draft.posting {
            return;
        }
        let cache = store.read(cx).cache();
        let Some(info) = cache.channel(channel).and_then(|c| c.forum.clone()) else { return };
        let title = draft.title.read(cx).value().trim().to_string();
        let content = draft.body.read(cx).value().to_string();
        let manager = cache.can_manage_threads(channel);
        if let Some(problem) = forum::post_problem(&info, &title, &content, !draft.files.is_empty(), &draft.tags, manager) {
            draft.problem = Some(problem);
            cx.notify();
            return;
        }
        let content = match nav::prepare_message(&content) {
            Ok(content) => content,
            Err("") => String::new(),
            Err(message) => {
                draft.problem = Some(message.to_string());
                cx.notify();
                return;
            }
        };
        draft.posting = true;
        draft.problem = None;
        let files = draft.files.iter().cloned().filter_map(Pending::into_upload).collect();
        let post = NewPost { title, content, tags: draft.tags.clone() };
        let task = store.update(cx, |store, cx| store.create_post(channel, post, files, cx));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let made = task.await;
            let _ = this.update(cx, |view, cx| match made {
                Some(id) => {
                    view.draft = None;
                    view.open_post(id, cx);
                    cx.notify();
                }
                // Told by a toast; the form stays so nothing typed is lost. Its files were
                // read for the attempt, so they're picked again.
                None => {
                    if let Some(draft) = &mut view.draft {
                        draft.posting = false;
                        draft.problem = Some("The post wasn't made. Nothing you wrote is lost.".to_string());
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

fn pill(id: impl Into<gpui_kit::ElementId>, tag: &CardTag, on: bool, dim: bool, cx: &App) -> gpui_kit::Stateful<gpui_kit::Div> {
    let theme = cx.theme();
    let blurple = Hsla::from(gpui_kit::rgb(BLURPLE));
    h_flex()
        .id(id)
        .h(u(24.))
        .px(u(8.))
        .gap(u(4.))
        .flex_shrink_0()
        .items_center()
        .rounded(u(6.))
        .text_size(u(12.))
        .font_weight(FontWeight::MEDIUM)
        .bg(if on { blurple.opacity(0.35) } else { theme.foreground.opacity(0.08) })
        .text_color(if dim { theme.muted_foreground.opacity(0.6) } else { theme.foreground })
        .when_some(tag.emoji_name.clone().filter(|_| tag.emoji_id.is_none()), |d, emoji| d.child(crate::badges::emoji_image(&emoji, 14.)))
        .when_some(tag.emoji_id, |d, id| d.child(img(format!("https://cdn.discordapp.com/emojis/{id}.png?size=32")).size(u(14.)).flex_shrink_0()))
        .child(tag.name.clone())
}

fn card_tag(tag: &cord_core::model::ForumTag) -> CardTag {
    CardTag { name: tag.name.clone(), emoji_name: tag.emoji_name.clone(), emoji_id: tag.emoji_id }
}

/// The reactions on a post's opening message, added up.
fn reaction_total(first: Option<&Message>) -> u32 {
    first.map_or(0, |m| m.reactions.iter().map(|r| r.count).sum())
}

impl Render for ForumView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let store = self.ctx.services.store.read(cx);
        let cache = store.cache();
        let Some(channel) = cache.channel(self.channel) else {
            return div().size_full().flex().items_center().justify_center().text_color(muted).child("This channel doesn't exist, or you can't see it.").into_any_element();
        };
        if !cache.can_view(self.channel) {
            return div().size_full().flex().items_center().justify_center().text_color(muted).child("You can't view this channel.").into_any_element();
        }
        let media = channel.kind == cord_core::model::ChannelKind::Media;
        let info = channel.forum.clone().unwrap_or_default();
        let gallery = media || info.layout == 2;
        let label = nav::channel_label(cache, channel);
        let lock = nav::composer_lock(cache, channel);
        let _ = window;
        let now = chrono::Utc::now().timestamp_millis().max(0) as u64;

        // The posts we know of, as the forum would list them.
        let mut posts = cache.threads_under(self.channel);
        posts.retain(|p| forum::matches_tags(p, &self.tags));
        forum::sort_posts(&mut posts, self.by_creation);
        let cards: Vec<PostCard> = posts.iter().map(|p| wording::post_card(cache, p, self.firsts.get(&p.id))).collect();
        let totals: Vec<u32> = posts.iter().map(|p| reaction_total(self.firsts.get(&p.id))).collect();

        let header = h_flex()
            .h(u(BAR))
            .pl(u(16.))
            .pr(u(8.))
            .gap(u(8.))
            .flex_shrink_0()
            .items_center()
            .border_b_1()
            .border_color(border)
            .child(Icon::new(if media { IconName::Image } else { IconName::MessageSquare }).size(u(24.)).text_color(muted))
            .child(div().flex_shrink_0().text_size(u(16.)).font_weight(FontWeight::SEMIBOLD).child(label.clone()));

        let blurple = Hsla::from(gpui_kit::rgb(BLURPLE));
        let new_post = match (&lock, self.draft.is_some()) {
            (_, true) => None,
            (None, false) => Some(
                div()
                    .id("new-post")
                    .h(u(32.))
                    .px(u(14.))
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .rounded(u(6.))
                    .bg(blurple)
                    .text_color(gpui_kit::white())
                    .text_size(u(14.))
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .hover(|d| d.opacity(0.85))
                    .on_click(cx.listener(|this, _, window, cx| this.start_post(window, cx)))
                    .child("New Post")
                    .into_any_element(),
            ),
            (Some(reason), false) => Some(div().text_size(u(13.)).text_color(muted).child(reason.clone()).into_any_element()),
        };

        let segment = |id: &'static str, text: &'static str, on: bool, creation: bool| {
            div()
                .id(id)
                .h(u(28.))
                .px(u(10.))
                .flex()
                .items_center()
                .rounded(u(6.))
                .text_size(u(13.))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .bg(if on { theme.foreground.opacity(0.12) } else { gpui_kit::transparent_black() })
                .text_color(if on { theme.foreground } else { muted })
                .hover(|d| d.bg(theme.foreground.opacity(0.08)))
                .on_click(cx.listener(move |this, _, _, cx| this.sort_by(creation, cx)))
                .child(text)
        };
        let toolbar = h_flex()
            .w_full()
            .gap(u(8.))
            .items_center()
            .flex_wrap()
            .child(div().text_size(u(12.)).font_weight(FontWeight::BOLD).text_color(muted).child("SORT BY"))
            .child(segment("sort-activity", "Recent Activity", !self.by_creation, false))
            .child(segment("sort-created", "Date Posted", self.by_creation, true))
            .child(div().flex_1())
            .children(new_post);

        let tag_filter = (!info.tags.is_empty()).then(|| {
            h_flex().w_full().gap(u(6.)).items_center().flex_wrap().child(div().text_size(u(12.)).font_weight(FontWeight::BOLD).text_color(muted).child("TAGS")).children(
                info.tags.iter().enumerate().map(|(i, t)| {
                    let id = t.id;
                    pill(("filter-tag", i), &card_tag(t), self.tags.contains(&id), false, cx).cursor_pointer().on_click(cx.listener(move |this, _, _, cx| this.toggle_tag(id, cx)))
                }),
            )
        });

        let guidelines = channel.topic.clone().map(|topic| {
            v_flex()
                .w_full()
                .p(u(16.))
                .gap(u(6.))
                .rounded(u(8.))
                .border_1()
                .border_color(border)
                .bg(theme.foreground.opacity(0.03))
                .child(div().text_size(u(12.)).font_weight(FontWeight::BOLD).text_color(muted).child("GUIDELINES"))
                .children(topic.lines().map(|line| div().text_size(u(14.)).child(SharedString::from(line.trim().to_string()))).collect::<Vec<_>>())
        });

        let status = match &self.fetch {
            Fetch::Loading => Some("Loading posts\u{2026}".to_string()),
            Fetch::Failed(error) => Some(format!("Couldn't load posts: {error}")),
            _ if cards.is_empty() => Some(if self.tags.is_empty() { "No posts yet. Be the first." } else { "No posts with these tags." }.to_string()),
            _ => None,
        };
        let more = matches!(self.fetch, Fetch::Done(true));
        let card_views: Vec<AnyElement> = cards
            .iter()
            .zip(&totals)
            .enumerate()
            .map(|(ix, (card, reactions))| self.card(ix, card, *reactions, gallery, now, cx))
            .collect();

        let body: AnyElement = match &self.draft {
            Some(draft) => self.form(draft, &info, cache.can_manage_threads(self.channel), cx),
            None => v_flex()
                .w_full()
                .gap(u(12.))
                .child(toolbar)
                .children(tag_filter)
                .children(guidelines)
                .child(if gallery {
                    h_flex().w_full().gap(u(12.)).flex_wrap().items_start().children(card_views).into_any_element()
                } else {
                    v_flex().w_full().gap(u(8.)).children(card_views).into_any_element()
                })
                .when_some(status, |d, text| d.child(div().p(u(24.)).text_center().text_size(u(14.)).text_color(muted).child(text)))
                .when(more, |d| {
                    d.child(
                        div()
                            .id("posts-more")
                            .h(u(36.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(u(6.))
                            .cursor_pointer()
                            .text_size(u(14.))
                            .text_color(theme.link)
                            .hover(|d| d.bg(theme.foreground.opacity(0.06)))
                            .on_click(cx.listener(|this, _, _, cx| this.fetch_more(cx)))
                            .child("Load more posts"),
                    )
                })
                .into_any_element(),
        };
        let smooth = self.ctx.services.settings.read(cx).get().smooth_scrolling;
        v_flex()
            .size_full()
            .child(header)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("forum-posts")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .child(div().w_full().max_w(u(980.)).mx_auto().p(u(16.)).child(body)),
                    )
                    .when(smooth, |d| d.child(self.scroller.layer())),
            )
            .into_any_element()
    }
}

impl ForumView {
    /// One post: a row in the list, or a tile in the gallery.
    fn card(&self, ix: usize, card: &PostCard, reactions: u32, gallery: bool, now: u64, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let id = card.id;
        let title = div()
            .flex_1()
            .min_w_0()
            .truncate()
            .text_size(u(16.))
            .font_weight(if card.unread { FontWeight::BOLD } else { FontWeight::SEMIBOLD })
            .child(SharedString::from(card.title.clone()));
        let tags = h_flex().gap(u(6.)).flex_wrap().children(card.tags.iter().enumerate().map(|(i, t)| pill(("card-tag", ix * 8 + i), t, false, false, cx)));
        let footer = h_flex()
            .gap(u(12.))
            .items_center()
            .text_size(u(12.))
            .text_color(muted)
            .children(card.author.clone().map(|name| {
                h_flex()
                    .gap(u(6.))
                    .items_center()
                    .min_w_0()
                    .when_some(card.author_avatar.clone(), |d, url| d.child(Avatar::new().border_0().src(url).name(name.clone()).size(u(18.))))
                    .child(div().truncate().font_weight(FontWeight::MEDIUM).child(SharedString::from(name)))
            }))
            .when(reactions > 0, |d| d.child(h_flex().gap(u(4.)).items_center().child(Icon::new(IconName::FaceSlightlySmiling).size(u(14.))).child(reactions.to_string())))
            .child(h_flex().gap(u(4.)).items_center().child(Icon::new(IconName::MessageCircle).size(u(14.))).child(card.replies.to_string()))
            .child(div().flex_shrink_0().child(format!("Active {}", wording::ago(now, card.active_ms))))
            .when(card.locked, |d| d.child("Locked"))
            .when(card.archived, |d| d.child("Closed"));
        let text = v_flex()
            .flex_1()
            .min_w_0()
            .gap(u(6.))
            .child(
                h_flex()
                    .gap(u(6.))
                    .items_center()
                    .when(card.pinned, |d| d.child(Icon::default().path("icons/toolbar-pin.svg").size(u(14.)).text_color(muted)))
                    .when(card.unread, |d| d.child(div().flex_shrink_0().size(u(8.)).rounded_full().bg(theme.foreground)))
                    .child(title),
            )
            .when(!card.preview.is_empty(), |d| d.child(div().text_size(u(14.)).text_color(muted).line_clamp(2).child(SharedString::from(card.preview.clone()))))
            .when(!card.tags.is_empty(), |d| d.child(tags))
            .child(footer);
        let picture = |size_w: Option<f32>, h: f32| {
            card.image.clone().map(|url| {
                img(url).h(u(h)).when_some(size_w, |d, w| d.w(u(w))).when(size_w.is_none(), |d| d.w_full()).flex_shrink_0().rounded(u(6.)).object_fit(ObjectFit::Cover)
            })
        };
        let base = div()
            .id(("post", ix))
            .p(u(12.))
            .rounded(u(8.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .cursor_pointer()
            .hover(|d| d.bg(theme.foreground.opacity(0.06)))
            .on_click(cx.listener(move |this, _, _, cx| this.open_post(id, cx)));
        if gallery {
            base.w(u(300.)).flex_shrink_0().child(v_flex().gap(u(8.)).children(picture(None, 170.)).child(text)).into_any_element()
        } else {
            base.w_full().child(h_flex().gap(u(12.)).items_start().child(text).children(picture(Some(120.), 90.))).into_any_element()
        }
    }

    fn form(&self, draft: &Draft, info: &cord_core::model::ForumInfo, manager: bool, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let blurple = Hsla::from(gpui_kit::rgb(BLURPLE));
        let heading = |text: &'static str| div().text_size(u(12.)).font_weight(FontWeight::BOLD).text_color(muted).child(text);
        let button = |id: &'static str, label: &'static str, bg: Hsla, fg: Hsla| {
            div()
                .id(id)
                .h(u(36.))
                .px(u(16.))
                .flex()
                .items_center()
                .rounded(u(8.))
                .bg(bg)
                .text_color(fg)
                .text_size(u(14.))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .hover(|d| d.opacity(0.85))
                .child(label)
        };
        let tags = (!info.tags.is_empty()).then(|| {
            let required = info.requires_tag() && !manager;
            v_flex()
                .gap(u(8.))
                .child(heading(if required { "TAGS (CHOOSE AT LEAST ONE)" } else { "TAGS" }))
                .child(h_flex().gap(u(6.)).flex_wrap().children(info.tags.iter().enumerate().map(|(i, t)| {
                    let (id, moderated) = (t.id, t.moderated && !manager);
                    let on = draft.tags.contains(&id);
                    let p = pill(("draft-tag", i), &card_tag(t), on, moderated, cx);
                    if moderated {
                        p.cursor_not_allowed().tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Only moderators can use this tag").build(window, cx))
                    } else {
                        p.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| this.toggle_draft_tag(id, cx)))
                    }
                })))
        });
        let files = h_flex().gap(u(8.)).flex_wrap().items_center().children(draft.files.iter().enumerate().map(|(i, file)| {
            h_flex()
                .gap(u(6.))
                .h(u(28.))
                .px(u(8.))
                .items_center()
                .rounded(u(6.))
                .bg(theme.foreground.opacity(0.08))
                .text_size(u(13.))
                .child(Icon::new(if file.is_image() { IconName::Image } else { IconName::Paperclip }).size(u(14.)))
                .child(div().max_w(u(220.)).truncate().child(SharedString::from(file.name.clone())))
                .child(
                    div()
                        .id(("draft-file", i))
                        .cursor_pointer()
                        .text_color(muted)
                        .hover(|d| d.text_color(theme.danger))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(draft) = &mut this.draft
                                && i < draft.files.len()
                            {
                                draft.files.remove(i).discard();
                                cx.notify();
                            }
                        }))
                        .child(Icon::new(IconName::Close).size(u(14.))),
                )
        }));
        v_flex()
            .w_full()
            .gap(u(16.))
            .p(u(20.))
            .rounded(u(12.))
            .border_1()
            .border_color(border)
            .bg(theme.popover)
            .child(div().text_size(u(20.)).font_weight(FontWeight::SEMIBOLD).child("New Post"))
            .child(v_flex().gap(u(8.)).child(heading("TITLE")).child(Input::new(&draft.title)))
            .child(v_flex().gap(u(8.)).child(heading("POST")).child(Textarea::new(&draft.body)))
            .children(tags)
            .child(
                v_flex().gap(u(8.)).child(heading("FILES")).child(files).child(
                    div()
                        .id("draft-attach")
                        .h(u(32.))
                        .px(u(12.))
                        .flex()
                        .items_center()
                        .self_start()
                        .gap(u(6.))
                        .rounded(u(6.))
                        .border_1()
                        .border_color(border)
                        .text_size(u(13.))
                        .cursor_pointer()
                        .hover(|d| d.bg(theme.foreground.opacity(0.06)))
                        .on_click(cx.listener(|this, _, _, cx| this.pick_files(cx)))
                        .child(Icon::new(IconName::Plus).size(u(14.)))
                        .child("Attach files"),
                ),
            )
            .when_some(draft.problem.clone(), |d, problem| d.child(div().text_size(u(13.)).text_color(theme.danger).child(problem)))
            .child(
                h_flex()
                    .justify_end()
                    .gap(u(8.))
                    .child(button("post-cancel", "Cancel", theme.foreground.opacity(0.08), theme.foreground).on_click(cx.listener(|this, _, _, cx| this.cancel_post(cx))))
                    .child(
                        button("post-submit", if draft.posting { "Posting\u{2026}" } else { "Post" }, blurple, gpui_kit::white())
                            .when(draft.posting, |d| d.opacity(0.5))
                            .on_click(cx.listener(|this, _, _, cx| this.submit_post(cx))),
                    ),
            )
            .into_any_element()
    }
}
