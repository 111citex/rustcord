//! Message search, as Discord's own client asks for it: one request per page of
//! 25 results, filtered by who wrote them, what they hold, where and when.

use serde_json::Value;

use crate::Id;
use crate::model::{Message, lenient_vec};

/// Results come 25 to a page.
pub const PAGE: u32 = 25;

/// What a message must hold to match `has:`.
pub const HAS: [&str; 9] = ["link", "embed", "file", "video", "image", "sound", "sticker", "poll", "forward"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    Newest,
    Oldest,
    /// Discord's "Most Relevant".
    Relevant,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    pub content: String,
    pub authors: Vec<Id>,
    pub mentions: Vec<Id>,
    pub has: Vec<String>,
    /// In a server: only these channels.
    pub channels: Vec<Id>,
    /// Snowflakes standing for times: only messages before or after them.
    pub before: Option<Id>,
    pub after: Option<Id>,
    pub pinned: bool,
    /// `user`, `bot` or `webhook`.
    pub author_type: Vec<String>,
    pub sort: Sort,
    pub offset: u32,
}

/// Percent-encodes a query parameter.
fn encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

impl Query {
    /// Nothing to search for: Discord refuses an empty search.
    pub fn is_empty(&self) -> bool {
        self.content.trim().is_empty()
            && self.authors.is_empty()
            && self.mentions.is_empty()
            && self.has.is_empty()
            && self.channels.is_empty()
            && self.before.is_none()
            && self.after.is_none()
            && !self.pinned
            && self.author_type.is_empty()
    }

    /// The URL's query string.
    pub fn query_string(&self) -> String {
        let mut parts = Vec::new();
        let content = self.content.trim();
        if !content.is_empty() {
            parts.push(format!("content={}", encode(content)));
        }
        parts.extend(self.authors.iter().map(|a| format!("author_id={a}")));
        parts.extend(self.mentions.iter().map(|m| format!("mentions={m}")));
        parts.extend(self.has.iter().map(|h| format!("has={}", encode(h))));
        parts.extend(self.channels.iter().map(|c| format!("channel_id={c}")));
        parts.extend(self.author_type.iter().map(|t| format!("author_type={}", encode(t))));
        if let Some(before) = self.before {
            parts.push(format!("max_id={before}"));
        }
        if let Some(after) = self.after {
            parts.push(format!("min_id={after}"));
        }
        if self.pinned {
            parts.push("pinned=true".into());
        }
        let (by, order) = match self.sort {
            Sort::Newest => ("timestamp", "desc"),
            Sort::Oldest => ("timestamp", "asc"),
            Sort::Relevant => ("relevance", "desc"),
        };
        parts.push(format!("sort_by={by}&sort_order={order}"));
        if self.offset > 0 {
            parts.push(format!("offset={}", self.offset));
        }
        parts.join("&")
    }
}

/// One page of results.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Page {
    pub total: u32,
    pub messages: Vec<Message>,
}

/// What a search answered.
#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    Page(Page),
    /// The place isn't indexed yet: ask again after this many seconds.
    Indexing(f32),
}

/// Reads a search response. Each hit comes as a list holding the message (once
/// with the messages around it too); the hit is the one marked so, or the only one.
pub fn read_answer(body: Value) -> Answer {
    if body.get("messages").is_none() {
        let wait = body["retry_after"].as_f64().unwrap_or(2.) as f32;
        return Answer::Indexing(wait.clamp(1., 30.));
    }
    let total = body["total_results"].as_u64().unwrap_or(0) as u32;
    let mut messages = Vec::new();
    for group in body["messages"].as_array().into_iter().flatten() {
        let group = group.as_array().cloned().unwrap_or_else(|| vec![group.clone()]);
        let hit = group.iter().find(|m| m["hit"] == true).or_else(|| group.first()).cloned();
        if let Some(Ok(message)) = hit.map(serde_json::from_value::<Message>) {
            messages.push(message);
        }
    }
    Answer::Page(Page { total, messages })
}

/// Reads the pinned messages, from the newer `{items: [{message}]}` answer or the
/// older plain list.
pub fn read_pins(body: Value) -> Vec<Message> {
    let list = match body.get("items") {
        Some(items) => items.as_array().into_iter().flatten().map(|i| i["message"].clone()).collect(),
        None => body,
    };
    lenient_vec(list).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(id: u64) -> Value {
        json!({"id": id.to_string(), "channel_id": "5", "author": {"id": "7", "username": "kit"}, "content": "hi", "timestamp": "2026-01-01T00:00:00+00:00"})
    }

    #[test]
    fn builds_the_query_string() {
        let query = Query {
            content: " cats & dogs ".into(),
            authors: vec![Id(7)],
            has: vec!["image".into()],
            channels: vec![Id(5)],
            before: Some(Id(100)),
            pinned: true,
            sort: Sort::Oldest,
            offset: 25,
            ..Default::default()
        };
        assert_eq!(
            query.query_string(),
            "content=cats%20%26%20dogs&author_id=7&has=image&channel_id=5&max_id=100&pinned=true&sort_by=timestamp&sort_order=asc&offset=25"
        );
        assert!(Query::default().is_empty());
        assert!(!query.is_empty());
        assert_eq!(Query { content: "é".into(), ..Default::default() }.query_string(), "content=%C3%A9&sort_by=timestamp&sort_order=desc");
    }

    #[test]
    fn reads_hits_and_waits_for_indexing() {
        let body = json!({"total_results": 2, "messages": [[message(1)], [message(2), {"bad": 1}]]});
        let Answer::Page(page) = read_answer(body) else { panic!("a page") };
        assert_eq!(page.total, 2);
        assert_eq!(page.messages.iter().map(|m| m.id).collect::<Vec<_>>(), vec![Id(1), Id(2)]);
        let mut hit = message(9);
        hit["hit"] = json!(true);
        let Answer::Page(page) = read_answer(json!({"total_results": 1, "messages": [[message(8), hit]]})) else { panic!() };
        assert_eq!(page.messages[0].id, Id(9), "the hit, not its neighbour");
        assert_eq!(read_answer(json!({"message": "Index not yet available", "retry_after": 3})), Answer::Indexing(3.));
    }

    #[test]
    fn reads_both_shapes_of_pins() {
        assert_eq!(read_pins(json!({"items": [{"pinned_at": "x", "message": message(3)}], "has_more": false})).len(), 1);
        assert_eq!(read_pins(json!([message(3), message(4)])).len(), 2);
        assert!(read_pins(json!({"items": []})).is_empty());
    }
}
