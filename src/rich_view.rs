//! Everything in a message besides its markdown text, drawn like Discord:
//! reply lines, attachments, stickers, embeds and Components V2 layouts.
//! Display only: link buttons and pictures open in the browser, nothing else
//! reacts.

use cord_core::cache::Cache;
use cord_core::cdn;
use cord_core::model::{Message, Reaction};
use cord_core::rich::{
    Attachment, Button, Component, ComponentEmoji, ComponentMedia, Embed, EmbedField, EmbedMedia, GalleryItem, Select,
    StickerItem,
};
use gpui_kit::AnimationExt;
use gpui_kit::SharedString;
use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, App, ElementId, FontWeight, Hsla, InteractiveElement, IntoElement, ObjectFit, ParentElement,
    StatefulInteractiveElement, Styled, StyledImage, div, img,
};

use crate::ui::u;
use crate::markdown;
use crate::markdown_view::{self, Env, Shared};
use crate::nav;

/// Pictures are scaled to fit in this box, like Discord's chat.
const IMAGE_MAX: (f32, f32) = (400., 300.);
/// Embeds and layout containers don't grow past this.
/// Discord's embed cards are at most 520px wide, and Components V2
/// containers 600px; both shrink to what they hold.
const CARD_MAX_W: f32 = 520.;
const CONTAINER_MAX_W: f32 = 600.;
/// A reaction pill's height and its emoji, as in Discord.
const REACTION_H: f32 = 30.;
const REACTION_EMOJI: f32 = 16.;
/// A message sets this flag when its `components` are the whole layout and
/// its attachments are only referenced from inside them.
const FLAG_COMPONENTS_V2: u64 = 1 << 15;
/// Message type of a reply.
const KIND_REPLY: u8 = 19;

/// The size an image is drawn at: its own, shrunk to fit the box with the
/// aspect ratio kept. Known up front so the list doesn't jump on load.
pub fn fit_image(width: u32, height: u32, max_w: f32, max_h: f32) -> (f32, f32) {
    let (w, h) = (width.max(1) as f32, height.max(1) as f32);
    let scale = (max_w / w).min(max_h / h).min(1.0);
    ((w * scale).round().max(1.), (h * scale).round().max(1.))
}

/// "512 B", "1.5 KB", "20.0 MB".
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024. && unit < UNITS.len() - 1 {
        value /= 1024.;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

/// Where a sticker's picture lives. Lottie stickers (format 3) have none.
pub fn sticker_url(sticker: &StickerItem) -> Option<String> {
    let id = sticker.id;
    match sticker.format_type {
        1 | 2 => Some(format!("https://media.discordapp.net/stickers/{id}.png?size=160")),
        4 => Some(format!("https://media.discordapp.net/stickers/{id}.gif?size=160")),
        _ => None,
    }
}

/// Groups embed fields into rows: consecutive inline fields share a row, up
/// to three; a block field has a row to itself. Returns field indices.
pub fn field_rows(fields: &[EmbedField]) -> Vec<Vec<usize>> {
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut inline: Vec<usize> = Vec::new();
    for (i, field) in fields.iter().enumerate() {
        if field.inline {
            inline.push(i);
            if inline.len() == 3 {
                rows.push(std::mem::take(&mut inline));
            }
        } else {
            if !inline.is_empty() {
                rows.push(std::mem::take(&mut inline));
            }
            rows.push(vec![i]);
        }
    }
    if !inline.is_empty() {
        rows.push(inline);
    }
    rows
}

/// A component's `attachment://name` reference, resolved against the
/// message's attachments to something loadable.
fn resolve_media<'a>(message: &'a Message, media: &'a ComponentMedia) -> (&'a str, Option<(u32, u32)>) {
    if let Some(name) = media.url.strip_prefix("attachment://") {
        if let Some(a) = message.attachments.iter().find(|a| a.filename == name) {
            return (a.display_url(), a.dimensions());
        }
    }
    (media.display_url(), media.dimensions())
}

fn is_http(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

fn eid(env: &Env, name: &str, key: &str) -> ElementId {
    ElementId::from((ElementId::from(("rv", env.message.id.0)), format!("{name}.{key}")))
}

/// A picture at its fitted size; clicking opens `open` in the browser.
fn picture(id: ElementId, src: &str, open: &str, dims: Option<(u32, u32)>, max: (f32, f32), cx: &App) -> AnyElement {
    picture_with(id, src, open, dims, max, dims, cx)
}

/// `picture`, where the viewer should show `real` (the file's own size)
/// rather than the box it's cropped to here.
fn picture_with(
    id: ElementId,
    src: &str,
    open: &str,
    dims: Option<(u32, u32)>,
    max: (f32, f32),
    real: Option<(u32, u32)>,
    cx: &App,
) -> AnyElement {
    let (w, h, fit) = match dims {
        Some((w, h)) => {
            let (w, h) = fit_image(w, h, max.0, max.1);
            (w, h, ObjectFit::Cover)
        }
        // Size unknown: a fixed box with the picture inside whole.
        None => (max.0.min(300.), max.1.min(200.), ObjectFit::Contain),
    };
    let fitted = picture_url(src, w, h, real);
    let open = open.to_string();
    let (full, full_dims) = (src.to_string(), real);
    let theme = cx.theme();
    div()
        .id(id)
        // Narrower than the picture, it shrinks whole instead of being cut off.
        .relative()
        .w(u(w))
        .max_w_full()
        .min_w_0()
        .aspect_ratio(w / h.max(1.))
        .rounded(u(8.))
        .overflow_hidden()
        .bg(theme.secondary)
        // A picture that's taking its time: a spinner, appearing only if it's slow to.
        .when(crate::anim::is_loading(&fitted), |d| {
            d.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(gpui_kit::component::spinner::Spinner::new().color(theme.muted_foreground))
                    .with_animation(
                        SharedString::from(format!("picture-wait-{fitted}")),
                        gpui_kit::Animation::new(std::time::Duration::from_millis(600)),
                        |d, t| d.opacity((t * 2. - 1.).max(0.)),
                    ),
            )
        })
        .cursor_pointer()
        // Pictures open in the viewer; "Open in Browser" there leads on to `open`.
        // Right click has the viewer's Copy and Save without opening it first.
        .when(is_http(&open), |d| {
            let (menu_src, menu_link) = (full.clone(), open.clone());
            d.on_mouse_down(gpui_kit::MouseButton::Right, move |event: &gpui_kit::MouseDownEvent, _, cx: &mut App| {
                cx.stop_propagation();
                crate::viewer::show_menu(event.position, menu_src.clone(), menu_link.clone(), cx);
            })
            .on_click(move |_, _, cx| crate::viewer::show(&full, &open, full_dims, cx))
        })
        // GPUI doesn't clip children to rounded corners: the image rounds itself.
        .child(
            crate::anim::image_fading(fitted.clone(), cdn::original_url(src), fit, 8., theme.secondary).absolute().top_0().left_0().size_full(),
        )
        .into_any_element()
}

/// Pictures from this size up are looked at, and worth the memory; smaller ones aren't.
const SHARP_FROM: f32 = 120.;

/// The size to ask for: the proxy fits a picture inside what it's asked for, so a
/// picture being cropped to a box has to be asked for big enough to cover it.
fn covering_size(w: f32, h: f32, real: Option<(u32, u32)>) -> (f32, f32) {
    match real.filter(|(rw, rh)| *rw > 0 && *rh > 0) {
        Some((rw, rh)) => {
            let scale = (w / rw as f32).max(h / rh as f32);
            (rw as f32 * scale, rh as f32 * scale)
        }
        None => (w, h),
    }
}

/// The address for a picture shown in a `w` x `h` box.
fn picture_url(src: &str, w: f32, h: f32, real: Option<(u32, u32)>) -> String {
    let (cover_w, cover_h) = covering_size(w, h, real);
    if w.max(h) < SHARP_FROM {
        return cdn::fitted_url(src, cover_w.ceil() as u32, cover_h.ceil() as u32);
    }
    // Twice the display's pixels, but no more than the file has.
    let times = (2. * cdn::device_scale()).min(real.map_or(f32::MAX, |(rw, _)| rw as f32 / cover_w.max(1.))).max(1.);
    cdn::sharp_url(src, (cover_w * times).ceil() as u32, (cover_h * times).ceil() as u32)
}

