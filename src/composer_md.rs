//! The message box's live markdown: the marks stay visible (dimmed) and what
//! they wrap takes its style as you type, as in Discord's composer.

use std::ops::Range;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Look {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub code: bool,
    pub spoiler: bool,
    /// A mark itself: `**`, `_`…
    pub mark: bool,
}

/// Delimiters, longest first so `**` isn't read as two `*`.
const MARKS: [&str; 7] = ["**", "__", "~~", "||", "`", "*", "_"];

fn apply(mark: &str, look: Look) -> Look {
    match mark {
        "**" => Look { bold: true, ..look },
        "__" => Look { underline: true, ..look },
        "~~" => Look { strike: true, ..look },
        "||" => Look { spoiler: true, ..look },
        "`" => Look { code: true, ..look },
        _ => Look { italic: true, ..look },
    }
}

/// `text`'s styled stretches, sorted and not overlapping. Plain text has none.
pub fn styles(text: &str) -> Vec<(Range<usize>, Look)> {
    let mut out = Vec::new();
    scan(text, 0, Look::default(), &mut out);
    out
}

fn scan(text: &str, base: usize, look: Look, out: &mut Vec<(Range<usize>, Look)>) {
    let mut at = 0;
    let mut plain = 0;
    while at < text.len() {
        let rest = &text[at..];
        let opened = MARKS.iter().find(|m| rest.starts_with(**m)).and_then(|mark| {
            // The closing mark, with something between them.
            let inner = &rest[mark.len()..];
            let end = inner.find(mark)?;
            // Inside a word, `_` is just a letter, as in `snake_case_name`.
            if *mark == "_" {
                let before = text[..at].chars().next_back();
                let after = inner[end + mark.len()..].chars().next();
                if before.is_some_and(char::is_alphanumeric) || after.is_some_and(char::is_alphanumeric) {
                    return None;
                }
            }
            (end > 0 && !inner[..end].starts_with(char::is_whitespace)).then_some((*mark, end))
        });
        let Some((mark, end)) = opened else {
            at += rest.chars().next().map_or(1, char::len_utf8);
            continue;
        };
        if plain < at && look != Look::default() {
            out.push((base + plain..base + at, look));
        }
        let (open, close) = (at + mark.len(), at + mark.len() + end);
        out.push((base + at..base + open, Look { mark: true, ..look }));
        let inner = apply(mark, look);
        if mark == "`" {
            out.push((base + open..base + close, inner));
        } else {
            scan(&text[open..close], base + open, inner, out);
        }
        out.push((base + close..base + close + mark.len(), Look { mark: true, ..look }));
        at = close + mark.len();
        plain = at;
    }
    if plain < text.len() && look != Look::default() {
        out.push((base + plain..base + text.len(), look));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_and_contents_are_styled_apart() {
        let s = styles("a **b** c");
        assert_eq!(s.len(), 3);
        assert_eq!(s[0], (2..4, Look { mark: true, ..Look::default() }));
        assert_eq!(s[1], (4..5, Look { bold: true, ..Look::default() }));
        assert_eq!(s[2].0, 5..7);
    }

    #[test]
    fn nesting_adds_up_and_ranges_stay_sorted() {
        let s = styles("**a _b_ c**");
        assert!(s.iter().any(|(r, l)| *r == (5..6) && l.bold && l.italic));
        assert!(s.windows(2).all(|w| w[0].0.end <= w[1].0.start));
    }

    #[test]
    fn unfinished_marks_and_plain_text_are_left_alone() {
        assert!(styles("just text").is_empty());
        assert!(styles("**open").is_empty());
        assert!(styles("snake_case and 2 * 3").is_empty());
        assert!(styles("#men_in_women_field").is_empty(), "underscores inside a word");
        assert!(styles("snake_case_name").is_empty());
        assert!(styles("an _italic_ word").iter().any(|(_, l)| l.italic), "around a word still counts");
    }

    #[test]
    fn code_keeps_marks_inside_literal() {
        let s = styles("`**x**`");
        assert_eq!(s.len(), 3);
        assert!(s[1].1.code && !s[1].1.bold);
    }
}
