//! The little things Discord draws after a name: a server tag pill (with
//! its badge) and a role icon.

use cord_core::cache::RoleIcon;
use gpui_kit::component::ActiveTheme;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{AnimationExt, AnyElement, App, FontWeight, StyledImage, InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement, Styled, div, img};

use crate::ui::u;

/// "♡ STBL": the server tag a user wears, 18px tall.
pub fn tag_pill(id: impl Into<gpui_kit::ElementId>, tag: &str, badge: Option<&str>, cx: &App) -> AnyElement {
    tag_pill_in(id, tag, badge, cx.theme().foreground)
}

/// `tag_pill` on a background of its own, like a themed profile: in `ink`.
pub fn tag_pill_in(id: impl Into<gpui_kit::ElementId>, tag: &str, badge: Option<&str>, ink: gpui_kit::Hsla) -> AnyElement {
    div()
        .id(id)
        .flex_shrink_0()
        .h(u(18.))
        .px(u(4.))
        .flex()
        .items_center()
        .gap(u(3.))
        .rounded(u(4.))
        .bg(ink.opacity(0.1))
        .text_color(ink)
        .text_size(u(12.))
        .line_height(u(16.))
        .font_weight(FontWeight::SEMIBOLD)
        .when_some(badge, |d, url| d.child(img(url.to_string()).size(u(12.)).flex_shrink_0()))
        .child(tag.to_string())
        .into_any_element()
}

/// A role icon, `size` px square: the role's image, or its emoji.
pub fn role_icon(id: impl Into<SharedString>, icon: &RoleIcon, size: f32) -> AnyElement {
    match icon {
        RoleIcon::Image { url, name } => {
            let name = SharedString::from(name.clone());
            div()
                .id(id.into())
                .flex_shrink_0()
                .tooltip(move |window, cx| Tooltip::new(name.clone()).build(window, cx))
                .child(img(url.clone()).size(u(size)))
                .into_any_element()
        }
        RoleIcon::Emoji(emoji) => emoji_image(emoji, size).into_any_element(),
    }
}

