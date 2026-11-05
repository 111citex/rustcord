//! `:jo` → 😂: the emoji the composer offers while a `:shortcode` is being
//! typed, and the `@name` being typed for a mention.

use unicode_segmentation::UnicodeSegmentation;

/// The `:query` ending at `cursor`: where its colon is, and the text after
/// it. Needs two characters, as Discord does, and the colon must start a word.
pub fn query(text: &str, cursor: usize) -> Option<(usize, &str)> {
    let before = text.get(..cursor)?;
    let colon = before.rfind(':')?;
    let q = &before[colon + 1..];
    let word_start = before[..colon].chars().next_back().is_none_or(char::is_whitespace);
    let valid = q.len() >= 2 && q.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+'));
    (word_start && valid).then_some((colon, q))
}

/// The `@query` ending at `cursor`: where its `@` is, and the name typed
/// after it (possibly nothing yet, as Discord opens the list on a bare `@`).
pub fn mention_query(text: &str, cursor: usize) -> Option<(usize, &str)> {
    let before = text.get(..cursor)?;
    let at = before.rfind('@')?;
    let q = &before[at + 1..];
    let word_start = before[..at].chars().next_back().is_none_or(char::is_whitespace);
    // Spaces are allowed: names have them. If nobody matches, no popup shows.
    let valid = q.len() <= 32 && !q.chars().any(|c| matches!(c, '\n' | '@' | '<' | '>' | ':'));
    (word_start && valid).then_some((at, q))
}

/// Up to `limit` emoji whose shortcode matches `q`: prefix matches first,
/// then ones that merely contain it, shorter names first.
pub fn suggestions(q: &str, limit: usize) -> Vec<(&'static str, &'static str)> {
    let q = q.to_ascii_lowercase();
    let mut found: Vec<(u8, &'static str, &'static str)> = Vec::new();
    for emoji in emojis::iter() {
        let best = emoji
            .shortcodes()
            .filter_map(|code| {
                if code.starts_with(&q) {
                    Some((0, code))
                } else if code.contains(&q) {
                    Some((1, code))
                } else {
                    None
                }
            })
            .min_by_key(|(rank, code)| (*rank, code.len()));
        if let Some((rank, code)) = best {
            found.push((rank, emoji.as_str(), code));
        }
    }
    found.sort_by_key(|(rank, _, code)| (*rank, code.len(), *code));
    found.into_iter().take(limit).map(|(_, e, code)| (e, code)).collect()
}

/// A whole `:joy:` just typed, ending at `cursor`: where it starts and its emoji.
pub fn finished_shortcode(text: &str, cursor: usize) -> Option<(usize, &'static str)> {
    let before = text.get(..cursor)?.strip_suffix(':')?;
    let start = before.rfind(':')?;
    let emoji = emojis::get_by_shortcode(&before[start + 1..])?;
    Some((start, emoji.as_str()))
}

/// The first emoji in `text` outside `skip`, typed or pasted as a character.
pub fn raw_emoji(text: &str, skip: &[std::ops::Range<usize>]) -> Option<std::ops::Range<usize>> {
    text.grapheme_indices(true)
        .filter(|(_, g)| !g.is_ascii() && emojis::get(g).is_some())
        .map(|(at, g)| at..at + g.len())
        .find(|range| !skip.iter().any(|s| s.start < range.end && range.start < s.end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_query_at_the_cursor() {
        assert_eq!(query("hi :jo", 6), Some((3, "jo")));
        assert_eq!(query(":jo", 3), Some((0, "jo")));
        // One letter isn't enough; a colon inside a word isn't a query.
        assert_eq!(query("hi :j", 5), None);
        assert_eq!(query("12:30", 5), None);
        assert_eq!(query("hi :jo there", 12), None);
    }

    #[test]
    fn finds_the_mention_at_the_cursor() {
        assert_eq!(mention_query("hi @", 4), Some((3, "")));
        assert_eq!(mention_query("hi @ve", 6), Some((3, "ve")));
        assert_eq!(mention_query("mail a@b", 8), None);
        assert_eq!(mention_query("@amy's ho", 9), Some((0, "amy's ho")));
        assert_eq!(mention_query("@ada\nhi", 7), None);
    }

    #[test]
    fn prefix_matches_come_first() {
        let found = suggestions("jo", 8);
        assert_eq!(found.first().map(|(e, _)| *e), Some("😂"));
        assert!(found.iter().all(|(_, code)| code.contains("jo")));
        assert!(found.len() <= 8);
    }

    #[test]
    fn whole_shortcodes_are_found() {
        assert_eq!(finished_shortcode("lol :joy:", 9), Some((4, "😂")));
        assert_eq!(finished_shortcode(":joy", 4), None);
        assert_eq!(finished_shortcode("12:30:", 6), None);
    }

    #[test]
    fn raw_emoji_skips_what_is_already_a_token() {
        assert_eq!(raw_emoji("hi 😂 ok", &[]), Some(3..7));
        assert_eq!(raw_emoji("hi 😂 ok", &[3..7]), None);
        assert_eq!(raw_emoji("plain #1", &[]), None);
    }
}
