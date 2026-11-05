//! Hovering someone fetches their profile ahead of a click, so the card opens
//! at once; leaving them alone for a few seconds forgets it again. Discord's
//! client doesn't do this, so it's kept rare: only a pointer that stays put
//! asks, and never more than a few times a minute. A click always fetches.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cord_core::{Core, Id};
use gpui_kit::{App, Global};

/// How long the pointer must rest on someone before asking.
const SETTLE: Duration = Duration::from_millis(800);
/// Preloads allowed in a minute; past that, the click does the fetching.
const PER_MINUTE: usize = 6;
const FORGET_AFTER: Duration = Duration::from_secs(5);

type Key = (Id, Option<Id>);

/// Each person's latest hover change: a timer whose number is stale does nothing.
#[derive(Default)]
struct Hovers(HashMap<Key, u64>, VecDeque<Instant>);

impl Global for Hovers {}

fn take_allowance(cx: &mut App) -> bool {
    let recent = &mut cx.global_mut::<Hovers>().1;
    let now = Instant::now();
    while recent.front().is_some_and(|t| now - *t > Duration::from_secs(60)) {
        recent.pop_front();
    }
    if recent.len() >= PER_MINUTE {
        return false;
    }
    recent.push_back(now);
    true
}

pub fn hover(user: Id, guild: Option<Id>, over: bool, core: &Arc<Core>, cx: &mut App) {
    if !cx.has_global::<Hovers>() {
        cx.set_global(Hovers::default());
    }
    let key = (user, guild);
    let turn = cx.global_mut::<Hovers>().0.entry(key).or_default();
    *turn += 1;
    let turn = *turn;
    let core = core.clone();
    let wait = if over { SETTLE } else { FORGET_AFTER };
    cx.spawn(async move |cx| {
        cx.background_executor().timer(wait).await;
        let current = cx.update(|cx| cx.global::<Hovers>().0.get(&key).copied() == Some(turn));
        if !current {
            return;
        }
        if over {
            // Already at hand costs nothing; otherwise it spends the allowance.
            if core.cached_profile(user, guild).is_none() && !cx.update(|cx| take_allowance(cx)) {
                return;
            }
            if let Ok(profile) = core.user_profile(user, guild).await
                && let Some(sku) = profile.frame_sku.clone()
            {
                let _ = core.profile_frame(sku).await;
            }
        } else {
            core.forget_profile(user, guild);
            let _ = cx.update(|cx| cx.global_mut::<Hovers>().0.remove(&key));
        }
    })
    .detach();
}