fn file_card(id: ElementId, name: &str, size: Option<u64>, url: &str, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let (url, link) = (url.to_string(), url.to_string());
    let download_id = gpui_kit::SharedString::from(format!("download-{link}"));
    h_flex()
        .id(id)
        .gap_3()
        .items_center()
        .w_full()
        .max_w(u(CARD_MAX_W * 0.75))
        .px_3()
        .py_2()
        .rounded(u(8.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .when(is_http(&url), |d| d.cursor_pointer().on_click(move |_, _, cx| crate::links::open(&url, cx)))
        .child(div().text_xl().child("\u{1f4c4}"))
        .child(
            v_flex()
                .min_w_0()
                .child(div().truncate().text_color(theme.link).child(name.to_string()))
                .when_some(size, |d, size| d.child(div().text_xs().text_color(theme.muted_foreground).child(format_bytes(size)))),
        )
        .child(div().flex_1())
        .when(is_http(&link), |d| d.child(crate::file_preview::download_button(download_id, &link, name, cx)))
        .into_any_element()
}

fn attachment(env: &Env, a: &Attachment, cx: &App) -> AnyElement {
    let key = a.id.to_string();
    if crate::audio::is_audio(a) {
        if crate::audio::is_voice_message(env.message.flags) {
            crate::audio::voice(env, a, cx)
        } else {
            crate::audio::file(env, a, format_bytes(a.size), cx)
        }
    } else if a.is_image() {
        picture(eid(env, "att", &key), a.display_url(), &a.url, a.dimensions(), IMAGE_MAX, cx)
    } else if a.is_video() {
        let (w, h) = a.dimensions().map_or((400., 225.), |(w, h)| fit_image(w, h, VIDEO_MAX.0, VIDEO_MAX.1));
        let poster = picture_url(a.display_url(), w, h, a.dimensions());
        crate::videos::video_box(eid(env, "video", &key), &a.url, &poster, (w, h), cx)
    } else {
        crate::file_preview::previewable(a)
            .then(|| crate::file_preview::view(env, a, format_bytes(a.size), cx))
            .flatten()
            .unwrap_or_else(|| file_card(eid(env, "file", &key), &a.filename, Some(a.size), &a.url, cx))
    }
}

/// Discord's media mosaic is 550px wide with 4px gutters.
const MOSAIC_W: f32 = 550.;
const MOSAIC_GAP: f32 = 4.;
const VIDEO_MAX: (f32, f32) = (550., 350.);
/// A folded embed's picture grid, inside the card's padding.
const EMBED_GRID_W: f32 = 400.;

/// How many tiles go in each row of a mosaic of `n` pictures, top to bottom:
/// the leftover one or two first, then rows of three (2, 4 and 3 are special).
pub fn mosaic_rows(n: usize) -> Vec<usize> {
    match n {
        0 => vec![],
        1 => vec![1],
        2 => vec![2],
        4 => vec![2, 2],
        _ => {
            let mut rows = vec![3; n / 3];
            if n % 3 != 0 {
                rows.insert(0, n % 3);
            }
            rows
        }
    }
}

/// A message's attachments: one picture alone keeps its shape, several form
/// Discord's mosaic, and other files follow as cards.
/// A file on its way to Discord: how far along, how fast, and how long is left.
fn uploading(env: &Env, a: &Attachment, cx: &App) -> AnyElement {
    use crate::uploads::{Progress, progress, status_line};
    let theme = cx.theme();
    let (fraction, status, moving, failed) = match progress(env.message.id, a.id.0) {
        Progress::Failed => (0., "Upload failed".to_string(), false, true),
        Progress::Finishing => (1., format!("Finishing  \u{2022}  {}", format_bytes(a.size)), false, false),
        Progress::Sending(sent, secs) => ((sent as f32 / a.size.max(1) as f32).min(1.), status_line(sent, a.size, secs), true, false),
    };
    v_flex()
        .w_full()
        .max_w(u(CARD_MAX_W * 0.75))
        .mt(u(4.))
        .p(u(12.))
        .gap(u(6.))
        .rounded(u(8.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .child(div().min_w_0().truncate().text_size(u(13.)).font_weight(FontWeight::MEDIUM).child(a.filename.clone()))
        .child(
            div().w_full().h(u(4.)).rounded(u(2.)).overflow_hidden().bg(theme.muted).child(
                div().h_full().w(gpui_kit::relative(fraction)).rounded(u(2.)).bg(if failed { theme.danger } else { gpui_kit::Hsla::from(gpui_kit::rgb(0x5865f2)) }),
            ),
        )
        .child(div().truncate().text_size(u(12.)).text_color(if failed { theme.danger } else { theme.muted_foreground }).child(status))
        // Ten updates a second is plenty for a number.
        .when(moving, |d| {
            d.child(gpui_kit::canvas(|_, _, _| {}, |_, _, window, cx| alhazen_gpui::redraw_after(std::time::Duration::from_millis(100), window, cx)).absolute().size_0())
        })
        .into_any_element()
}

fn attachments(env: &Env, all: &[Attachment], cx: &App) -> Vec<AnyElement> {
    if all.iter().any(|a| a.url.is_empty()) {
        return all.iter().map(|a| uploading(env, a, cx)).collect();
    }
    let (pictures, files): (Vec<&Attachment>, Vec<&Attachment>) = all.iter().partition(|a| a.is_image());
    let mut out = Vec::new();
    match pictures.len() {
        0 => {}
        1 => out.push(attachment(env, pictures[0], cx)),
        _ => out.push(mosaic(env, &pictures, cx)),
    }
    out.extend(files.into_iter().map(|a| attachment(env, a, cx)));
    out
}

fn mosaic(env: &Env, pictures: &[&Attachment], cx: &App) -> AnyElement {
    let tiles: Vec<Tile> = pictures.iter().map(|a| Tile { key: a.id.to_string(), src: a.display_url(), open: &a.url, real: a.width.zip(a.height) }).collect();
    tile_grid(env, "att", &tiles, MOSAIC_W, cx)
}

/// One picture of a mosaic: its element key, what to draw and what a click opens.
struct Tile<'a> {
    key: String,
    src: &'a str,
    open: &'a str,
    real: Option<(u32, u32)>,
}

/// Discord's mosaic of `tiles`, `width` wide.
fn tile_grid(env: &Env, name: &str, tiles: &[Tile], width: f32, cx: &App) -> AnyElement {
    let tile = |t: &Tile, w: f32, h: f32| picture_with(eid(env, name, &t.key), t.src, t.open, Some((w as u32, h as u32)), (w, h), t.real, cx);
    let pictures = tiles;
    // Three: one tall picture on the left, two stacked on the right.
    if pictures.len() == 3 {
        let w = (width - MOSAIC_GAP) / 2.;
        let tall = w * 4. / 3.;
        let short = (tall - MOSAIC_GAP) / 2.;
        return h_flex()
            .gap(u(MOSAIC_GAP))
            .child(tile(&pictures[0], w, tall))
            .child(v_flex().gap(u(MOSAIC_GAP)).child(tile(&pictures[1], w, short)).child(tile(&pictures[2], w, short)))
            .into_any_element();
    }
    let mut rest = pictures.iter();
    v_flex()
        .gap(u(MOSAIC_GAP))
        .children(mosaic_rows(pictures.len()).into_iter().map(|per_row| {
            let w = (width - MOSAIC_GAP * (per_row as f32 - 1.)) / per_row as f32;
            // Rows of two or three are squares; a lone top row is a wide strip.
            let h = if per_row == 1 { width / 2. } else { w };
            h_flex()
                .gap(u(MOSAIC_GAP))
                .children(rest.by_ref().take(per_row).map(|a| tile(a, w, h)))
        }))
        .into_any_element()
}

fn sticker(env: &Env, s: &StickerItem, cx: &App) -> AnyElement {
    // The card opens past its right edge, level with its top.
    let spot = markdown_view::NameSpot::default();
    let spot_layer = |spot: &markdown_view::NameSpot| {
        let spot = spot.clone();
        gpui_kit::canvas(move |at, _, _| spot.set(Some(at)), |_, _, _, _| {}).absolute().top_0().left_0().size_full()
    };
    if crate::stickers::is_rendered(s.format_type) {
        // A fixed box while it renders, so the row keeps its height.
        return match crate::stickers::image(s.id, cx) {
            Ok(image) => div()
                .id(eid(env, "sticker", &s.id.to_string()))
                .relative()
                .size(u(160.))
                .cursor_pointer()
                .child(spot_layer(&spot))
                .on_click({
                    let subject = crate::emoji_card::Subject::Sticker { id: s.id, name: s.name.clone(), picture: sticker_url(s).unwrap_or_default() };
                    let (here, target) = (env.guild, spot.clone());
                    move |event, _, cx| {
                        cx.stop_propagation();
                        crate::emoji_card::toggle(subject.clone(), here, markdown_view::card_at(&target, event.position()), cx);
                    }
                })
                .children(image.map(|image| crate::anim::image(image, ObjectFit::Contain, 0.).size(u(160.))))
                .into_any_element(),
            Err(()) => div().italic().text_color(cx.theme().muted_foreground).child(format!("Sticker: {}", s.name)).into_any_element(),
        };
    }
    match sticker_url(s) {
        Some(url) => {
            div()
                .id(eid(env, "sticker", &s.id.to_string()))
                .relative()
                .size(u(160.))
                .cursor_pointer()
                .child(spot_layer(&spot))
                .on_click({
                    let subject = crate::emoji_card::Subject::Sticker { id: s.id, name: s.name.clone(), picture: sticker_url(s).unwrap_or_default() };
                    let (here, target) = (env.guild, spot.clone());
                    move |event, _, cx| {
                        cx.stop_propagation();
                        crate::emoji_card::toggle(subject.clone(), here, markdown_view::card_at(&target, event.position()), cx);
                    }
                })
                .child(
                    crate::anim::image(cdn::fitted_url(&url, 160, 160), ObjectFit::Contain, 0.).size(u(160.)),
                )
                .into_any_element()
        }
        None => div().italic().text_color(cx.theme().muted_foreground).child(format!("Sticker: {}", s.name)).into_any_element(),
    }
}

fn is_bare_media_link(message: &Message) -> bool {
    let content = message.content.trim();
    let without_query = |url: &str| url.split(['?', '#']).next().unwrap_or("").to_string();
    !content.is_empty()
        && !content.contains(char::is_whitespace)
        && message.embeds.iter().any(|e| {
            let media = match e.kind.as_deref() {
                Some("gifv") => true,
                Some("image") | Some("video") => e.title.is_none() && e.description.is_none(),
                _ => false,
            };
            media && e.url.as_deref().map(without_query).as_deref() == Some(without_query(content).as_str())
        })
}

/// The whole body of a message: text, stickers, attachments, embeds and
/// components, in the order Discord stacks them.
pub fn body(env: &Env, cx: &App) -> AnyElement {
    let message = env.message;
    let theme = cx.theme();
    // A message that is only a picture or GIF link shows it, not the link.
    let content = if is_bare_media_link(message) { "" } else { message.content.as_str() };
    let mut parsed = env.parse(content);
    // "(edited)" goes after the last line of text, or under the whole body.
    let edited = message.edited_timestamp.is_some();
    let mut edited_below = edited;
    // Inline only after a short last line; a long one has it underneath, as Discord does.
    if edited && !parsed.is_empty() && markdown::last_line_is_short(&parsed) {
        if let Some(marked) = markdown::with_edited_mark(&parsed) {
            parsed = std::rc::Rc::new(marked);
            edited_below = false;
        }
    }
    let v2 = message.flags & FLAG_COMPONENTS_V2 != 0;
    let forwarded = message.forwarded().map(|inner| {
        h_flex()
            .w_full()
            .min_w_0()
            .items_stretch()
            .gap(u(12.))
            .child(div().w(u(4.)).flex_shrink_0().rounded(u(4.)).bg(theme.muted_foreground.opacity(0.5)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(u(2.))
                    .child(div().italic().text_size(u(14.)).text_color(theme.muted_foreground).child("\u{21b1} Forwarded"))
                    .child(body(&Env { shared: env.shared, cache: env.cache, message: &inner, guild: env.guild }, cx)),
            )
    });
    v_flex()
        .w_full()
        .min_w_0()
        .gap(u(4.))
        .when(!parsed.is_empty(), |d| d.child(markdown_view::blocks(env, &parsed, "text", cx)))
        .children(forwarded)
        .when_some(message.poll.as_deref(), |d, p| d.child(poll(env, p, cx)))
        .children(message.sticker_items.iter().map(|s| sticker(env, s, cx)))
        .when(!v2, |d| d.children(attachments(env, &message.attachments, cx)))
        .children(embed_groups(&message.embeds).into_iter().map(|(i, extra)| embed(env, &message.embeds[i], i, &extra, cx)))
        .children(crate::invites::cards(message, &|g| env.cache.guild(g).is_some(), cx))
        .children(message.components.iter().enumerate().map(|(i, c)| component(env, c, &i.to_string(), cx)))
        .when(!message.reactions.is_empty(), |d| d.child(reactions(env, cx)))
        .when(edited_below, |d| {
            let tip = crate::nav::edited_at(message, &chrono::Local).unwrap_or_default();
            d.child(
                div()
                    .id("edited")
                    .text_size(u(env.shared.text_size * 0.625))
                    .text_color(theme.muted_foreground)
                    .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx))
                    .child("(edited)"),
            )
        })
        .when(
            matches!(message.kind, 0 | KIND_REPLY)
                && parsed.is_empty()
                && message.sticker_items.is_empty()
                && message.poll.is_none()
                && message.message_snapshots.is_empty()
                && message.attachments.is_empty()
                && message.embeds.is_empty()
                && message.components.is_empty(),
            |d| d.child(div().italic().text_color(theme.muted_foreground).child("(no content)")),
        )
        .into_any_element()
}

/// The picture of a reaction's emoji: Apple's for unicode, the CDN for custom.
fn reaction_emoji(r: &Reaction) -> AnyElement {
    emoji_picture(&r.emoji, REACTION_EMOJI)
}

fn emoji_picture(emoji: &cord_core::model::ReactionEmoji, size: f32) -> AnyElement {
    let name = emoji.name.clone().unwrap_or_default();
    let url = match emoji.id {
        Some(id) => crate::message_text::custom_emoji_url(id, emoji.animated),
        None => crate::message_text::emoji_url(&name),
    };
    let fallback = crate::message_text::emoji_fallback(&name, emoji.id.is_some());
    // Letters and the like have no picture: their font glyph, centred like one.
    let (glyph, box_size) = (size * 0.8, size);
    if url.contains(".gif") {
        return crate::anim::emoji(url, REACTION_EMOJI);
    }
    img(url)
        .size(u(size))
        .flex_shrink_0()
        .with_fallback(move || {
            div().size(u(box_size)).flex().items_center().justify_center().text_size(u(glyph)).child(fallback.clone()).into_any_element()
        })
        .into_any_element()
}

const POLL_GREEN: u32 = 0x23a55a;

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn votes_label(count: u32) -> String {
    format!("{count} {}", if count == 1 { "vote" } else { "votes" })
}

/// A poll: pick and vote while it's open, bars once you've voted or it's over.
fn poll(env: &Env, poll: &cord_core::poll::Poll, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let message = env.message.id;
    let now = now_ms();
    let closed = poll.is_closed(now);
    let voted = poll.voted();
    let (showing, picks) = {
        let ui = env.shared.poll_ui.borrow();
        (ui.showing.contains(&message), ui.picks.get(&message).cloned().unwrap_or_default())
    };
    let results = closed || voted || showing;
    let green = gpui_kit::Hsla::from(gpui_kit::rgb(POLL_GREEN));
    let multi = poll.allow_multiselect;
    let answers = poll.answers.iter().map(|answer| {
        let id = answer.answer_id;
        let (share, winner, mine, picked) = (poll.percent(id), poll.is_winner(id, now), poll.me_voted(id), picks.contains(&id));
        let edge = if winner {
            green
        } else if mine || picked {
            theme.primary
        } else {
            gpui_kit::transparent_black()
        };
        let fill = if winner {
            green.opacity(0.25)
        } else if mine {
            theme.primary.opacity(0.3)
        } else {
            theme.foreground.opacity(0.1)
        };
        let ui = env.shared.poll_ui.clone();
        h_flex()
            .id(eid(env, "poll", &id.to_string()))
            .relative()
            .w_full()
            .h(u(48.))
            .px(u(12.))
            .gap(u(10.))
            .items_center()
            .rounded(u(8.))
            .overflow_hidden()
            .border_1()
            .border_color(edge)
            .bg(theme.secondary)
            .when(results, |d| {
                // GPUI doesn't clip to rounded corners: the fill rounds itself.
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left_0()
                        .w(gpui_kit::relative(share as f32 / 100.))
                        .rounded_l(u(7.))
                        .when(share >= 100, |d| d.rounded_r(u(7.)))
                        .bg(fill),
                )
            })
            .when(!results, |d| {
                d.cursor_pointer().hover(|s| s.bg(theme.secondary_hover)).on_click(move |_, _, cx| {
                    let mut ui = ui.borrow_mut();
                    let list = ui.picks.entry(message).or_default();
                    match list.iter().position(|a| *a == id) {
                        Some(at) => {
                            list.remove(at);
                        }
                        None if multi => list.push(id),
                        None => *list = vec![id],
                    }
                    drop(ui);
                    cx.refresh_windows();
                })
            })
            .children(answer.poll_media.emoji.as_ref().map(|e| emoji_picture(e, 24.)))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(u(14.))
                    .font_weight(FontWeight::MEDIUM)
                    .child(answer.poll_media.text.clone().unwrap_or_default()),
            )
            .when(results, |d| {
                d.child(div().relative().flex_shrink_0().text_size(u(13.)).font_weight(FontWeight::MEDIUM).child(votes_label(poll.count(id))))
                    .child(div().relative().flex_shrink_0().text_size(u(15.)).font_weight(FontWeight::SEMIBOLD).child(format!("{share}%")))
            })
            .when(!results, |d| {
                d.child(
                    div()
                        .flex_shrink_0()
                        .size(u(20.))
                        .border_2()
                        .border_color(if picked { theme.primary } else { theme.muted_foreground })
                        .when(picked, |d| d.bg(theme.primary))
                        .map(|d| if multi { d.rounded(u(4.)) } else { d.rounded_full() }),
                )
            })
    });
    let link = |key: &str, label: &'static str| {
        div()
            .id(eid(env, "poll", key))
            .text_size(u(14.))
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .hover(|s| s.underline())
            .child(label)
    };
    let actions: Option<AnyElement> = if closed {
        None
    } else if voted {
        let vote = env.shared.vote.clone();
        Some(link("remove", "Remove Vote").on_click(move |_, _, cx| vote(message, Vec::new(), cx)).into_any_element())
    } else if showing {
        let ui = env.shared.poll_ui.clone();
        Some(
            link("back", "Go back to vote")
                .on_click(move |_, _, cx| {
                    ui.borrow_mut().showing.remove(&message);
                    cx.refresh_windows();
                })
                .into_any_element(),
        )
    } else {
        let (ui, ui_vote, vote, chosen) = (env.shared.poll_ui.clone(), env.shared.poll_ui.clone(), env.shared.vote.clone(), picks.clone());
        Some(
            h_flex()
                .gap(u(12.))
                .items_center()
                .child(link("show", "Show results").on_click(move |_, _, cx| {
                    ui.borrow_mut().showing.insert(message);
                    cx.refresh_windows();
                }))
                .child(
                    div()
                        .id(eid(env, "poll", "vote"))
                        .px(u(16.))
                        .py(u(6.))
                        .rounded(u(8.))
                        .text_size(u(14.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(gpui_kit::white())
                        .bg(theme.primary)
                        .when(chosen.is_empty(), |d| d.opacity(0.5))
                        .when(!chosen.is_empty(), |d| {
                            d.cursor_pointer().on_click(move |_, _, cx| {
                                ui_vote.borrow_mut().picks.remove(&message);
                                vote(message, chosen.clone(), cx);
                            })
                        })
                        .child("Vote"),
                )
                .into_any_element(),
        )
    };
    let status = if closed {
        Some("Poll closed".to_string())
    } else {
        poll.time_left(now)
    };
    v_flex()
        .w_full()
        .max_w(u(560.))
        .mt(u(4.))
        .p(u(16.))
        .gap(u(8.))
        .rounded(u(8.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.popover)
        .child(div().text_size(u(16.)).font_weight(FontWeight::SEMIBOLD).child(poll.question.text.clone().unwrap_or_default()))
        .when(!results, |d| {
            d.child(div().text_size(u(13.)).text_color(theme.muted_foreground).child(if multi {
                "Select one or more answers"
            } else {
                "Select one answer"
            }))
        })
        .children(answers)
        .child(
            h_flex()
                .mt(u(4.))
                .gap(u(6.))
                .items_center()
                .flex_wrap()
                .text_size(u(14.))
                .child(div().font_weight(FontWeight::MEDIUM).child(votes_label(poll.total_votes())))
                .children(status.map(|s| div().text_color(theme.muted_foreground).child(format!("\u{2022}  {s}"))))
                .child(div().flex_1())
                .children(actions),
        )
        .into_any_element()
}

/// A message AutoMod stopped, read off its alert's embed.
#[derive(Clone, Debug, PartialEq)]
pub struct AutomodAlert {
    pub text: String,
    pub rule: String,
    pub channel: Option<cord_core::Id>,
    pub keyword: Option<String>,
    /// The words that set it off, to light up in `text`.
    pub matched: Option<String>,
    /// Let through and flagged, rather than blocked.
    pub flagged: bool,
}

pub fn automod_alert(message: &Message) -> Option<AutomodAlert> {
    let embed = message.embeds.iter().find(|e| e.kind.as_deref() == Some("auto_moderation_message"))?;
    let field = |name: &str| embed.fields.iter().find(|f| f.name == name).map(|f| f.value.as_str());
    Some(AutomodAlert {
        text: embed.description.clone().unwrap_or_default(),
        rule: field("rule_name").unwrap_or_default().to_string(),
        channel: field("channel_id").and_then(|id| id.parse().ok()).map(cord_core::Id),
        keyword: field("keyword").map(str::to_string),
        matched: field("keyword_matched_content").map(str::to_string),
        flagged: field("decision_outcome") == Some("flagged"),
    })
}

/// Where `needle` appears in `text`, whatever its case, as byte ranges of `text`.
pub fn find_ignoring_case(text: &str, needle: &str) -> Vec<std::ops::Range<usize>> {
    let wanted: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    if wanted.is_empty() {
        return Vec::new();
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut found = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let mut lower = chars[at..].iter().enumerate().flat_map(|(i, (_, c))| c.to_lowercase().map(move |l| (i, l)));
        let mut last = None;
        let hit = wanted.iter().all(|w| match lower.next() {
            Some((i, l)) if l == *w => {
                last = Some(i);
                true
            }
            _ => false,
        });
        match last.filter(|_| hit) {
            Some(used) => {
                let end = chars.get(at + used + 1).map_or(text.len(), |(byte, _)| *byte);
                found.push(chars[at].0..end);
                at += used + 1;
            }
            None => at += 1,
        }
    }
    found
}

/// Above a bot's answer: who ran which command.
fn command_line(env: &Env, user: &cord_core::model::User, command: &str, indent: f32, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let guild = env.guild.or(env.message.guild_id);
    let styled = env.cache.styled_user(user);
    let colors = guild.map(|g| env.cache.member_colors(g, user.id)).unwrap_or_default();
    let look = crate::badges::name_look(styled, &colors);
    let blurple = gpui_kit::Hsla::from(gpui_kit::rgb(0x5865f2));
    let tag = styled.server_tag().map(|(t, b)| (t.to_string(), b));
    h_flex()
        .h(u(18.))
        .mb(u(4.))
        .items_center()
        .text_size(u(14.))
        .child(
            div()
                .w(u(32.))
                .h(u(10.))
                .mt(u(10.))
                .ml(u(indent))
                .flex_shrink_0()
                .border_l_2()
                .border_t_2()
                .border_color(theme.muted_foreground.opacity(0.5))
                .rounded_tl(u(6.)),
        )
        .child(
            h_flex()
                .min_w_0()
                .ml(u(4.))
                .gap(u(5.))
                .items_center()
                .child(img(env.cache.avatar_url(guild, styled, 32)).size(u(16.)).flex_shrink_0().rounded_full())
                .child({
                    // Their name opens their card, like any other name.
                    let name = env.cache.display_name(guild, user).to_string();
                    let (open, who) = (env.shared.open_user.clone(), user.id);
                    let spot = markdown_view::NameSpot::default();
                    let target = spot.clone();
                    markdown_view::spotted(&spot, div()
                        .id(eid(env, "used-by", ""))
                        .min_w_0()
                        .truncate()
                        .cursor_pointer()
                        .hover(|d| d.underline())
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(look.color.unwrap_or(theme.foreground))
                        .when_some(look.font, |d, font| d.font_family(font))
                        .on_click(move |event, window, cx| {
                            cx.stop_propagation();
                            open(who, markdown_view::card_at(&target, event.position()), window, cx);
                        })
                        .child(crate::badges::name_effect(crate::badges::gradient_text(&name, &look.stops, 0.), &name, &look, false)))
                })
                .children(tag.map(|(t, b)| crate::badges::tag_pill(eid(env, "used-tag", ""), &t, b.as_deref(), cx)))
                .child(div().flex_shrink_0().text_color(theme.muted_foreground).child("used"))
                .child(
                    div()
                        .flex_shrink_0()
                        .px(u(4.))
                        .rounded(u(4.))
                        .bg(blurple.opacity(0.25))
                        .text_color(gpui_kit::Hsla::from(gpui_kit::rgb(0xa9b1ff)))
                        .font_weight(FontWeight::MEDIUM)
                        .child(command.to_string()),
                ),
        )
        .into_any_element()
}

/// What a system message's row shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemKind {
    Added,
    Left,
    Renamed,
    Join,
    Boost,
    Pin,
    Thread,
    FriendAccepted,
    Call,
}

/// How a call's length reads: Discord rounds it, the way people say it.
pub fn call_length(seconds: i64) -> String {
    let minutes = (seconds as f64 / 60.).round() as i64;
    let hours = (seconds as f64 / 3600.).round() as i64;
    match seconds {
        ..45 => "a few seconds".to_string(),
        45..90 => "a minute".to_string(),
        90..2700 => format!("{minutes} minutes"),
        2700..5400 => "an hour".to_string(),
        _ => format!("{hours} hours"),
    }
}

/// How long a finished call lasted; `None` while it's still going.
pub fn call_lasted(message: &Message) -> Option<String> {
    let ended = message.call.as_ref()?.ended_timestamp.as_deref()?;
    let ended = chrono::DateTime::parse_from_rfc3339(ended).ok()?.timestamp_millis();
    Some(call_length((ended - message.id.timestamp_ms() as i64) / 1000))
}

/// A system message, as what comes before and after the person's name.
#[derive(Clone, Debug, PartialEq)]
pub struct SystemLine {
    pub kind: SystemKind,
    pub before: &'static str,
    pub after: String,
}

/// Discord's welcomes, picked by the message's timestamp, so every client shows the same one.
const WELCOMES: [(&str, &str); 13] = [
    ("", " joined the party."),
    ("", " is here."),
    ("Welcome, ", ". We hope you brought pizza."),
    ("A wild ", " appeared."),
    ("", " just landed."),
    ("", " just slid into the server."),
    ("", " just showed up!"),
    ("Welcome ", ". Say hi!"),
    ("", " hopped into the server."),
    ("Everyone welcome ", "!"),
    ("Glad you're here, ", "."),
    ("Good to see you, ", "."),
    ("Yay you made it, ", "!"),
];

pub fn system_line(message: &Message) -> Option<SystemLine> {
    let boost = |after: String| Some(SystemLine { kind: SystemKind::Boost, before: "", after });
    match message.kind {
        // Group chats. One person removed by another reads differently from leaving.
        1 => {
            let added = message.mentions.first().map_or_else(|| "someone".to_string(), |u| u.display_name().to_string());
            Some(SystemLine { kind: SystemKind::Added, before: "", after: format!(" added {added} to the group.") })
        }
        2 => match message.mentions.first().filter(|u| u.id != message.author.id) {
            Some(removed) => Some(SystemLine { kind: SystemKind::Left, before: "", after: format!(" removed {} from the group.", removed.display_name()) }),
            None => Some(SystemLine { kind: SystemKind::Left, before: "", after: " left the group.".into() }),
        },
        3 => Some(SystemLine {
            kind: SystemKind::Call,
            before: "",
            after: call_lasted(message).map_or_else(|| " started a call.".to_string(), |lasted| format!(" started a call that lasted {lasted}.")),
        }),
        4 => Some(SystemLine { kind: SystemKind::Renamed, before: "", after: format!(" changed the group name: {}.", message.content) }),
        5 => Some(SystemLine { kind: SystemKind::Renamed, before: "", after: " changed the group icon.".into() }),
        6 => Some(SystemLine { kind: SystemKind::Pin, before: "", after: " pinned a message to this channel.".into() }),
        7 => {
            let (before, after) = WELCOMES[(message.id.timestamp_ms() % WELCOMES.len() as u64) as usize];
            Some(SystemLine { kind: SystemKind::Join, before, after: after.into() })
        }
        8 => boost(" just boosted the server!".into()),
        // Who's who depends on who's looking: the row works that out.
        67 => Some(SystemLine { kind: SystemKind::FriendAccepted, before: "", after: " accepted your friend request.".into() }),
        18 => Some(SystemLine { kind: SystemKind::Thread, before: "", after: format!(" started a thread: {}.", message.content) }),
        9..=11 => boost(format!(" just boosted the server! The server has reached Level {}!", message.kind - 8)),
        _ => None,
    }
}

/// Discord's message type for "this poll has closed".
pub const KIND_POLL_RESULT: u8 = 46;

/// What a closed poll's notice says, read off its `poll_result` embed.
#[derive(Clone, Debug, PartialEq)]
pub struct PollOutcome {
    pub question: String,
    /// The winning answer; none when it was tied or nobody voted.
    pub winner: Option<(String, Option<cord_core::model::ReactionEmoji>)>,
    pub votes: u32,
    pub total: u32,
}

pub fn poll_outcome(message: &Message) -> Option<PollOutcome> {
    let embed = message.embeds.iter().find(|e| e.kind.as_deref() == Some("poll_result"))?;
    let field = |name: &str| embed.fields.iter().find(|f| f.name == name).map(|f| f.value.as_str());
    let number = |name: &str| field(name).and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
    let emoji = field("victor_answer_emoji_name").map(|name| cord_core::model::ReactionEmoji {
        id: field("victor_answer_emoji_id").and_then(|id| id.parse().ok()).map(cord_core::Id),
        name: Some(name.to_string()),
        animated: field("victor_answer_emoji_animated") == Some("true"),
    });
    Some(PollOutcome {
        question: field("poll_question_text").unwrap_or_default().to_string(),
        winner: field("victor_answer_text").map(|text| (text.to_string(), emoji)),
        votes: number("victor_answer_votes"),
        total: number("total_votes"),
    })
}

/// The card under "…'s poll has closed": who won, and the way back to the poll.
pub fn poll_outcome_card(env: &Env, outcome: &PollOutcome, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let target = env.message.message_reference.as_ref().and_then(|r| r.message_id);
    let jump = env.shared.jump.clone();
    let (title, detail) = match &outcome.winner {
        Some((text, _)) => {
            let share = (f64::from(outcome.votes) * 100. / f64::from(outcome.total.max(1))).round() as u32;
            (text.clone(), format!("Winning answer \u{2022} {share}%"))
        }
        None if outcome.total == 0 => ("There were no votes".to_string(), String::new()),
        None => ("The results were tied".to_string(), String::new()),
    };
    h_flex()
        .w_full()
        .max_w(u(480.))
        .mt(u(4.))
        .p(u(10.))
        .gap(u(12.))
        .items_center()
        .rounded(u(8.))
        .border_1()
        .border_color(theme.border)
        .children(outcome.winner.as_ref().and_then(|(_, e)| e.as_ref()).map(|e| {
            div().size(u(48.)).flex_shrink_0().flex().items_center().justify_center().rounded(u(8.)).bg(theme.secondary).child(emoji_picture(e, 28.))
        }))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    h_flex()
                        .gap(u(6.))
                        .items_center()
                        .child(div().min_w_0().truncate().text_size(u(15.)).font_weight(FontWeight::SEMIBOLD).child(title))
                        .when(outcome.winner.is_some(), |d| {
                            d.child(
                                div()
                                    .size(u(16.))
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .bg(gpui_kit::Hsla::from(gpui_kit::rgb(POLL_GREEN)))
                                    .text_color(gpui_kit::white())
                                    .child(gpui_kit::component::Icon::new(gpui_kit::assets::IconName::Check).size(u(11.))),
                            )
                        }),
                )
                .when(!detail.is_empty(), |d| d.child(div().text_size(u(13.)).text_color(theme.muted_foreground).child(detail))),
        )
        .children(target.map(|id| {
            div()
                .id(eid(env, "poll", "view"))
                .flex_shrink_0()
                .px(u(14.))
                .py(u(7.))
                .rounded(u(8.))
                .text_size(u(14.))
                .font_weight(FontWeight::MEDIUM)
                .bg(theme.secondary)
                .cursor_pointer()
                .hover(|s| s.bg(theme.secondary_hover))
                .on_click(move |_, _, cx| jump(id, cx))
                .child("View Poll")
        }))
        .into_any_element()
}

