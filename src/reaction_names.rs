//! Who reacted, for a reaction's tooltip: the first few names, asked of Discord
//! only when the pointer rests on the pill, and kept until the count changes.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use cord_core::model::ReactionEmoji;
use cord_core::{Core, Id};
use gpui_kit::{App, Entity, Global};

use crate::store::Store;

const SETTLE: Duration = Duration::from_millis(250);
const SHOWN: usize = 3;

type Key = (Id, String, u32);

#[derive(Default)]
struct Names {
    known: HashMap<Key, Vec<String>>,
    /// The pill under the pointer: a pointer that moved on asks nothing.
    hovered: Option<Key>,
    asked: HashSet<Key>,
}

impl Global for Names {}

fn key(message: Id, emoji: &ReactionEmoji, count: u32) -> Key {
    (message, emoji.path_segment(), count)
}

/// The pointer rests on a pill: fetches its names, once it has stayed a moment.
#[allow(clippy::too_many_arguments)]
pub fn hover(channel: Id, message: Id, emoji: ReactionEmoji, count: u32, guild: Option<Id>, core: Arc<Core>, store: Entity<Store>, cx: &mut App) {
    if !cx.has_global::<Names>() {
        cx.set_global(Names::default());
    }
    let id = key(message, &emoji, count);
    let names = cx.global_mut::<Names>();
    names.hovered = Some(id.clone());
    if names.known.contains_key(&id) || !names.asked.insert(id.clone()) {
        return;
    }
    cx.spawn(async move |cx| {
        cx.background_executor().timer(SETTLE).await;
        if !cx.update(|cx| cx.global::<Names>().hovered.as_ref() == Some(&id)) {
            let _ = cx.update(|cx| cx.global_mut::<Names>().asked.remove(&id));
            return;
        }
        let Ok(users) = core.reaction_users(channel, message, &emoji, SHOWN as u8).await else { return };
        let _ = cx.update(|cx| {
            let found = {
                let store = store.read(cx);
                users.iter().map(|u| store.cache().display_name(guild, u).to_string()).collect()
            };
            cx.global_mut::<Names>().known.insert(id, found);
            cx.refresh_windows();
        });
    })
    .detach();
}

pub fn unhover(cx: &mut App) {
    if cx.has_global::<Names>() {
        cx.global_mut::<Names>().hovered = None;
    }
}

/// The tooltip's words.
pub fn tooltip(message: Id, emoji: &ReactionEmoji, count: u32, cx: &App) -> String {
    let id = key(message, emoji, count);
    let names = cx.try_global::<Names>().and_then(|n| n.known.get(&id)).map_or(&[][..], Vec::as_slice);
    // Ends where the emoji's picture goes.
    who_line(names, count as usize, "").trim_end().to_string()
}

/// "a, b and c reacted with :x:", "a, b, c and 4 others reacted with :x:".
pub fn who_line(names: &[String], total: usize, emoji: &str) -> String {
    let others = total.saturating_sub(names.len());
    let who = match (names, others) {
        ([], _) => return format!("{total} reacted with {emoji}"),
        ([one], 0) => one.clone(),
        (names, 0) => format!("{} and {}", names[..names.len() - 1].join(", "), names[names.len() - 1]),
        (names, 1) => format!("{} and 1 other", names.join(", ")),
        (names, n) => format!("{} and {n} others", names.join(", ")),
    };
    format!("{who} reacted with {emoji}")
}

#[cfg(test)]
mod tests {
    use super::who_line;

    #[test]
    fn names_then_how_many_more() {
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(who_line(&names(&["a"]), 1, "x"), "a reacted with x");
        assert_eq!(who_line(&names(&["a", "b", "c"]), 3, "x"), "a, b and c reacted with x");
        assert_eq!(who_line(&names(&["a", "b", "c"]), 7, "x"), "a, b, c and 4 others reacted with x");
        assert_eq!(who_line(&[], 5, "x"), "5 reacted with x");
    }
}