/// The blurple "✓ APP" tag beside a bot's name.
pub fn app_badge() -> AnyElement {
    div()
        .flex_shrink_0()
        .h(u(16.))
        .px(u(5.))
        .flex()
        .items_center()
        .rounded(u(4.))
        .bg(gpui_kit::Hsla::from(gpui_kit::rgb(0x5865f2)))
        .text_color(gpui_kit::white())
        .text_size(u(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .child("\u{2713} APP")
        .into_any_element()
}

/// On Discord's own account, in place of the APP badge.
pub fn official_badge() -> AnyElement {
    div()
        .flex_shrink_0()
        .h(u(16.))
        .px(u(5.))
        .flex()
        .items_center()
        .rounded(u(4.))
        .bg(gpui_kit::Hsla::from(gpui_kit::rgb(0x5865f2)))
        .text_color(gpui_kit::white())
        .text_size(u(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .child("\u{2713} OFFICIAL")
        .into_any_element()
}

/// A server tag as just its badge, as the voice list shows it.
pub fn tag_icon(url: &str, cx: &App) -> AnyElement {
    div()
        .flex_shrink_0()
        .h(u(18.))
        .px(u(4.))
        .flex()
        .items_center()
        .rounded(u(4.))
        .bg(cx.theme().foreground.opacity(0.1))
        .child(img(url.to_string()).size(u(12.)))
        .into_any_element()
}

/// A unicode emoji as Apple's picture, falling back to the character.
pub fn emoji_image(emoji: &str, size: f32) -> gpui_kit::Img {
    let fallback = emoji.to_string();
    img(crate::message_text::emoji_url(emoji))
        .size(u(size))
        .flex_shrink_0()
        .with_fallback(move || div().child(fallback.clone()).into_any_element())
}

/// How a name is drawn. Inside a server a coloured role wins over a Nitro
/// display name style's colours, as in Discord; the style's font stays.
pub struct NameLook {
    /// The display name style's effect (3 neon, 4 toon, 5 pop) and its own colour,
    /// which a role's colour on the text doesn't change.
    pub effect: u8,
    pub effect_color: Option<gpui_kit::Hsla>,
    pub font: Option<&'static str>,
    pub color: Option<gpui_kit::Hsla>,
    /// Two or more: a gradient.
    pub stops: Vec<gpui_kit::Hsla>,
}

pub fn name_look(user: &cord_core::model::User, role_colors: &[u32]) -> NameLook {
    let style = user.extras.display_name_styles.as_ref();
    let colors = if role_colors.is_empty() { style.map_or(&[][..], |s| s.colors.as_slice()) } else { role_colors };
    let stops: Vec<gpui_kit::Hsla> = colors.iter().map(|c| gpui_kit::Hsla::from(gpui_kit::rgb(*c))).collect();
    let effect_color = style.and_then(|s| s.colors.first()).map(|c| gpui_kit::Hsla::from(gpui_kit::rgb(*c)));
    NameLook { effect: style.map_or(0, |s| s.effect_id), effect_color, font: style.and_then(|s| crate::name_fonts::family(s.font_id)), color: stops.first().copied(), stops }
}

/// Discord's gradient repeats every this many pixels of text.
const GRADIENT_LOOP: f32 = 100.;
/// Slices wide. GPUI has no gradient text, so each glyph is drawn once per slice it
/// touches, cut to that slice, in that slice's colour.
const SLICE: f32 = 3.;

/// Where along a gradient's loop the slice at `x` is, `phase` (0 to 1) along.
fn gradient_position(x: f32, phase: f32) -> f32 {
    (x / GRADIENT_LOOP - phase).rem_euclid(1.)
}

/// The little crown beside whoever owns a server or group.
pub fn owner_crown() -> impl IntoElement {
    gpui_kit::component::Icon::default().path("icons/crown.svg").size(gpui_kit::rems(14. / 16.)).flex_shrink_0().text_color(gpui_kit::rgb(0xf0b232))
}

/// Keeps a name to its width. A gradient name paints itself, so its sizing text must
/// never be measured as an ellipsised string: the paint would be clipped to that.
pub fn name_clip<E: gpui_kit::Styled>(element: E, stops: &[gpui_kit::Hsla]) -> E {
    if stops.len() < 2 {
        element.truncate()
    } else {
        element.overflow_hidden().whitespace_nowrap()
    }
}

/// `text` as a smooth left-to-right gradient through `stops`, `phase` (0 to 1) along
/// its loop. Fewer than two stops: plain text (the caller sets its colour).
pub fn gradient_text(text: &str, stops: &[gpui_kit::Hsla], phase: f32) -> AnyElement {
    if stops.len() < 2 || text.is_empty() {
        return text.to_string().into_any_element();
    }
    let (owned, stops) = (gpui_kit::SharedString::from(text.to_string()), stops.to_vec());
    let painted = owned.clone();
    div()
        .relative()
        .min_w_0()
        // Sizes it, and truncates it, as text would be. Never seen.
        .child(div().text_color(gpui_kit::transparent_black()).child(owned))
        .child(
            gpui_kit::canvas(
                |_, _, _| {},
                move |bounds, _, window, cx| {
                    let style = window.text_style();
                    let font_size = style.font_size.to_pixels(window.rem_size());
                    let line_height = window.line_height();
                    let run = style.to_run(painted.len());
                    let line = window.text_system().shape_line(painted.clone(), font_size, &[run], None);
                    // Only ever narrower when something squeezed it: worth knowing if it clips.
                    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                    if bounds.size.width + gpui_kit::px(1.) < line.width && !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        tracing::info!(text = %painted, box_width = f32::from(bounds.size.width), text_width = f32::from(line.width), "gradient name is wider than its box");
                    }
                    // The sizing text's underline is as invisible as the rest of it, so a
                    // name hovered draws its own, in its own colors.
                    let underlined = style.underline.is_some();
                    let baseline = bounds.origin.y + (line_height - line.ascent - line.descent) / 2. + line.ascent;
                    let _ = cx;
                    if underlined {
                        let mut x = 0.;
                        while x < f32::from(line.width) {
                            let width = SLICE.min(f32::from(line.width) - x);
                            let at = gpui_kit::Bounds::new(gpui_kit::point(bounds.origin.x + gpui_kit::px(x), baseline + gpui_kit::px(2.)), gpui_kit::size(gpui_kit::px(width), gpui_kit::px(1.)));
                            window.paint_quad(gpui_kit::fill(at, gradient_at(&stops, gradient_position(x + SLICE / 2., phase))));
                            x += SLICE;
                        }
                    }
                    for run in &line.runs {
                        for (i, glyph) in run.glyphs.iter().enumerate() {
                            let origin = gpui_kit::point(bounds.origin.x + glyph.position.x, baseline);
                            if glyph.is_emoji {
                                let _ = window.paint_emoji(origin, run.font_id, glyph.id, font_size);
                                continue;
                            }
                            let next = run.glyphs.get(i + 1).map_or(line.width, |g| g.position.x);
                            // A little either side: letters lean past their advance.
                            let (from, to) = (f32::from(glyph.position.x) - 2., f32::from(next) + 2.);
                            let mut slice = (from / SLICE).floor() * SLICE;
                            while slice < to {
                                let mask = gpui_kit::Bounds::new(
                                    gpui_kit::point(bounds.origin.x + gpui_kit::px(slice), bounds.origin.y),
                                    gpui_kit::size(gpui_kit::px(SLICE), bounds.size.height),
                                );
                                let color = gradient_at(&stops, gradient_position(slice + SLICE / 2., phase));
                                window.with_content_mask(Some(gpui_kit::ContentMask { bounds: mask }), |window| {
                                    let _ = window.paint_glyph(origin, run.font_id, glyph.id, font_size, color);
                                });
                                slice += SLICE;
                            }
                        }
                    }
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        )
        .into_any_element()
}

/// A name's text: with `moving` set, a gradient drifts along it on a seamless loop.
pub fn name_text(id: impl Into<gpui_kit::ElementId>, text: &str, stops: &[gpui_kit::Hsla], moving: bool) -> AnyElement {
    if !moving || stops.len() < 2 {
        return gradient_text(text, stops, 0.);
    }
    let (text, stops) = (text.to_string(), stops.to_vec());
    div()
        .with_animation(id, gpui_kit::Animation::new(std::time::Duration::from_millis(1500)).repeat(), move |d, t| {
            d.child(gradient_text(&text, &stops, t))
        })
        .into_any_element()
}

/// The stroke around a name, `r` pixels out.
fn stroke(r: f32, strength: f32) -> Vec<(f32, f32, f32)> {
    [(r, 0.), (-r, 0.), (0., r), (0., -r), (r, r), (-r, r), (r, -r), (-r, -r)].map(|(x, y)| (x, y, strength)).to_vec()
}

/// A soft halo: rings of faint copies, the widest and faintest first. Whole
/// pixels only, because that's where text lands.
fn halo() -> Vec<(f32, f32, f32)> {
    const RINGS: [(f32, f32); 2] = [(3., 0.05), (2., 0.1)];
    let mut layers = Vec::new();
    for (radius, strength) in RINGS {
        for step in 0..12 {
            let angle = step as f32 / 12. * std::f32::consts::TAU;
            layers.push(((radius * angle.cos()).round(), (radius * angle.sin()).round(), strength));
        }
    }
    layers
}

/// Copies of a name to draw behind it, as (x, y, strength).
fn effect_layers(effect: u8) -> Vec<(f32, f32, f32)> {
    match effect {
        // Discord's: a thin stroke and `text-shadow: 0 0 calc(4px + .12em)`.
        3 => halo().into_iter().chain(stroke(1., 0.8)).collect(),
        4 => stroke(1., 1.).to_vec(),
        // A dark stroke with the name's colour dropped underneath.
        5 => stroke(1., 0.9).into_iter().chain([(0., 2., 1.)]).collect(),
        _ => Vec::new(),
    }
}

/// `name` with its display name effect, which Discord only shows while the
/// pointer's on the message or row: the rest of the time it's the plain name. Per
/// its CSS, neon and pop draw white text in a stroke and glow (or shadow) of the
/// name's colour. GPUI has no text shadows: those are copies of the text behind it.
pub fn name_effect(name: AnyElement, text: &str, look: &NameLook, hovered: bool) -> AnyElement {
    let layers = effect_layers(look.effect);
    let Some(color) = look.effect_color.or(look.color).filter(|_| hovered && !layers.is_empty()) else { return name };
    let dark = color.blend(gpui_kit::black().opacity(0.45));
    let white_text = matches!(look.effect, 3 | 5);
    div()
        .relative()
        .min_w_0()
        .children(layers.into_iter().map(|(x, y, strength)| {
            // Neon's halo is the colour itself and its stroke a shade darker, so the text stays readable.
            let tint = match look.effect {
                3 if strength < 0.5 => color,
                _ => dark,
            };
            div().absolute().left(gpui_kit::px(x)).top(gpui_kit::px(y)).whitespace_nowrap().text_color(tint.opacity(strength)).child(text.to_string())
        }))
        .child(div().relative().when(white_text, |d| d.text_color(gpui_kit::white())).child(name))
        .into_any_element()
}

/// The colour `t` (0–1) of the way around the loop `stops`, then back to the first.
pub fn gradient_at(stops: &[gpui_kit::Hsla], t: f32) -> gpui_kit::Hsla {
    let n = stops.len();
    let at = t.rem_euclid(1.) * n as f32;
    let i = (at.floor() as usize).min(n - 1);
    let f = at - i as f32;
    let (a, b) = (stops[i].to_rgb(), stops[(i + 1) % n].to_rgb());
    let mix = |x: f32, y: f32| x + (y - x) * f;
    gpui_kit::Rgba { r: mix(a.r, b.r), g: mix(a.g, b.g), b: mix(a.b, b.b), a: 1. }.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gradient_moves_smoothly_along_the_text() {
        assert_eq!(gradient_position(0., 0.), 0.);
        assert_eq!(gradient_position(50., 0.), 0.5);
        assert_eq!(gradient_position(150., 0.), 0.5, "it loops");
        assert_eq!(gradient_position(0., 0.25), 0.75, "the phase drifts it");
    }

    #[test]
    fn the_gradient_loop_closes() {
        let stops = [gpui_kit::Hsla::from(gpui_kit::rgb(0xff0000)), gpui_kit::Hsla::from(gpui_kit::rgb(0x0000ff))];
        let (start, near_end) = (gradient_at(&stops, 0.).to_rgb(), gradient_at(&stops, 0.999).to_rgb());
        assert!((start.r - near_end.r).abs() < 0.01 && (start.b - near_end.b).abs() < 0.01);
        assert!(gradient_at(&stops, 0.5).to_rgb().b > 0.99, "halfway is the second stop");
    }
}

#[cfg(test)]
mod effect_tests {
    use super::effect_layers;

    #[test]
    fn effects_draw_behind_on_whole_pixels() {
        for effect in [3, 4, 5] {
            let layers = effect_layers(effect);
            assert!(!layers.is_empty());
            assert!(layers.iter().all(|l| l.0.fract() == 0. && l.1.fract() == 0.));
        }
        assert!(effect_layers(1).is_empty());
        // Neon's halo goes faintest first, and ends in the stroke.
        let neon = effect_layers(3);
        assert!(neon.last().unwrap().2 >= 0.5);
        assert!(neon.first().unwrap().2 < 0.1);
    }
}