/// The row of reaction pills under a message. Clicking one toggles the
/// viewer's own reaction.
fn reactions(env: &Env, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let message = env.message.id;
    h_flex()
        .flex_wrap()
        .gap(u(4.))
        .mt(u(4.))
        .children(env.message.reactions.iter().enumerate().map(|(i, r)| {
            let (react, emoji, add) = (env.shared.react.clone(), r.emoji.clone(), !r.me);
            let count = r.count as u32;
            let (who, hovered, tip_emoji) = (env.shared.who_reacted.clone(), r.emoji.clone(), r.emoji.clone());
            h_flex()
                .id(eid(env, "reaction", &i.to_string()))
                .on_hover(move |over, _, cx| who(over.then(|| (message, hovered.clone(), count)), cx))
                .tooltip(move |window, cx| {
                    // The emoji as a picture, as everywhere else, not the font's.
                    let (text, emoji) = (crate::reaction_names::tooltip(message, &tip_emoji, count, cx), tip_emoji.clone());
                    gpui_kit::component::tooltip::Tooltip::element(move |_, _| {
                        h_flex().gap(u(4.)).items_center().child(text.clone()).child(emoji_picture(&emoji, 18.))
                    })
                    .build(window, cx)
                })
                .h(u(REACTION_H))
                .px(u(6.))
                .gap(u(6.))
                .items_center()
                .rounded(u(8.))
                .border_1()
                .border_color(if r.me { theme.primary } else { gpui_kit::transparent_black() })
                .bg(if r.me { theme.primary.opacity(0.15) } else { theme.secondary })
                .cursor_pointer()
                .hover(|s| s.bg(if r.me { theme.primary.opacity(0.25) } else { theme.secondary_hover }))
                .on_click(move |_, _, cx| react(message, emoji.clone(), add, cx))
                .child(reaction_emoji(r))
                .child(
                    div()
                        .text_size(u(14.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(if r.me { theme.foreground } else { theme.muted_foreground })
                        .child(r.count.to_string()),
                )
        }))
        // Discord's "Add Reaction" pill after the others (the picker comes later).
        .child(
            div()
                .id(eid(env, "reaction", "add"))
                .h(u(REACTION_H))
                .w(u(34.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(u(8.))
                .bg(theme.secondary)
                .text_color(theme.muted_foreground)
                .cursor_pointer()
                .hover(|s| s.bg(theme.secondary_hover).text_color(theme.foreground))
                .child(gpui_kit::component::Icon::new(gpui_kit::assets::IconName::FaceSlightlySmiling).size(u(18.))),
        )
        .into_any_element()
}

/// An embed always has a bar: its colour, or Discord's gray.
fn embed_color(embed: &Embed, _cx: &App) -> Hsla {
    gpui_kit::rgb(embed.color.unwrap_or(EMBED_GRAY)).into()
}

const EMBED_GRAY: u32 = 0x4e5058;

/// Embeds and component containers set their text at 14px where the chat
/// uses 16 (and everything sized from it shrinks alike).
const CARD_TEXT: f32 = 14. / 16.;
/// A card's colour bar and its padding, across.
const CARD_PADDING: f32 = 32.;

/// Embeds as Discord shows them: ones that share a link and add only a
/// picture (a post with several images) fold into the first, whose image
/// becomes a mosaic. Each kept embed's index and the pictures it takes in.
fn embed_groups(embeds: &[Embed]) -> Vec<(usize, Vec<&EmbedMedia>)> {
    let mut groups: Vec<(usize, Vec<&EmbedMedia>)> = Vec::new();
    for (i, e) in embeds.iter().enumerate() {
        let picture_only = e.title.is_none() && e.description.is_none() && e.fields.is_empty() && e.author.is_none();
        let host = e.url.as_ref().filter(|_| picture_only).and_then(|url| {
            groups.iter_mut().find(|(g, _)| embeds[*g].url.as_ref() == Some(url) && embeds[*g].image.is_some())
        });
        match (host, e.image.as_ref()) {
            (Some((_, extra)), Some(image)) => extra.push(image),
            _ => groups.push((i, Vec::new())),
        }
    }
    groups
}

/// How long ago something was, as Discord says it.
fn time_ago(seconds: i64) -> String {
    let plural = |n: i64, unit: &str| format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" });
    match seconds {
        ..60 => "just now".to_string(),
        60..3600 => plural(seconds / 60, "minute"),
        3600..86400 => plural(seconds / 3600, "hour"),
        86400..2_592_000 => plural(seconds / 86400, "day"),
        _ => plural(seconds / 86400, "day"),
    }
}

/// Discord's notices about your own account come as an embed of named fields, meant for
/// its client to lay out: its heading, when, what it says, and a link.
fn discord_notice(env: &Env, e: &Embed, cx: &App) -> Option<AnyElement> {
    let field = |name: &str| e.fields.iter().find(|f| f.name == name).map(|f| f.value.as_str());
    // `client_version_message` is what Discord leaves for a client that can't show one: it marks
    // these, and is the only words some have. The rest of those fields are for its own client.
    if field("header").is_none() && field("client_version_message").is_none() {
        return None;
    }
    let header = field("header").or(e.title.as_deref())?;
    let body = field("body").or(field("client_version_message"))?;
    let theme = cx.theme();
    let link_blue = gpui_kit::Hsla::from(gpui_kit::rgb(0x00a8fc));
    let when = field("timestamp").or(field("incident_time")).and_then(|t| t.parse::<f64>().ok()).map(|at| time_ago(chrono::Utc::now().timestamp() - at as i64));
    let link = field("learn_more_link").map(str::to_string);
    Some(
        v_flex()
            .w_full()
            .max_w(u(720.))
            .mt(u(2.))
            .rounded(u(8.))
            .overflow_hidden()
            .bg(theme.secondary)
            .child(
                v_flex()
                    .p(u(12.))
                    .gap(u(4.))
                    .child(
                        h_flex()
                            .gap(u(6.))
                            .items_center()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(gpui_kit::component::Icon::new(crate::sidebar::channel_icon(cord_core::model::ChannelKind::Announcement)).size(u(18.)).text_color(link_blue).flex_shrink_0())
                            .child(header.to_string()),
                    )
                    .when_some(when, |d, when| d.child(div().text_size(u(12.)).text_color(theme.muted_foreground).child(when)))
                    .child(div().pt(u(4.)).text_color(theme.muted_foreground).child(body.to_string())),
            )
            .when_some(link, |d, url| {
                d.child(
                    div()
                        .id(eid(env, "notice-link", ""))
                        .px(u(12.))
                        .py(u(8.))
                        .bg(theme.foreground.opacity(0.06))
                        .text_color(link_blue)
                        .cursor_pointer()
                        .hover(|d| d.underline())
                        .on_click(move |_, _, cx| cx.open_url(&url))
                        .child("Learn More"),
                )
            })
            .into_any_element(),
    )
}

fn embed(env: &Env, e: &Embed, index: usize, extra: &[&EmbedMedia], cx: &App) -> AnyElement {
    let small = Shared { text_size: env.shared.text_size * CARD_TEXT, in_card: true, ..env.shared.clone() };
    let env = &Env { shared: &small, cache: env.cache, message: env.message, guild: env.guild };
    let theme = cx.theme();
    let key = index.to_string();
    if let Some(card) = discord_notice(env, e, cx) {
        return card;
    }
    let kind = e.kind.as_deref().unwrap_or("rich");
    let open = e.url.clone().or_else(|| e.video.as_ref().map(|v| v.url.clone()));

    // A bare picture, and video or GIF links: just the picture.
    // Discord drops a gifv's title and provider (Tenor, Klipy…) and shows only the GIF.
    if kind == "gifv" || (matches!(kind, "image" | "video") && e.title.is_none() && e.description.is_none()) {
        let shown = e.thumbnail.as_ref().or(e.image.as_ref());
        if let Some(media) = shown {
            let target = open.clone().unwrap_or_else(|| media.url.clone());
            let pic = picture(eid(env, "embed-pic", &key), media.display_url(), &target, media.dimensions(), IMAGE_MAX, cx);
            if let Some(video) = e.video.as_ref().filter(|_| kind == "gifv") {
                crate::viewer::link_video(media.display_url(), video.display_url());
            }
            // A GIF plays over its still frame once the chat list has warmed a player for it.
            let playing = (kind == "gifv")
                .then(|| e.video.as_ref())
                .flatten()
                .and_then(|v| crate::gifs::player(v.display_url(), cx));
            return match playing {
                Some(player) => h_flex()
                    .child(div().relative().child(pic)
                    .child(
                        alhazen_gpui::video_view(player)
                            .corner_radius(u(8.))
                            .object_fit(ObjectFit::Cover)
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full(),
                    ))
                    .into_any_element(),
                None if kind == "image" => pic,
                None => with_play_badge(pic, cx),
            };
        }
    }

    let thumb_beside = thumb_goes_beside(kind, e.image.is_some(), e.thumbnail.as_ref().and_then(|t| t.dimensions()));
    let side_thumb = e.thumbnail.as_ref().filter(|_| thumb_beside && e.video.is_none());
    let big_image = e.image.as_ref().or_else(|| e.thumbnail.as_ref().filter(|_| side_thumb.is_none()));

    let size = env.shared.text_size;
    // Sized by its content (not `flex_1`), so a card that shrinks to fit
    // doesn't squeeze its text to one letter per line.
    let mut text_column = v_flex().min_w_0().flex_auto().gap(u(8.)).text_size(u(size)).line_height(gpui_kit::relative(1.3));
    if let Some(provider) = e.provider.as_ref().and_then(|p| p.name.as_deref()).filter(|_| kind != "rich") {
        text_column = text_column.child(div().text_xs().text_color(theme.muted_foreground).child(provider.to_string()));
    }
    if let Some(author) = &e.author {
        // Only Discord's proxy: the original host would see the viewer's address.
        let icon = author.proxy_icon_url.as_deref();
        text_column = text_column.child(
            h_flex()
                .gap_2()
                .items_center()
                .when_some(icon, |d, url| d.child(img(cdn::fitted_url(url, 24, 24)).size(u(24.)).rounded_full().flex_shrink_0()))
                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(author.name.clone())),
        );
    }
    if let Some(title) = e.title.as_deref().filter(|t| !t.is_empty()) {
        // Titles have markdown and emoji too.
        let parsed_title = env.parse(title);
        let title_el = div().font_weight(FontWeight::SEMIBOLD).text_size(u(size / CARD_TEXT)).child(markdown_view::blocks(env, &parsed_title, &format!("e{key}t"), cx));
        text_column = text_column.child(match e.url.clone().filter(|u| is_http(u)) {
            Some(url) => title_el
                .id(eid(env, "embed-title", &key))
                .text_color(theme.link)
                .cursor_pointer()
                .hover(|s| s.underline())
                .on_click(move |_, _, cx| crate::links::open(&url, cx))
                .into_any_element(),
            None => title_el.into_any_element(),
        });
    }
    if let Some(description) = e.description.as_deref().filter(|d| !d.is_empty()) {
        let blocks = env.parse(description);
        text_column = text_column.child(div().child(markdown_view::blocks(env, &blocks, &format!("e{key}d"), cx)));
    }
    for (r, row) in field_rows(&e.fields).into_iter().enumerate() {
        text_column = text_column.child(
            h_flex().w_full().gap_3().items_start().children(row.into_iter().map(|f| {
                let field = &e.fields[f];
                let value = env.parse(&field.value);
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(u(2.))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(field.name.clone()))
                    .child(div().child(markdown_view::blocks(env, &value, &format!("e{key}f{r}.{f}"), cx)))
            })),
        );
    }
    if let (Some(first), false) = (big_image, extra.is_empty()) {
        let tiles: Vec<Tile> = std::iter::once(first)
            .chain(extra.iter().copied())
            .enumerate()
            .map(|(i, m)| Tile { key: format!("{key}.{i}"), src: m.display_url(), open: &m.url, real: m.dimensions() })
            .collect();
        text_column = text_column.child(div().mt(u(4.)).child(tile_grid(env, "embed-tile", &tiles, EMBED_GRID_W, cx)));
    } else if let Some(media) = big_image {
        let target = media.url.clone();
        let pic = picture(eid(env, "embed-img", &key), media.display_url(), &target, media.dimensions(), IMAGE_MAX, cx);
        text_column = text_column.child(div().mt(u(4.)).child(if e.video.is_some() { with_play_badge(pic, cx) } else { pic }));
    }
    if let Some(line) = embed_footer(e, cx) {
        text_column = text_column.child(line);
    }

    // Discord's `max-width: max-content`: as wide as its content wants, up to
    // the limit, and narrower when the chat is.
    // Beside a thumbnail, shrink-to-fit can squeeze the text down to a letter per line:
    // those get the width their text calls for.
    let thumb_width = side_thumb.map(|_| {
        let longest = |text: &str, size: f32| text.lines().map(|l| l.chars().count()).max().unwrap_or(0) as f32 * size * 0.56;
        let title = e.title.as_deref().map_or(0., |t| longest(t, size / CARD_TEXT));
        let description = e.description.as_deref().map_or(0., |t| longest(t, size));
        let fields = if e.fields.is_empty() { 0. } else { CARD_MAX_W };
        (title.max(description).max(fields).max(160.) + 80. + 48.).min(CARD_MAX_W)
    });
    // Under a big picture, as wide as the picture and its padding: text wraps to it.
    let picture_width = big_image.filter(|_| extra.is_empty()).and_then(|media| media.dimensions()).map(|(w, h)| fit_image(w, h, IMAGE_MAX.0, IMAGE_MAX.1).0 + CARD_PADDING);
    h_flex().w_full().child(h_flex()
        .max_w(u(CARD_MAX_W))
        .min_w_0()
        .when_some(thumb_width.or(picture_width), |d, width| d.w(u(width)))
        // The colour bar is the card's own background showing down the left:
        // a 4px-wide element can't take the card's 8px corners itself.
        .rounded(u(8.))
        .bg(embed_color(e, cx))
        .pl(u(4.))
        .items_stretch()
        .child(
            h_flex()
                .flex_auto()
                .min_w_0()
                .rounded_r(u(8.))
                .border_1()
                .border_l_0()
                .border_color(theme.border)
                .bg(card_surface(theme))
                .gap_4()
                .items_start()
                .pt(u(8.))
                .pr(u(16.))
                .pb(u(16.))
                .pl(u(12.))
                .child(text_column)
                .when_some(side_thumb, |d, media| d.child(thumbnail(env, media, &key, cx))),
        ))
        .into_any_element()
}

/// Link previews with a proper picture show it big under the text; a little one, like
/// a site's icon, sits to the side. Bots' embeds say what they mean: always the side.
fn thumb_goes_beside(kind: &str, has_image: bool, size: Option<(u32, u32)>) -> bool {
    const BIG_PREVIEW: u32 = 400;
    if has_image || kind == "rich" {
        return true;
    }
    (kind == "link" || kind == "article") && size.is_none_or(|(w, _)| w < BIG_PREVIEW)
}

/// A card's face, opaque: the colour bar sits behind it.
fn card_surface(theme: &gpui_kit::component::Theme) -> Hsla {
    theme.background.blend(theme.secondary)
}

fn thumbnail(env: &Env, media: &EmbedMedia, key: &str, cx: &App) -> AnyElement {
    picture(eid(env, "embed-thumb", key), media.display_url(), &media.url, media.dimensions(), (80., 80.), cx)
}

/// A small play glyph over a video or GIF thumbnail.
fn with_play_badge(pic: AnyElement, cx: &App) -> AnyElement {
    div()
        .relative()
        .child(pic)
        .child(
            div()
                .absolute()
                .bottom(u(8.))
                .left(u(8.))
                .px_2()
                .rounded(u(4.))
                .bg(cx.theme().background.opacity(0.75))
                .text_xs()
                .child("\u{25b6}"),
        )
        .into_any_element()
}

fn embed_footer(e: &Embed, cx: &App) -> Option<AnyElement> {
    let text = e.footer.as_ref().map(|f| f.text.as_str()).filter(|t| !t.is_empty());
    let when = e
        .timestamp
        .as_deref()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| nav::format_message_time(t.timestamp_millis().max(0) as u64, &chrono::Local::now()));
    if text.is_none() && when.is_none() {
        return None;
    }
    let icon = e.footer.as_ref().and_then(|f| f.proxy_icon_url.as_deref());
    let line = [text.map(str::to_string), when].into_iter().flatten().collect::<Vec<_>>().join(" \u{2022} ");
    Some(
        h_flex()
            .gap_2()
            .items_center()
            .mt(u(4.))
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .when_some(icon, |d, url| d.child(img(cdn::fitted_url(url, 20, 20)).size(u(20.)).rounded_full().flex_shrink_0()))
            .child(line)
            .into_any_element(),
    )
}

/// One Components V2 component (or action row).
fn component(env: &Env, c: &Component, key: &str, cx: &App) -> AnyElement {
    let theme = cx.theme();
    match c {
        Component::ActionRow(items) => h_flex()
            .flex_wrap()
            .gap(u(8.))
            .items_center()
            .children(items.iter().enumerate().map(|(i, c)| component(env, c, &format!("{key}.{i}"), cx)))
            .into_any_element(),
        Component::Button(b) => button(env, b, key, cx),
        Component::Select(s) => select(s, cx),
        Component::Section { components, accessory } => h_flex()
            .w_full()
            .gap_3()
            // Text beside a button sits level with it; beside a picture it starts at the top.
            .map(|d| if matches!(accessory.as_deref(), Some(Component::Button { .. })) { d.items_center() } else { d.items_start() })
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(u(2.))
                    .children(components.iter().enumerate().map(|(i, c)| component(env, c, &format!("{key}.{i}"), cx))),
            )
            .when_some(accessory.as_deref(), |d, a| d.child(div().flex_shrink_0().child(component(env, a, &format!("{key}.a"), cx))))
            .into_any_element(),
        Component::TextDisplay(content) => {
            let blocks = env.parse(content);
            markdown_view::blocks(env, &blocks, &format!("c{key}"), cx)
        }
        Component::Thumbnail { media, .. } => {
            let (url, dims) = resolve_media(env.message, media);
            picture(eid(env, "thumb", key), url, url, dims, (80., 80.), cx)
        }
        Component::MediaGallery(items) => gallery(env, items, key, cx),
        Component::File { file, name, size } => {
            let (url, _) = resolve_media(env.message, file);
            let fallback = file.url.strip_prefix("attachment://").unwrap_or("file");
            let name = name.as_deref().unwrap_or(fallback);
            let size = size.or_else(|| env.message.attachments.iter().find(|a| a.filename == name).map(|a| a.size));
            file_card(eid(env, "cfile", key), name, size, url, cx)
        }
        Component::Separator { divider, spacing } => {
            let gap = if *spacing >= 2 { 16. } else { 8. };
            div().w_full().py(u(gap / 2.)).when(*divider, |d| d.child(div().h(u(1.)).w_full().bg(theme.border))).into_any_element()
        }
        Component::Container { components, accent_color } => {
            let small = Shared { text_size: env.shared.text_size * CARD_TEXT, in_card: true, ..env.shared.clone() };
            let env = &Env { shared: &small, cache: env.cache, message: env.message, guild: env.guild };
            let accent = accent_color.map(|c| Hsla::from(gpui_kit::rgb(c)));
            h_flex().w_full().child(h_flex()
                .max_w(u(CONTAINER_MAX_W))
                .min_w_0()
                // An accent colour shows as the card's background down the left,
                // so it follows the corners; see `embed`.
                .rounded(u(8.))
                .items_stretch()
                .when_some(accent, |d, color| d.bg(color).pl(u(4.)))
                .child(
                    v_flex()
                        .flex_auto()
                        .min_w_0()
                        .border_1()
                        .border_color(theme.border)
                        .bg(card_surface(theme))
                        .map(|d| if accent.is_some() { d.border_l_0().rounded_r(u(8.)) } else { d.rounded(u(8.)) })
                        .gap(u(8.))
                        .p(u(16.))
                        .text_size(u(env.shared.text_size))
                        .line_height(gpui_kit::relative(1.3))
                        .children(components.iter().enumerate().map(|(i, c)| component(env, c, &format!("{key}.{i}"), cx))),
                ))
                .into_any_element()
        }
        Component::Unknown(_) => div().into_any_element(),
    }
}

