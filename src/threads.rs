//! Wording and numbers for threads and forum posts that need no window:
//! "3 Messages", "5 minutes ago", and what a forum card shows of a post.

use cord_core::Id;
use cord_core::cache::Cache;
use cord_core::model::{Channel, Message};

/// "just now", "5 minutes ago", "3 hours ago", "2 days ago", "4 months ago", "1 year ago".
pub fn ago(now_ms: u64, then_ms: u64) -> String {
    let secs = now_ms.saturating_sub(then_ms) / 1000;
    let (n, unit) = match secs {
        0..60 => return "just now".to_string(),
        60..3600 => (secs / 60, "minute"),
        3600..86_400 => (secs / 3600, "hour"),
        86_400..2_592_000 => (secs / 86_400, "day"),
        2_592_000..31_536_000 => (secs / 2_592_000, "month"),
        _ => (secs / 31_536_000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// "1 Message", "12 Messages".
pub fn messages(count: u32) -> String {
    format!("{count} Message{}", if count == 1 { "" } else { "s" })
}

/// A line of text from a message: newlines and runs of spaces become one space,
/// cut at `max` characters with an ellipsis.
pub fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{}\u{2026}", cut.trim_end())
}

/// Who started `thread`, by the name they go by here, when we know them.
pub fn starter_name(cache: &Cache, thread: &Channel) -> Option<String> {
    let owner = thread.owner_id?;
    if let Some(user) = cache.user(owner) {
        return Some(cache.display_name(thread.guild_id, user).to_string());
    }
    // Their own messages know them too.
    let author = cache.messages(thread.id)?.iter().map(|m| &m.message.author).find(|a| a.id == owner)?;
    Some(cache.display_name(thread.guild_id, author).to_string())
}

/// One post in a forum, ready to draw.
#[derive(Clone, Debug, PartialEq)]
pub struct PostCard {
    pub id: Id,
    pub title: String,
    pub author: Option<String>,
    pub author_avatar: Option<String>,
    /// (name, emoji) of each tag, the emoji as text or a custom emoji's id.
    pub tags: Vec<CardTag>,
    pub replies: u32,
    pub preview: String,
    pub image: Option<String>,
    pub created_ms: u64,
    pub active_ms: u64,
    pub pinned: bool,
    pub unread: bool,
    pub archived: bool,
    pub locked: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CardTag {
    pub name: String,
    pub emoji_name: Option<String>,
    pub emoji_id: Option<Id>,
}

/// The first picture of a message: an attachment, or an embed's image.
pub fn first_image(message: &Message) -> Option<String> {
    message
        .attachments
        .iter()
        .find(|a| a.is_image())
        .map(|a| a.display_url().to_string())
        .or_else(|| message.embeds.iter().find_map(|e| e.image.as_ref().or(e.thumbnail.as_ref())).map(|m| m.display_url().to_string()))
}

/// What a card shows of `post`, given its opening message when we have it.
pub fn post_card(cache: &Cache, post: &Channel, first: Option<&Message>) -> PostCard {
    let info = post.thread.clone().unwrap_or_default();
    let forum = post.parent_id.and_then(|p| cache.channel(p)).and_then(|p| p.forum.as_ref());
    let tags = info
        .applied_tags
        .iter()
        .filter_map(|id| forum?.tag(*id))
        .map(|t| CardTag { name: t.name.clone(), emoji_name: t.emoji_name.clone(), emoji_id: t.emoji_id })
        .collect();
    let author = first.map(|m| &m.author).or_else(|| post.owner_id.and_then(|o| cache.user(o)));
    PostCard {
        id: post.id,
        title: post.name.clone().unwrap_or_default(),
        author: author.map(|a| cache.display_name(post.guild_id, a).to_string()),
        author_avatar: author.map(|a| cache.avatar_url(post.guild_id, a, 64)),
        tags,
        replies: info.message_count,
        preview: first.map(|m| one_line(&cache.readable_text(m), 220)).unwrap_or_default(),
        image: first.and_then(first_image),
        created_ms: post.id.timestamp_ms(),
        active_ms: post.last_message_id.unwrap_or(post.id).max(post.id).timestamp_ms(),
        pinned: info.pinned(),
        unread: cache.is_unread(post.id),
        archived: info.archived,
        locked: info.locked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_the_way_discord_says_them() {
        let now = 100_000_000_000;
        assert_eq!(ago(now, now - 20_000), "just now");
        assert_eq!(ago(now, now - 60_000), "1 minute ago");
        assert_eq!(ago(now, now - 5 * 60_000), "5 minutes ago");
        assert_eq!(ago(now, now - 3 * 3_600_000), "3 hours ago");
        assert_eq!(ago(now, now - 49 * 3_600_000), "2 days ago");
        assert_eq!(ago(now, now - 70 * 86_400_000), "2 months ago");
        assert_eq!(ago(now, now - 400 * 86_400_000), "1 year ago");
        // A clock a little behind ours is still "just now".
        assert_eq!(ago(now, now + 5_000), "just now");
    }

    #[test]
    fn counts_have_the_right_plural() {
        assert_eq!(messages(1), "1 Message");
        assert_eq!(messages(0), "0 Messages");
    }

    #[test]
    fn previews_are_one_short_line() {
        assert_eq!(one_line("a\n\n  b   c", 20), "a b c");
        assert_eq!(one_line("abcdefghij", 5), "abcd\u{2026}");
        assert_eq!(one_line("   ", 5), "");
    }

    fn message(v: serde_json::Value) -> Message {
        let mut base = serde_json::json!({"id": "20", "channel_id": "20", "timestamp": "t", "author": {"id": "5", "username": "ada"}});
        base.as_object_mut().unwrap().extend(v.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    #[test]
    fn a_card_shows_the_post_as_the_forum_knows_it() {
        use cord_core::event::CoreEvent;
        let channel = |v: serde_json::Value| serde_json::from_value::<Channel>(v).unwrap();
        let mut cache = Cache::default();
        cache.apply(CoreEvent::ChannelUpsert(channel(serde_json::json!({
            "id": "9", "type": 15, "guild_id": "1", "available_tags": [{"id": "3", "name": "Solved", "emoji_name": "ok"}, {"id": "4", "name": "Open"}],
        }))));
        let post = channel(serde_json::json!({
            "id": "20", "type": 11, "parent_id": "9", "guild_id": "1", "name": "How do I?", "message_count": 6, "last_message_id": "9999999999999999",
            "applied_tags": ["3", "77"], "thread_metadata": {"archived": true, "locked": true},
        }));
        let first = message(serde_json::json!({
            "content": "  Please\nhelp  ", "attachments": [{"id": "2", "filename": "b.png", "url": "https://x/b.png", "content_type": "image/png"}],
        }));
        let card = post_card(&cache, &post, Some(&first));
        assert_eq!(card.title, "How do I?");
        assert_eq!(card.replies, 6);
        assert_eq!(card.preview, "Please help");
        // A tag the forum no longer has is dropped.
        assert_eq!(card.tags.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["Solved"]);
        assert!(card.image.unwrap().contains("b.png"));
        assert!(card.archived && card.locked && !card.pinned);
        assert!(card.active_ms > card.created_ms);
        assert_eq!(card.author.as_deref(), Some("ada"));
        // Before its first message has come, only what the post itself says.
        let bare = post_card(&cache, &post, None);
        assert!(bare.preview.is_empty() && bare.image.is_none() && bare.author.is_none());
    }

    #[test]
    fn the_first_picture_is_an_attachment_or_an_embed() {
        let none = message(serde_json::json!({"content": "hi"}));
        assert_eq!(first_image(&none), None);
        let attached = message(serde_json::json!({"attachments": [
            {"id": "1", "filename": "a.txt", "url": "https://x/a.txt", "proxy_url": "https://p/a.txt", "size": 1, "content_type": "text/plain"},
            {"id": "2", "filename": "b.png", "url": "https://x/b.png", "proxy_url": "https://p/b.png", "size": 1, "content_type": "image/png", "width": 4, "height": 4},
        ]}));
        assert!(first_image(&attached).unwrap().contains("b.png"));
        let embedded = message(serde_json::json!({"embeds": [{"type": "image", "image": {"url": "https://x/e.png", "proxy_url": "https://p/e.png"}}]}));
        assert!(first_image(&embedded).unwrap().contains("e.png"));
    }
}
