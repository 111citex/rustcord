//! Splits message content into what the chat draws differently: plain text,
//! Unicode emoji (drawn as Apple's artwork), server custom emoji and
//! user mentions. Full markdown arrives in plan 1b.

use cord_core::Id;
use unicode_segmentation::UnicodeSegmentation;

/// Apple's emoji artwork, from the `emoji-datasource-apple` package.
const EMOJI_BASE: &str = "https://cdn.jsdelivr.net/npm/emoji-datasource-apple@16.0.0/img/apple/64";
/// Discord shows a message made only of up to this many emoji at large size.
const JUMBO_MAX: usize = 27;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    Emoji { grapheme: String, url: String },
    CustomEmoji { name: String, url: String },
    Mention(Id),
}

/// What a message shows: Discord renders content without the blank lines
/// and spaces around it (raw content often ends in a newline).
pub fn display_text(content: &str) -> &str {
    content.trim()
}

pub fn segments(content: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut pending = String::new();
    let mut rest = content;
    while let Some(ch) = rest.chars().next() {
        if ch == '<' {
            if let Some((segment, len)) = parse_token(rest) {
                flush_text(&mut pending, &mut out);
                out.push(segment);
                rest = &rest[len..];
                continue;
            }
        }
        pending.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    flush_text(&mut pending, &mut out);
    out
}

/// `<@id>`, `<@!id>`, `<:name:id>` or `<a:name:id>` at the start of `s`,
/// with the number of bytes it spans.
fn parse_token(s: &str) -> Option<(Segment, usize)> {
    let end = s.find('>')?;
    let inner = &s[1..end];
    let is_id = |v: &str| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit());
    if let Some(user) = inner.strip_prefix('@') {
        let user = user.strip_prefix('!').unwrap_or(user);
        let id: u64 = if is_id(user) { user.parse().ok()? } else { return None };
        return Some((Segment::Mention(Id(id)), end + 1));
    }
    let animated = inner.starts_with("a:");
    let emoji = inner.strip_prefix('a').unwrap_or(inner).strip_prefix(':')?;
    let (name, id) = emoji.split_once(':')?;
    (!name.is_empty() && is_id(id)).then(|| {
        let url = custom_emoji_url(id, animated);
        (Segment::CustomEmoji { name: name.to_string(), url }, end + 1)
    })
}

/// A custom emoji's picture: a GIF when animated (shown as its first frame
/// unless something animates it), a PNG otherwise.
pub fn custom_emoji_url(id: impl std::fmt::Display, animated: bool) -> String {
    format!("https://cdn.discordapp.com/emojis/{id}.{}?size=48", if animated { "gif" } else { "png" })
}

/// What stands in for an emoji whose picture can't load: `:name:` for a
/// custom one, the name itself for unicode, and a placeholder when it has none.
pub fn emoji_fallback(name: &str, custom: bool) -> String {
    match (name.is_empty(), custom) {
        (true, _) => "(emoji)".to_string(),
        (false, true) => format!(":{name}:"),
        (false, false) => name.to_string(),
    }
}

/// An emoji newer than the table of them: the blocks Unicode keeps adding to.
fn is_newer_emoji(grapheme: &str) -> bool {
    grapheme.chars().next().is_some_and(|c| matches!(c as u32, 0x1FA70..=0x1FAFF | 0x1F6D5..=0x1F6DF | 0x1F7E0..=0x1F7F0))
}

/// Moves `text` into `out`, cutting out the emoji graphemes.
fn flush_text(text: &mut String, out: &mut Vec<Segment>) {
    let mut run = String::new();
    for grapheme in text.graphemes(true) {
        if emojis::get(grapheme).is_some() || is_newer_emoji(grapheme) {
            if !run.is_empty() {
                out.push(Segment::Text(std::mem::take(&mut run)));
            }
            out.push(Segment::Emoji { grapheme: grapheme.to_string(), url: emoji_url(grapheme) });
        } else {
            run.push_str(grapheme);
        }
    }
    if !run.is_empty() {
        out.push(Segment::Text(run));
    }
    text.clear();
}

/// The package names files by the code points of the fully qualified emoji,
/// in hex of at least four digits: `2764-fe0f.png`, `0023-fe0f-20e3.png`.
pub fn emoji_url(grapheme: &str) -> String {
    let qualified = emojis::get(grapheme).map_or(grapheme, |e| e.as_str());
    let code: Vec<String> = qualified.chars().map(|c| format!("{:04x}", c as u32)).collect();
    format!("{EMOJI_BASE}/{}.png", code.join("-"))
}