/// The most a gallery's lone picture may fill.
const GALLERY_ONE: (f32, f32) = (420., 420.);

fn gallery(env: &Env, items: &[GalleryItem], key: &str, cx: &App) -> AnyElement {
    let theme = cx.theme();
    // One picture fills the container, as in Discord; several share a grid.
    let max = if items.len() == 1 { GALLERY_ONE } else { (200., 200.) };
    h_flex()
        .flex_wrap()
        .gap(u(4.))
        .children(items.iter().enumerate().map(|(i, item)| {
            let (url, dims) = resolve_media(env.message, &item.media);
            let id = eid(env, "gallery", &format!("{key}.{i}"));
            if item.spoiler {
                let url = url.to_string();
                return div()
                    .id(id)
                    .px_4()
                    .py_8()
                    .rounded(u(6.))
                    .bg(theme.muted_foreground.opacity(0.4))
                    .text_xs()
                    .font_weight(FontWeight::BOLD)
                    .cursor_pointer()
                    .when(is_http(&url), |d| d.on_click(move |_, _, cx| crate::links::open(&url, cx)))
                    .child("SPOILER")
                    .into_any_element();
            }
            picture(id, url, url, dims, max, cx)
        }))
        .into_any_element()
}

fn emoji(e: &ComponentEmoji, size: f32) -> Option<AnyElement> {
    match (&e.id, &e.name) {
        (Some(id), name) => {
            let url = crate::message_text::custom_emoji_url(id, e.animated);
            let name = name.clone().unwrap_or_default();
            Some(
                img(url)
                    .size(u(size))
                    .with_fallback(move || div().child(crate::message_text::emoji_fallback(&name, true)).into_any_element())
                    .into_any_element(),
            )
        }
        (None, Some(name)) => Some(crate::badges::emoji_image(name, size).into_any_element()),
        (None, None) => None,
    }
}

