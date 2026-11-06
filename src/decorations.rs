//! Avatar decorations that move. They're animated PNGs, which the image loader draws as one
//! still, so the animated ones are decoded here, only for where they're meant to play: the
//! row or message under the pointer, and the profile previews. Everywhere else the still is
//! drawn, which costs nothing.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures::AsyncReadExt as _;
use gpui_kit::{AnyElement, App, IntoElement as _, ObjectFit, ParentElement as _, RenderImage, Styled as _, canvas, div};

/// Decoded decorations kept, by the bytes their frames take.
const BUDGET: usize = 48 * 1024 * 1024;
/// One nobody's drawn for this long is let go of.
const REST_AFTER: Duration = Duration::from_secs(20);

enum State {
    Loading,
    Ready { image: Arc<RenderImage>, bytes: usize },
    Failed,
}

#[derive(Default)]
struct Pool(VecDeque<(String, State)>);

thread_local! {
    /// Everything decoded or on its way. The UI thread's alone.
    static POOL: std::cell::RefCell<Pool> = std::cell::RefCell::default();
    /// Wanted while being laid out, started once there's an `App` to start them with.
    static WANTED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The URL of the picture that moves, for a still one.
pub fn animated(still: &str) -> String {
    still.replace("passthrough=false", "passthrough=true")
}

/// The URL of the still, for one that moves.
fn still(url: &str) -> String {
    url.replace("passthrough=true", "passthrough=false")
}

/// Whether `url` asks for the version that moves.
fn moves(url: &str) -> bool {
    url.contains("passthrough=true")
}

/// A decoration to draw. `url` is the animated one or the still: the animated one plays
/// once it's loaded (showing the still till then), the still stays still.
pub fn view(url: &str) -> AnyElement {
    if !moves(url) {
        return crate::anim::image(url.to_string(), ObjectFit::Contain, 0.).size_full().into_any_element();
    }
    crate::anim::note_used(url);
    let ready = POOL.with_borrow(|pool| {
        pool.0.iter().find_map(|(known, state)| match state {
            State::Ready { image, .. } if known == url => Some(image.clone()),
            _ => None,
        })
    });
    match ready {
        Some(image) => crate::anim::image(image, ObjectFit::Contain, 0.).size_full().into_any_element(),
        None => {
            WANTED.with_borrow_mut(|wanted| {
                if !wanted.iter().any(|w| w == url) {
                    wanted.push(url.to_string());
                }
            });
            div()
                .size_full()
                .child(crate::anim::image(still(url), ObjectFit::Contain, 0.).size_full())
                // Starts what was asked for: here there's something to start it with.
                .child(canvas(|_, _, cx| start_wanted(cx), |_, _, _, _| {}).absolute().size_0())
                .into_any_element()
        }
    }
}

fn start_wanted(cx: &mut App) {
    let wanted = WANTED.with_borrow_mut(std::mem::take);
    if wanted.is_empty() {
        return;
    }
    for url in wanted {
        let new = POOL.with_borrow_mut(|pool| {
            let known = pool.0.iter().any(|(known, _)| *known == url);
            if !known {
                pool.0.push_back((url.clone(), State::Loading));
            }
            !known
        });
        if !new {
            continue;
        }
        let client = cx.http_client();
        cx.spawn(async move |cx| {
            let bytes = async {
                let mut response = client.get(&url, Default::default(), true).await.ok()?;
                let mut bytes = Vec::new();
                response.body_mut().read_to_end(&mut bytes).await.ok()?;
                Some(bytes)
            }
            .await;
            let job = cx.background_executor().spawn(async move { crate::stickers::render_apng(&bytes?) });
            let state = match job.await {
                Some(image) => {
                    let size = image.size(0);
                    let bytes = (size.width.0.max(0) as usize) * (size.height.0.max(0) as usize) * 4 * image.frame_count();
                    State::Ready { image: Arc::new(image), bytes }
                }
                None => State::Failed,
            };
            let _ = cx.update(|cx| {
                let over = POOL.with_borrow_mut(|pool| {
                    if let Some(entry) = pool.0.iter_mut().find(|(known, _)| *known == url) {
                        entry.1 = state;
                    }
                    over_budget(pool)
                });
                for image in over {
                    cx.drop_image(image, None);
                }
                cx.refresh_windows();
            });
        })
        .detach();
    }
}

/// Takes out the oldest ready ones until what's held fits the budget.
fn over_budget(pool: &mut Pool) -> Vec<Arc<RenderImage>> {
    let mut gone = Vec::new();
    let mut total: usize = pool.0.iter().map(|(_, s)| if let State::Ready { bytes, .. } = s { *bytes } else { 0 }).sum();
    while total > BUDGET {
        let Some(at) = pool.0.iter().position(|(_, s)| matches!(s, State::Ready { .. })) else { break };
        if let Some((_, State::Ready { image, bytes })) = pool.0.remove(at) {
            total -= bytes;
            gone.push(image);
        }
    }
    gone
}

/// Lets go of decorations that haven't been drawn for a while.
pub fn rest(cx: &mut App) {
    let idle: Vec<Arc<RenderImage>> = POOL.with_borrow_mut(|pool| {
        let mut gone = Vec::new();
        pool.0.retain_mut(|(url, state)| {
            if matches!(state, State::Loading) || !crate::anim::unused_for(url, REST_AFTER) {
                return true;
            }
            if let State::Ready { image, .. } = state {
                gone.push(image.clone());
            }
            false
        });
        gone
    });
    for image in idle {
        cx.drop_image(image, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_versions_are_the_same_picture_asked_for_differently() {
        let still = "https://cdn.discordapp.com/avatar-decoration-presets/a_x.png?size=96&passthrough=false";
        let moving = animated(still);
        assert!(moving.ends_with("passthrough=true") && moves(&moving) && !moves(still));
        assert_eq!(super::still(&moving), still);
    }

    #[test]
    fn the_oldest_go_when_over_budget_and_the_loading_stay() {
        let frame = || Arc::new(RenderImage::new([image::Frame::new(image::RgbaImage::new(1, 1))]));
        let mut pool = Pool::default();
        pool.0.push_back(("a".into(), State::Ready { image: frame(), bytes: BUDGET / 2 + 1 }));
        pool.0.push_back(("loading".into(), State::Loading));
        pool.0.push_back(("b".into(), State::Ready { image: frame(), bytes: BUDGET / 2 + 1 }));
        assert_eq!(over_budget(&mut pool).len(), 1);
        let left: Vec<&str> = pool.0.iter().map(|(u, _)| u.as_str()).collect();
        assert_eq!(left, ["loading", "b"]);
    }
}
