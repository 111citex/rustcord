//! Where a clicked link goes: Discord's own channel and message links open
//! here, in the app, like the official client; anything else in the browser.

use cord_core::Id;
use gpui_kit::{App, Entity, Global};

use crate::router::{Router, navigate};

struct AppRouter(Entity<Router>);

impl Global for AppRouter {}

/// A message a link pointed at, for its channel's page to scroll to.
struct PendingJump {
    channel: Id,
    message: Id,
}

impl Global for PendingJump {}

/// Called once at start, so links anywhere can route in-app.
pub fn init(router: Entity<Router>, cx: &mut App) {
    cx.set_global(AppRouter(router));
}

pub fn open(url: &str, cx: &mut App) {
    match (in_app_path(url), cx.try_global::<AppRouter>().map(|r| r.0.clone())) {
        (Some(path), Some(router)) => {
            if let Some((channel, message)) = linked_message(url) {
                cx.set_global(PendingJump { channel, message });
            }
            navigate(&router, path, cx);
            // The channel may already be open: its page picks the jump up on redraw.
            cx.refresh_windows();
        }
        _ => cx.open_url(url),
    }
}

/// A person clicked outside the chat (the voice list): the page opens their card.
struct PendingProfile {
    user: Id,
    position: gpui_kit::Point<gpui_kit::Pixels>,
}

impl Global for PendingProfile {}

pub fn open_profile(user: Id, position: gpui_kit::Point<gpui_kit::Pixels>, cx: &mut App) {
    cx.set_global(PendingProfile { user, position });
    cx.refresh_windows();
}

pub fn take_profile(cx: &mut App) -> Option<(Id, gpui_kit::Point<gpui_kit::Pixels>)> {
    let pending = cx.try_global::<PendingProfile>().map(|p| (p.user, p.position))?;
    cx.remove_global::<PendingProfile>();
    Some(pending)
}

/// The message a link asked for in `channel`, taken so it's jumped to once.
pub fn take_jump(channel: Id, cx: &mut App) -> Option<Id> {
    let pending = cx.try_global::<PendingJump>().filter(|j| j.channel == channel)?.message;
    cx.remove_global::<PendingJump>();
    Some(pending)
}

/// `…/channels/<guild>/<channel>/<message>` → (channel, message).
fn linked_message(url: &str) -> Option<(Id, Id)> {
    let path = url.split(['?', '#']).next()?;
    let mut parts = path.rsplit('/');
    let message = parts.next()?.parse().ok()?;
    let channel = parts.next()?.parse().ok()?;
    (parts.next()? != "channels").then_some((Id(channel), Id(message)))
}

/// Opens an app path (`/channels/…`) directly.
pub fn go(path: String, cx: &mut App) {
    if let Some(router) = cx.try_global::<AppRouter>().map(|r| r.0.clone()) {
        navigate(&router, path, cx);
    }
}

/// `https://discord.com/channels/<guild|@me>/<channel>[/<message>]` → the
/// app's `/channels/<guild|@me>/<channel>` (the message itself isn't jumped to yet).
pub fn in_app_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let (host, path) = rest.split_once('/')?;
    if !matches!(host, "discord.com" | "ptb.discord.com" | "canary.discord.com" | "discordapp.com") {
        return None;
    }
    let mut parts = path.split(['?', '#']).next()?.split('/');
    if parts.next()? != "channels" {
        return None;
    }
    let guild = parts.next()?;
    let channel = parts.next()?;
    let id = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    ((guild == "@me" || id(guild)) && id(channel)).then(|| format!("/channels/{guild}/{channel}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discord_links_route_in_app() {
        assert_eq!(in_app_path("https://discord.com/channels/1/2/3").as_deref(), Some("/channels/1/2"));
        assert_eq!(in_app_path("https://ptb.discord.com/channels/@me/2").as_deref(), Some("/channels/@me/2"));
        assert_eq!(in_app_path("https://discord.com/invite/abc"), None);
        assert_eq!(in_app_path("https://example.com/channels/1/2"), None);
    }

    #[test]
    fn message_links_name_their_message() {
        assert_eq!(linked_message("https://discord.com/channels/1/2/3"), Some((Id(2), Id(3))));
        assert_eq!(linked_message("https://discord.com/channels/@me/2/3"), Some((Id(2), Id(3))));
        assert_eq!(linked_message("https://discord.com/channels/1/2"), None);
    }
}

thread_local! {
    static MENTION: std::cell::RefCell<Option<(cord_core::Id, String)>> = const { std::cell::RefCell::new(None) };
}

/// Puts someone's mention in the message box of the chat on screen.
pub fn request_mention(user: cord_core::Id, name: String, cx: &mut gpui_kit::App) {
    MENTION.with_borrow_mut(|m| *m = Some((user, name)));
    crate::ui::redraw_chat(cx);
}

pub fn take_mention() -> Option<(cord_core::Id, String)> {
    MENTION.with_borrow_mut(Option::take)
}