/// Only emoji (and spaces), at most [`JUMBO_MAX`] of them.
pub fn is_jumbo(segments: &[Segment]) -> bool {
    let mut count = 0;
    for segment in segments {
        match segment {
            Segment::Emoji { .. } | Segment::CustomEmoji { .. } => count += 1,
            Segment::Text(t) if t.trim().is_empty() => {}
            _ => return false,
        }
    }
    (1..=JUMBO_MAX).contains(&count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Segment {
        Segment::Text(s.into())
    }

    #[test]
    fn surrounding_blank_lines_are_not_shown() {
        // Discord renders messages trimmed; raw content often ends in a newline.
        assert_eq!(display_text("1% OVERTHINKKING\n"), "1% OVERTHINKKING");
        assert_eq!(display_text("\n\n  hi there \r\n\n"), "hi there");
        assert_eq!(display_text("line one\n\nline three"), "line one\n\nline three");
    }

    #[test]
    fn plain_text_is_one_segment() {
        assert_eq!(segments("hello there"), [text("hello there")]);
        assert_eq!(segments(""), []);
    }

    #[test]
    fn unicode_emoji_become_twemoji() {
        let s = segments("hi 👋!");
        assert_eq!(s.len(), 3);
        assert_eq!(s[0], text("hi "));
        assert_eq!(s[1], Segment::Emoji { grapheme: "👋".into(), url: emoji_url("👋") });
        assert_eq!(s[2], text("!"));
    }

    #[test]
    fn emoji_file_names() {
        let file = |g: &str| emoji_url(g).rsplit('/').next().unwrap().to_string();
        assert_eq!(file("👋"), "1f44b.png");
        // Fully qualified, whichever form was typed.
        assert_eq!(file("❤️"), "2764-fe0f.png");
        assert_eq!(file("\u{2764}"), "2764-fe0f.png");
        assert_eq!(file("1️⃣"), "0031-fe0f-20e3.png");
        assert_eq!(file("👍🏽"), "1f44d-1f3fd.png");
        assert_eq!(file("👨‍👩‍👧"), "1f468-200d-1f469-200d-1f467.png");
        assert_eq!(file("🏳️‍🌈"), "1f3f3-fe0f-200d-1f308.png");
    }

    #[test]
    fn mentions_and_custom_emoji() {
        assert_eq!(
            segments("hey <@123> and <@!456>"),
            [text("hey "), Segment::Mention(Id(123)), text(" and "), Segment::Mention(Id(456))]
        );
        assert_eq!(
            segments("<:pog:789> <a:dance:790>"),
            [
                Segment::CustomEmoji { name: "pog".into(), url: "https://cdn.discordapp.com/emojis/789.png?size=48".into() },
                text(" "),
                Segment::CustomEmoji { name: "dance".into(), url: "https://cdn.discordapp.com/emojis/790.gif?size=48".into() },
            ]
        );
    }

    #[test]
    fn custom_emoji_urls_and_fallbacks() {
        assert_eq!(custom_emoji_url(9, false), "https://cdn.discordapp.com/emojis/9.png?size=48");
        assert_eq!(custom_emoji_url(Id(9), true), "https://cdn.discordapp.com/emojis/9.gif?size=48");
        assert_eq!(emoji_fallback("pog", true), ":pog:");
        assert_eq!(emoji_fallback("\u{2705}", false), "\u{2705}");
        assert_eq!(emoji_fallback("", true), "(emoji)");
        assert_eq!(emoji_fallback("", false), "(emoji)");
    }

    #[test]
    fn malformed_tokens_stay_text() {
        assert_eq!(segments("a < b <@x> <:no:> <@>"), [text("a < b <@x> <:no:> <@>")]);
    }

    #[test]
    fn jumbo_only_for_emoji_only_messages() {
        assert!(is_jumbo(&segments("👋")));
        assert!(is_jumbo(&segments("👋 🎉 <:pog:789>")));
        assert!(!is_jumbo(&segments("hi 👋")));
        assert!(!is_jumbo(&segments("")));
        assert!(!is_jumbo(&segments(&"👋".repeat(28))));
    }
}

/// `:thumbsup:` as the emoji it names, as cards show them: bots write them that way.
/// Custom emoji (`<:name:123>`) and names nobody knows stay as they are.
pub fn with_shortcodes(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains(':') {
        return text.into();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut changed = false;
    while let Some(start) = rest.find(':') {
        let after = &rest[start + 1..];
        let found = after.find(':').filter(|end| {
            !after[..*end].is_empty()
                && after[..*end].chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '+')
                && !rest[..start].ends_with(['<', 'a'])
        });
        match found.and_then(|end| Some((end, emojis::get_by_shortcode(&after[..end])?))) {
            Some((end, emoji)) => {
                out.push_str(&rest[..start]);
                out.push_str(emoji.as_str());
                rest = &after[end + 1..];
                changed = true;
            }
            None => {
                out.push_str(&rest[..=start]);
                rest = after;
            }
        }
    }
    if !changed {
        return text.into();
    }
    out.push_str(rest);
    out.into()
}

#[cfg(test)]
mod shortcode_tests {
    use super::with_shortcodes;

    #[test]
    fn named_emoji_become_emoji() {
        assert_eq!(with_shortcodes("Bump done! :thumbsup:"), "Bump done! \u{1f44d}");
        assert_eq!(with_shortcodes("no such :thing: here"), "no such :thing: here");
        assert_eq!(with_shortcodes("at 12:30:45 sharp"), "at 12:30:45 sharp");
        assert_eq!(with_shortcodes("<:thumbsup:123>"), "<:thumbsup:123>");
    }
}