/// Discord's component button colours and height.
const BUTTON_SUCCESS: u32 = 0x248046;
const BUTTON_DANGER: u32 = 0xda373c;
const BUTTON_H: f32 = 32.;

fn button(env: &Env, b: &Button, key: &str, cx: &App) -> AnyElement {
    let theme = cx.theme();
    // 1 primary, 2 secondary, 3 success, 4 danger, 5 link, 6 premium.
    let (bg, fg) = match b.style {
        1 | 6 => (theme.primary, theme.primary_foreground),
        3 => (Hsla::from(gpui_kit::rgb(BUTTON_SUCCESS)), theme.success_foreground),
        4 => (Hsla::from(gpui_kit::rgb(BUTTON_DANGER)), theme.danger_foreground),
        // Discord's secondary buttons: a faint fill with a hairline, not a solid block.
        _ => (theme.foreground.opacity(0.06), theme.secondary_foreground),
    };
    let link = b.url.clone().filter(|u| is_http(u));
    let outlined = b.style == 5;
    let hairline = outlined || !matches!(b.style, 1 | 3 | 4 | 6);
    let only_icon = b.label.as_deref().is_none_or(str::is_empty);
    h_flex()
        .id(eid(env, "button", key))
        .when(hairline, |d| d.border_1().border_color(theme.foreground.opacity(0.12)))
        // Discord's minimum width: icon buttons come out as wide, even keys.
        .min_w(u(if only_icon { 60. } else { 0. }))
        .gap(u(6.))
        .items_center()
        .h(u(BUTTON_H))
        .px(u(14.))
        .justify_center()
        .rounded(u(8.))
        .when(!outlined, |d| d.bg(bg))
        .text_color(fg)
        .text_size(u(14.))
        .line_height(gpui_kit::relative(1.))
        .font_weight(FontWeight::MEDIUM)
        .when(b.disabled, |d| d.opacity(0.5))
        .when_some(link.filter(|_| !b.disabled), |d, url| d.cursor_pointer().on_click(move |_, _, cx| crate::links::open(&url, cx)))
        .children(b.emoji.as_ref().and_then(|e| emoji(e, 16.)))
        .children(b.label.clone().filter(|l| !l.is_empty()).map(|l| div().child(l)))
        .when(outlined, |d| d.child(gpui_kit::component::Icon::new(gpui_kit::assets::IconName::ArrowUpRight).size(u(16.))))
        .into_any_element()
}

