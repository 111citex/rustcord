//! The rules of forum posts and threads that need no window to check: what a
//! new post must have, how posts are ordered and filtered, what a thread is
//! called by default.

use crate::Id;
use crate::model::{Channel, ForumInfo};

/// Discord's limits on a thread's name and a post's tags.
pub const NAME_LIMIT: usize = 100;
pub const TAG_LIMIT: usize = 5;

/// What a new post is missing, in words for the person making it. `manager`:
/// they may manage threads, which lifts the required tag and the moderated tags.
pub fn post_problem(forum: &ForumInfo, title: &str, content: &str, has_files: bool, tags: &[Id], manager: bool) -> Option<String> {
    if title.trim().is_empty() {
        return Some("Give your post a title.".into());
    }
    if title.trim().chars().count() > NAME_LIMIT {
        return Some(format!("A title can be at most {NAME_LIMIT} characters."));
    }
    if content.trim().is_empty() && !has_files {
        return Some("Write something for your post, or attach a file.".into());
    }
    if tags.len() > TAG_LIMIT {
        return Some(format!("A post takes at most {TAG_LIMIT} tags."));
    }
    if forum.requires_tag() && tags.is_empty() && !manager {
        return Some("Pick at least one tag for your post.".into());
    }
    if !manager && let Some(tag) = tags.iter().filter_map(|id| forum.tag(*id)).find(|t| t.moderated) {
        return Some(format!("Only moderators can use the tag \"{}\".", tag.name));
    }
    None
}

/// Newest activity first. A pinned post stays on top.
pub fn sort_posts(posts: &mut [&Channel], by_creation: bool) {
    posts.sort_by_key(|p| {
        let pinned = p.thread.as_ref().is_some_and(|t| t.pinned());
        let when = if by_creation { p.id } else { p.last_message_id.unwrap_or(p.id).max(p.id) };
        (std::cmp::Reverse(pinned), std::cmp::Reverse(when))
    });
}

/// Whether `post` has any of `tags`; with none chosen, every post.
pub fn matches_tags(post: &Channel, tags: &[Id]) -> bool {
    tags.is_empty() || post.thread.as_ref().is_some_and(|t| t.applied_tags.iter().any(|id| tags.contains(id)))
}

/// The name a thread starts with: the message's first line, as Discord does.
pub fn default_thread_name(content: &str) -> String {
    let line = content.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let name: String = line.chars().take(NAME_LIMIT).collect();
    let name = name.trim().to_string();
    if name.is_empty() { "New Thread".into() } else { name }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ForumTag;

    fn tag(id: u64, moderated: bool) -> ForumTag {
        ForumTag { id: Id(id), name: format!("tag{id}"), moderated, emoji_id: None, emoji_name: None }
    }

    fn forum(required: bool) -> ForumInfo {
        ForumInfo { tags: vec![tag(1, false), tag(2, true)], flags: if required { crate::model::FORUM_REQUIRE_TAG } else { 0 }, ..Default::default() }
    }

    fn post(id: u64, last: Option<u64>, tags: &[u64], pinned: bool) -> Channel {
        serde_json::from_value(serde_json::json!({
            "id": id.to_string(), "type": 11, "parent_id": "9", "last_message_id": last.map(|l| l.to_string()),
            "applied_tags": tags.iter().map(u64::to_string).collect::<Vec<_>>(), "flags": if pinned { 2 } else { 0 },
        }))
        .unwrap()
    }

    #[test]
    fn a_post_needs_a_title_and_something_to_say() {
        let f = forum(false);
        assert!(post_problem(&f, "  ", "text", false, &[], false).unwrap().contains("title"));
        assert!(post_problem(&f, "t", " ", false, &[], false).is_some());
        assert!(post_problem(&f, "t", "", true, &[], false).is_none());
        assert!(post_problem(&f, &"x".repeat(101), "text", false, &[], false).is_some());
        assert!(post_problem(&f, &"x".repeat(100), "text", false, &[], false).is_none());
    }

    #[test]
    fn required_tags_bind_everyone_but_managers() {
        let f = forum(true);
        assert!(post_problem(&f, "t", "x", false, &[], false).unwrap().contains("tag"));
        assert!(post_problem(&f, "t", "x", false, &[Id(1)], false).is_none());
        assert!(post_problem(&f, "t", "x", false, &[], true).is_none());
        assert!(post_problem(&forum(false), "t", "x", false, &[], false).is_none());
    }

    #[test]
    fn moderated_tags_and_the_tag_limit() {
        let f = forum(false);
        assert!(post_problem(&f, "t", "x", false, &[Id(2)], false).unwrap().contains("tag2"));
        assert!(post_problem(&f, "t", "x", false, &[Id(2)], true).is_none());
        let many: Vec<Id> = (1..=6).map(Id).collect();
        assert!(post_problem(&f, "t", "x", false, &many, true).is_some());
    }

    #[test]
    fn posts_sort_by_activity_or_creation_with_pins_first() {
        let (a, b, c) = (post(100, Some(900), &[], false), post(200, Some(300), &[], false), post(50, None, &[], true));
        let mut posts = vec![&a, &b, &c];
        sort_posts(&mut posts, false);
        assert_eq!(posts.iter().map(|p| p.id.0).collect::<Vec<_>>(), vec![50, 100, 200]);
        sort_posts(&mut posts, true);
        assert_eq!(posts.iter().map(|p| p.id.0).collect::<Vec<_>>(), vec![50, 200, 100]);
    }

    #[test]
    fn tag_filters_match_any_chosen_tag() {
        let p = post(1, None, &[3, 4], false);
        assert!(matches_tags(&p, &[]));
        assert!(matches_tags(&p, &[Id(4), Id(9)]));
        assert!(!matches_tags(&p, &[Id(9)]));
    }

    #[test]
    fn thread_names_come_from_the_first_line() {
        assert_eq!(default_thread_name("\n  hello there\nsecond"), "hello there");
        assert_eq!(default_thread_name(""), "New Thread");
        assert_eq!(default_thread_name(&"é".repeat(150)).chars().count(), 100);
    }
}
