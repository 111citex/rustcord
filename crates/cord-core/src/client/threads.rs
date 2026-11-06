//! Threads and forum posts over REST.

use serde_json::{Value, json};

use super::*;
use crate::http::Upload;

/// A thread's quiet time before it archives itself, in minutes (Discord's
/// choices; 3 days and 7 days need server boosts on old servers).
pub const AUTO_ARCHIVE: u32 = 1440;

/// Which posts a forum listing asks for.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadSearch {
    /// Archived ones too, or only active ones.
    pub archived: bool,
    /// Newest created first, instead of most recent activity first.
    pub by_creation: bool,
    /// Only posts with one of these tags.
    pub tags: Vec<Id>,
    pub offset: u32,
    pub limit: u8,
}

impl Default for ThreadSearch {
    fn default() -> Self {
        Self { archived: true, by_creation: false, tags: Vec::new(), offset: 0, limit: 25 }
    }
}

impl ThreadSearch {
    pub fn path(&self, channel: Id) -> String {
        let sort = if self.by_creation { "creation_time" } else { "last_message_time" };
        let mut path = format!(
            "/channels/{channel}/threads/search?archived={}&sort_by={sort}&sort_order=desc&limit={}&tag_setting=match_some&offset={}",
            self.archived, self.limit, self.offset
        );
        for tag in &self.tags {
            path.push_str(&format!("&tag={tag}"));
        }
        path
    }
}

/// One page of a thread listing.
#[derive(Clone, Debug, Default)]
pub struct ThreadPage {
    pub threads: Vec<Channel>,
    /// Each post's opening message, for the card's text and picture.
    pub first_messages: Vec<Message>,
    /// The listed threads we're members of.
    pub joined: Vec<Id>,
    pub has_more: bool,
}

impl ThreadPage {
    pub fn from_json(v: &Value) -> ThreadPage {
        let list = |key: &str| v[key].as_array().cloned().unwrap_or_default();
        ThreadPage {
            threads: list("threads").into_iter().filter_map(|t| serde_json::from_value(t).ok()).collect(),
            first_messages: list("first_messages").into_iter().filter_map(|m| serde_json::from_value(m).ok()).collect(),
            joined: list("members").iter().filter_map(|m| m["id"].as_str()?.parse().ok().map(Id)).collect(),
            has_more: v["has_more"].as_bool().unwrap_or(false),
        }
    }
}

/// The body of a new forum post.
#[derive(Clone, Debug, Default)]
pub struct NewPost {
    pub title: String,
    pub content: String,
    pub tags: Vec<Id>,
}

impl NewPost {
    pub fn body(&self) -> Value {
        json!({
            "name": self.title,
            "auto_archive_duration": AUTO_ARCHIVE,
            "applied_tags": self.tags.iter().map(Id::to_string).collect::<Vec<_>>(),
            "message": {"content": self.content},
        })
    }
}

impl Core {
    pub async fn fetch_channel(&self, channel: Id) -> Result<Channel, HttpError> {
        let http = self.http.clone();
        let result = self.on_rt(async move { http.get(&format!("/channels/{channel}")).await }).await;
        self.watch(result)
    }

    /// Joins a thread (sending in it does too).
    pub async fn join_thread(&self, thread: Id) -> Result<(), HttpError> {
        let http = self.http.clone();
        let result = self
            .on_rt(async move {
                let _: Value = http.put(&format!("/channels/{thread}/thread-members/@me")).await?;
                Ok(())
            })
            .await;
        self.watch(result)
    }

    pub async fn leave_thread(&self, thread: Id) -> Result<(), HttpError> {
        let http = self.http.clone();
        let result = self
            .on_rt(async move {
                let _: Value = http.delete(&format!("/channels/{thread}/thread-members/@me")).await?;
                Ok(())
            })
            .await;
        self.watch(result)
    }

    /// Starts a thread from `message`; Discord names it `name`.
    pub async fn start_thread(&self, channel: Id, message: Id, name: String) -> Result<Channel, HttpError> {
        let http = self.http.clone();
        let body = json!({"name": name, "auto_archive_duration": AUTO_ARCHIVE});
        let result = self.on_rt(async move { http.post(&format!("/channels/{channel}/messages/{message}/threads"), &body).await }).await;
        self.watch(result)
    }

    /// A page of a channel's threads, or of a forum's posts.
    pub async fn search_threads(&self, channel: Id, search: ThreadSearch) -> Result<ThreadPage, HttpError> {
        let http = self.http.clone();
        let path = search.path(channel);
        let result = self.on_rt(async move { http.get::<Value>(&path).await }).await;
        self.watch(result.map(|v| ThreadPage::from_json(&v)))
    }

    /// Makes a forum post: a thread whose first message is `post.content`.
    pub async fn create_post(&self, forum: Id, post: NewPost, files: Vec<Upload>) -> Result<(Channel, Option<Message>), HttpError> {
        let http = self.http.clone();
        let body = post.body();
        let result = self
            .on_rt(async move {
                let v: Value = http.post_files(&format!("/channels/{forum}/threads"), &body, &files).await?;
                let message = serde_json::from_value(v["message"].clone()).ok();
                let channel = serde_json::from_value(v).map_err(|e| HttpError::Decode(e.to_string()))?;
                Ok((channel, message))
            })
            .await;
        self.watch(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_asks_for_what_the_forum_shows() {
        let search = ThreadSearch { by_creation: true, tags: vec![Id(5), Id(6)], offset: 25, ..Default::default() };
        assert_eq!(
            search.path(Id(9)),
            "/channels/9/threads/search?archived=true&sort_by=creation_time&sort_order=desc&limit=25&tag_setting=match_some&offset=25&tag=5&tag=6"
        );
        assert!(ThreadSearch::default().path(Id(9)).contains("sort_by=last_message_time"));
    }

    #[test]
    fn a_page_reads_posts_first_messages_and_membership() {
        let page = ThreadPage::from_json(&json!({
            "threads": [{"id": "20", "type": 11, "parent_id": "9", "name": "a post"}, {"nonsense": true}],
            "first_messages": [{"id": "20", "channel_id": "20", "timestamp": "t", "author": {"id": "1", "username": "x"}, "content": "hi"}],
            "members": [{"id": "20", "user_id": "1"}],
            "has_more": true,
        }));
        assert_eq!(page.threads.len(), 1);
        assert_eq!(page.first_messages[0].content, "hi");
        assert_eq!(page.joined, vec![Id(20)]);
        assert!(page.has_more);
    }

    #[test]
    fn a_post_carries_its_tags_and_first_message() {
        let body = NewPost { title: "Help".into(), content: "text".into(), tags: vec![Id(3)] }.body();
        assert_eq!(body["name"], "Help");
        assert_eq!(body["applied_tags"][0], "3");
        assert_eq!(body["message"]["content"], "text");
    }
}