fn select(s: &Select, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let default = match s.kind {
        5 => "Select users",
        6 => "Select roles",
        7 => "Select users or roles",
        8 => "Select channels",
        _ => "Make a selection",
    };
    let placeholder = s.placeholder.clone().filter(|p| !p.is_empty()).unwrap_or_else(|| default.to_string());
    h_flex()
        .w(u(300.))
        .max_w_full()
        .justify_between()
        .items_center()
        .px_3()
        .py(u(8.))
        .rounded(u(4.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.input.opacity(0.4))
        .text_sm()
        .text_color(theme.muted_foreground)
        .when(s.disabled, |d| d.opacity(0.5))
        .child(div().truncate().child(placeholder))
        .child(div().text_xs().child("\u{25be}"))
        .into_any_element()
}

/// What a reply points at, from the message itself or else the loaded history.
fn referenced<'a>(message: &'a Message, cache: &'a Cache) -> Option<&'a Message> {
    // The live copy first: the one sent along with the reply never hears of edits.
    let live = message.message_reference.as_ref().and_then(|reference| {
        let (id, channel) = (reference.message_id?, reference.channel_id.unwrap_or(message.channel_id));
        cache.messages(channel)?.iter().map(|m| &m.message).find(|m| m.id == id)
    });
    live.or(message.referenced_message.as_deref())
}

/// The line above a reply: a curved connector, the original author's small
/// avatar and name, and the start of what they said. `indent` is how far in
/// the connector starts (the middle of the author's avatar).
pub fn reply_line(env: &Env, indent: f32, cx: &App) -> Option<AnyElement> {
    let message = env.message;
    if !is_reply(message) {
        return None;
    }
    let theme = cx.theme();
    if let Some((user, command)) = message.command_use() {
        return Some(command_line(env, user, command, indent, cx));
    }
    let target = referenced(message, env.cache);
    let jump = target.map(|m| (m.id, env.shared.jump.clone()));

    let connector = div()
        .w(u(32.))
        .h(u(10.))
        .mt(u(10.))
        .ml(u(indent))
        .flex_shrink_0()
        .border_l_2()
        .border_t_2()
        .border_color(theme.muted_foreground.opacity(0.5))
        .rounded_tl(u(6.));

    let content: AnyElement = match target {
        Some(m) => {
            let author = &m.author;
            // Fetched messages carry no guild_id: the page's is the one to trust.
            let guild = env.guild.or(m.guild_id).or(message.guild_id);
            let name = env.cache.display_name(guild, author).to_string();
            let styled = env.cache.styled_user(author);
            let colors = guild.map(|g| env.cache.member_colors(g, author.id)).unwrap_or_default();
            let look = crate::badges::name_look(styled, &colors);
            let tag = styled.server_tag().map(|(t, b)| (t.to_string(), b));
            let overflow = std::rc::Rc::new(std::cell::Cell::new(false));
            // A message that's only a picture link reads as an attachment, not as the link.
            // All of it on one line: what doesn't fit fades out at the edge of the row.
            let flattened: String = if is_bare_media_link(m) {
                String::new()
            } else {
                m.content.lines().map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>().join(" ")
            };
            let first_line = flattened.as_str();
            let preview = if first_line.is_empty() {
                let media = !m.attachments.is_empty() || !m.embeds.is_empty() || !m.sticker_items.is_empty();
                h_flex()
                    .gap(u(4.))
                    .italic()
                    .child(if media { "Click to see attachment" } else { "Click to see message" })
                    .when(media, |d| d.child(gpui_kit::component::Icon::new(gpui_kit::assets::IconName::Image).size(u(16.)).flex_shrink_0()))
                    .into_any_element()
            } else {
                // One line of fixed height: its emoji shrink to fit it, rather than it growing.
                let preview_shared = Shared { inert: true, emoji_cap: Some(18.), ..env.shared.clone() };
                let inner = Env { shared: &preview_shared, cache: env.cache, message: m, guild: env.guild };
                markdown_view::runs_line(&inner, "reply", &markdown::parse_inline(markdown::preview_line(first_line)), cx)
            };
            h_flex()
                .gap(u(6.))
                .items_center()
                .min_w_0()
                .child(
                    // Their server's picture if they have one, as on the message itself.
                    img(env.cache.avatar_url(guild, styled, 32))
                        .size(u(16.))
                        .rounded_full()
                        .flex_shrink_0(),
                )
                .child({
                    // Their name is its own target: it opens their card, not the message.
                    let (open, who) = (env.shared.open_user.clone(), author.id);
                    let spot = markdown_view::NameSpot::default();
                    let target = spot.clone();
                    markdown_view::spotted(&spot, div()
                        .id(eid(env, "reply-name", ""))
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .cursor_pointer()
                        .hover(|d| d.underline())
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(look.color.unwrap_or(theme.foreground))
                        .when_some(look.font, |d, f| d.font_family(f))
                        .on_mouse_down(gpui_kit::MouseButton::Right, {
                            let menu = env.shared.open_user_menu.clone();
                            move |event, _, cx| {
                                cx.stop_propagation();
                                menu(who, event.position, cx);
                            }
                        })
                        .on_click(move |event, window, cx| {
                            cx.stop_propagation();
                            open(who, markdown_view::card_at(&target, event.position()), window, cx);
                        })
                        .child({
                            // The @ is for replies that pinged them, as in Discord.
                            let pinged = message.mentions.iter().any(|u| u.id == author.id);
                            crate::badges::gradient_text(&if pinged { format!("@{name}") } else { name.clone() }, &look.stops, 0.)
                        }))
                })
                .when_some(tag, |d, (tag, badge)| d.child(crate::badges::tag_pill(eid(env, "reply-tag", "t"), &tag, badge.as_deref(), cx)))
                // Always one line: what doesn't fit fades out at the edge of the row.
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_w_0()
                        .h(u(20.))
                        .overflow_hidden()
                        .on_children_prepainted({
                            let over = overflow.clone();
                            // Children: the line at its full width, then the fade's full-size canvas.
                            move |bounds, _, _| over.set(matches!(bounds.as_slice(), [line, room, ..] if line.size.width > room.size.width))
                        })
                        .child(
                            // Centered, like the name beside it: at larger text the line is
                            // taller than this box, and from the top it'd sit lower than the name.
                            h_flex()
                                .flex_shrink_0()
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .left_0()
                                .items_center()
                                .gap(u(6.))
                                .whitespace_nowrap()
                                .text_color(theme.foreground.opacity(0.85))
                                .child(preview)
                                .when(!first_line.is_empty() && !m.attachments.is_empty(), |d| {
                                    d.child(gpui_kit::component::Icon::new(gpui_kit::assets::IconName::Image).size(u(16.)).flex_shrink_0())
                                }),
                        )
                        .child({
                            // Only when something is really cut off: a fade over a
                            // short line would be a dark block on a lit row.
                            // The fade ends in the row's own colour: a ping's is tinted gold.
                            let pings = env.cache.me().is_some_and(|me| {
                                let roles = env.guild.and_then(|g| env.cache.member(g, me.id)).map_or(&[][..], |m| m.roles.as_slice());
                                crate::chat_format::mentions_me(message, me.id, roles)
                            });
                            let fade = if pings { theme.background.blend(crate::pages::channel::GOLD.opacity(0.08)) } else { theme.background };
                            let over = overflow.clone();
                            gpui_kit::canvas(
                                |_, _, _| {},
                                move |bounds, _, window, _| {
                                    if !over.get() {
                                        return;
                                    }
                                    let width = window.rem_size() * 2.;
                                    let edge = gpui_kit::Bounds::new(
                                        gpui_kit::point(bounds.right() - width, bounds.top()),
                                        gpui_kit::size(width, bounds.size.height),
                                    );
                                    window.paint_quad(gpui_kit::fill(
                                        edge,
                                        gpui_kit::linear_gradient(
                                            90.,
                                            gpui_kit::linear_color_stop(fade.opacity(0.), 0.),
                                            gpui_kit::linear_color_stop(fade, 1.),
                                        ),
                                    ));
                                },
                            )
                            .absolute()
                            .size_full()
                        }),
                )
                .into_any_element()
        }
        None => div().italic().child("Original message was deleted").into_any_element(),
    };

    Some(
        h_flex()
            .id(eid(env, "reply", "line"))
            .w_full()
            // Discord's: an 18px line with 4px under it.
            .min_h(u(18.))
            .mb(u(4.))
            .gap(u(4.))
            .items_start()
            .text_sm()
            .text_color(theme.muted_foreground)
            .when_some(jump, |d, (id, jump)| d.cursor_pointer().on_click(move |_, _, cx| jump(id, cx)))
            .child(connector)
            .child(div().min_w_0().flex_1().pt(u(1.)).child(content))
            .into_any_element(),
    )
}

/// Replies get their own header line, so they never stack under the
/// previous message.
pub fn is_reply(message: &Message) -> bool {
    message.kind == KIND_REPLY || message.command_use().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cropped_picture_is_asked_for_big_enough_to_cover_its_box() {
        // A tall picture in a wide box: its width decides.
        assert_eq!(covering_size(300., 100., Some((100, 400))), (300., 1200.));
        assert_eq!(covering_size(300., 100., None), (300., 100.));
    }

    #[test]
    fn big_link_previews_go_under_the_text() {
        assert!(!thumb_goes_beside("article", false, Some((1200, 600))));
        assert!(thumb_goes_beside("article", false, Some((128, 128))));
        assert!(thumb_goes_beside("rich", false, Some((1200, 600))));
        assert!(thumb_goes_beside("link", false, None));
    }
    use cord_core::Id;

    #[test]
    fn mosaic_rows_follow_discord() {
        assert_eq!(mosaic_rows(2), vec![2]);
        assert_eq!(mosaic_rows(4), vec![2, 2]);
        assert_eq!(mosaic_rows(5), vec![2, 3]);
        assert_eq!(mosaic_rows(6), vec![3, 3]);
        assert_eq!(mosaic_rows(7), vec![1, 3, 3]);
        assert_eq!(mosaic_rows(10), vec![1, 3, 3, 3]);
    }

    #[test]
    fn same_link_pictures_fold_into_one_embed() {
        let pic = |u: &str| Some(EmbedMedia { url: u.into(), ..Default::default() });
        let post = Embed { url: Some("https://x.com/p/1".into()), title: Some("post".into()), image: pic("a"), ..Default::default() };
        let more = |u: &str| Embed { url: Some("https://x.com/p/1".into()), image: pic(u), ..Default::default() };
        let other = Embed { url: Some("https://x.com/p/2".into()), image: pic("z"), ..Default::default() };
        let embeds = vec![post, more("b"), more("c"), other];
        let groups = embed_groups(&embeds);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, 0);
        assert_eq!(groups[0].1.iter().map(|m| m.url.as_str()).collect::<Vec<_>>(), ["b", "c"]);
        assert_eq!(groups[1], (3, vec![]));
    }

    fn field(inline: bool) -> EmbedField {
        EmbedField { name: "n".into(), value: "v".into(), inline }
    }

    #[test]
    fn images_shrink_to_fit_keeping_the_ratio() {
        assert_eq!(fit_image(800, 600, 400., 300.), (400., 300.));
        assert_eq!(fit_image(1600, 600, 400., 300.), (400., 150.));
        assert_eq!(fit_image(600, 1200, 400., 300.), (150., 300.));
        // Never enlarged.
        assert_eq!(fit_image(100, 50, 400., 300.), (100., 50.));
        // Degenerate sizes don't divide by zero.
        assert_eq!(fit_image(0, 0, 400., 300.), (1., 1.));
    }

    #[test]
    fn byte_sizes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(20 * 1024 * 1024), "20.0 MB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.0 GB");
    }

    #[test]
    fn sticker_pictures() {
        let s = |format_type| StickerItem { id: Id(9), name: "wave".into(), format_type };
        assert_eq!(sticker_url(&s(1)).as_deref(), Some("https://media.discordapp.net/stickers/9.png?size=160"));
        assert_eq!(sticker_url(&s(2)), sticker_url(&s(1)));
        assert_eq!(sticker_url(&s(4)).as_deref(), Some("https://media.discordapp.net/stickers/9.gif?size=160"));
        assert_eq!(sticker_url(&s(3)), None);
    }

    #[test]
    fn inline_fields_fill_rows_of_three() {
        let fields: Vec<_> = [true, true, true, true, false, true, false].into_iter().map(field).collect();
        assert_eq!(field_rows(&fields), vec![vec![0, 1, 2], vec![3], vec![4], vec![5], vec![6]]);
        assert_eq!(field_rows(&[]), Vec::<Vec<usize>>::new());
        assert_eq!(field_rows(&[field(true), field(true)]), vec![vec![0, 1]]);
    }

    #[test]
    fn component_media_resolves_attachment_references() {
        let a = Attachment {
            id: Id(1),
            filename: "pic.png".into(),
            url: "https://cdn/pic.png".into(),
            proxy_url: "https://proxy/pic.png".into(),
            width: Some(10),
            height: Some(20),
            ..Attachment::default()
        };
        let message = test_message(vec![a]);
        let by_reference = ComponentMedia { url: "attachment://pic.png".into(), ..ComponentMedia::default() };
        assert_eq!(resolve_media(&message, &by_reference), ("https://proxy/pic.png", Some((10, 20))));
        let direct = ComponentMedia { url: "https://x/y.png".into(), ..ComponentMedia::default() };
        assert_eq!(resolve_media(&message, &direct), ("https://x/y.png", None));
        let missing = ComponentMedia { url: "attachment://nope.png".into(), ..ComponentMedia::default() };
        assert_eq!(resolve_media(&message, &missing).0, "attachment://nope.png");
    }

    fn test_message(attachments: Vec<Attachment>) -> Message {
        serde_json::from_value(serde_json::json!({
            "id": "1", "channel_id": "2", "author": {"id": "3", "username": "u"}, "timestamp": "t"
        }))
        .map(|mut m: Message| {
            m.attachments = attachments;
            m
        })
        .unwrap()
    }
}

#[cfg(test)]
mod poll_tests {
    use super::*;

    fn notice(fields: &[(&str, &str)]) -> Message {
        let fields: Vec<_> = fields.iter().map(|(name, value)| serde_json::json!({"name": name, "value": value})).collect();
        serde_json::from_value(serde_json::json!({
            "id": "1", "channel_id": "2", "timestamp": "2026-11-01T19:19:00+00:00", "type": 46,
            "author": {"id": "3", "username": "n"},
            "embeds": [{"type": "poll_result", "fields": fields}],
        }))
        .unwrap()
    }

    #[test]
    fn system_messages_read_like_discords() {
        let mut m = notice(&[]);
        m.kind = 7;
        let line = system_line(&m).unwrap();
        assert_eq!(line.kind, SystemKind::Join);
        let (before, after) = WELCOMES[(m.id.timestamp_ms() % 13) as usize];
        assert_eq!((line.before, line.after.as_str()), (before, after));
        m.kind = 10;
        assert!(system_line(&m).unwrap().after.ends_with("Level 2!"));
        m.kind = 0;
        assert_eq!(system_line(&m), None);
    }

    #[test]
    fn says_who_ran_a_command() {
        let mut m = notice(&[]);
        assert!(m.command_use().is_none() && !is_reply(&m));
        let user = |id: u64| serde_json::from_value(serde_json::json!({"id": id.to_string(), "username": "n"})).unwrap();
        m.interaction_metadata = Some(cord_core::model::CommandUse { name: None, user: user(7) });
        assert!(m.command_use().is_none(), "a button press isn't a command");
        m.interaction = Some(cord_core::model::CommandUse { name: Some("set voice".into()), user: user(8) });
        let (who, name) = m.command_use().unwrap();
        assert_eq!((who.id, name), (cord_core::Id(8), "set voice"));
        assert!(is_reply(&m));
    }

    #[test]
    fn reads_an_automod_alert() {
        let mut m = notice(&[
            ("rule_name", "Block Custom Words"), ("channel_id", "15"), ("keyword", "word"),
            ("keyword_matched_content", "word"), ("decision_outcome", "blocked"),
        ]);
        assert_eq!(automod_alert(&m), None);
        m.embeds[0].kind = Some("auto_moderation_message".into());
        m.embeds[0].description = Some("a word here".into());
        let alert = automod_alert(&m).unwrap();
        assert_eq!((alert.text.as_str(), alert.rule.as_str()), ("a word here", "Block Custom Words"));
        assert_eq!(alert.channel, Some(cord_core::Id(15)));
        assert!(!alert.flagged);
    }

    #[test]
    fn finds_words_whatever_their_case() {
        assert_eq!(find_ignoring_case("a WoRd and word", "word"), vec![2..6, 11..15]);
        assert_eq!(find_ignoring_case("héllo É", "é"), vec![1..3, 7..9]);
        assert!(find_ignoring_case("abc", "").is_empty());
        assert!(find_ignoring_case("abc", "abcd").is_empty());
    }

    #[test]
    fn reads_the_winner_off_the_notice() {
        let m = notice(&[
            ("poll_question_text", "Demote niz"), ("victor_answer_votes", "5"), ("total_votes", "9"),
            ("victor_answer_id", "2"), ("victor_answer_text", "No"), ("victor_answer_emoji_name", "x"),
        ]);
        let outcome = poll_outcome(&m).unwrap();
        assert_eq!(outcome.question, "Demote niz");
        assert_eq!((outcome.votes, outcome.total), (5, 9));
        let (text, emoji) = outcome.winner.unwrap();
        assert_eq!(text, "No");
        assert_eq!(emoji.unwrap().name.as_deref(), Some("x"));
    }

    #[test]
    fn a_tie_has_no_winner_and_other_messages_are_no_notice() {
        let tied = notice(&[("poll_question_text", "q"), ("total_votes", "4")]);
        assert_eq!(poll_outcome(&tied).unwrap().winner, None);
        let mut plain = tied;
        plain.embeds.clear();
        assert_eq!(poll_outcome(&plain), None);
    }
}
