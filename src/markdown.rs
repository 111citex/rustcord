//! Discord's markdown subset as a small AST: blocks (paragraphs, headings,
//! quotes, code blocks, lists) holding styled inline runs. Pure; drawing is
//! in `markdown_view`. Emoji and user mentions come from `message_text`,
//! whose tokenizer runs over every plain stretch.
//!
//! Where Discord's own parser is vague this one errs on showing the text
//! literally: a delimiter with no closing partner is just a character.

use chrono::{DateTime, TimeZone, Timelike};
use cord_core::Id;

use crate::message_text::{self, Segment};

/// What the text of a run looks like. `spoiler` is the group it belongs to:
/// runs sharing a group are revealed together by one click.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub code: bool,
    pub spoiler: Option<u16>,
    pub link: Option<String>,
    /// Secondary text, like the "(edited)" mark.
    pub muted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    Text(String),
    Emoji { grapheme: String, url: String },
    CustomEmoji { name: String, url: String },
    User(Id),
    Role(Id),
    Channel(Id),
    /// A bare link to a message: drawn as where it is, not as the address.
    MessageLink { channel: Id, url: String },
    /// `@everyone` or `@here`: a pill when the message really pinged, else plain text.
    Everyone(&'static str),
    /// `</name sub:id>`: a slash command, shown as "/name sub".
    Command(String),
    /// `<t:unix:style>`; `style` is one of `tTdDfFRsS` (`f` when absent).
    Timestamp { unix: i64, style: char },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub content: Content,
    pub style: Style,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListItem {
    /// Nesting level, 0 for top level.
    pub depth: u8,
    /// `Some` for numbered items.
    pub number: Option<u64>,
    pub runs: Vec<Run>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Paragraph(Vec<Run>),
    Heading { level: u8, runs: Vec<Run> },
    /// `-# small text`.
    Subtext(Vec<Run>),
    Quote(Vec<Block>),
    Code { lang: Option<String>, text: String },
    List(Vec<ListItem>),
}

/// How deep quotes nest. Deeper `>` stay literal text: nothing legitimate
/// needs more, and each level costs a recursion and a nested element.
const MAX_QUOTE_DEPTH: u8 = 3;

struct Parser {
    next_spoiler: u16,
    /// Quotes the text being parsed is already inside.
    depth: u8,
}

/// Parses a whole message. Surrounding blank space is dropped, as Discord does.
pub fn parse(content: &str) -> Vec<Block> {
    Parser { next_spoiler: 0, depth: 0 }.blocks(message_text::display_text(content))
}

/// `blocks` with " (edited)" appended, muted, to the last line, as Discord
/// shows it. `None` when the text ends in something that has no last line to
/// continue (code, a quote or a list), or there is no text.
pub fn with_edited_mark(blocks: &[Block]) -> Option<Vec<Block>> {
    let mark = Run { content: Content::Text(" (edited)".into()), style: Style { muted: true, ..Style::default() } };
    let mut out = blocks.to_vec();
    match out.last_mut()? {
        Block::Paragraph(runs) | Block::Subtext(runs) | Block::Heading { runs, .. } => runs.push(mark),
        _ => return None,
    }
    Some(out)
}

/// Whether the text's last block is short enough to carry "(edited)" on its line.
pub fn last_line_is_short(blocks: &[Block]) -> bool {
    const SHORT: usize = 90;
    match blocks.last() {
        Some(Block::Paragraph(runs) | Block::Subtext(runs) | Block::Heading { runs, .. }) => {
            runs.iter().map(|r| if let Content::Text(t) = &r.content { t.len() } else { 12 }).sum::<usize>() <= SHORT
        }
        _ => false,
    }
}

/// A line without what makes it a block (`# `, `-# `, `> `, `- `): a preview keeps
/// its bold and colours, but isn't a heading or a quote.
pub fn preview_line(line: &str) -> &str {
    let mut line = line.trim_start();
    loop {
        let hashes = line.bytes().take_while(|b| *b == b'#').count();
        let rest = if (1..=3).contains(&hashes) {
            line[hashes..].strip_prefix(' ')
        } else {
            ["-# ", ">>> ", "> ", "- ", "* "].iter().find_map(|mark| line.strip_prefix(mark))
        };
        match rest {
            Some(rest) => line = rest.trim_start(),
            None => return line,
        }
    }
}

/// A `<` from `\\<`, while the text goes through the mention and emoji tokenizer.
const ESCAPED_ANGLE: char = '\u{F8FE}';

/// Inline-only parse, for single lines such as a reply preview.
pub fn parse_inline(text: &str) -> Vec<Run> {
    let mut out = Vec::new();
    Parser { next_spoiler: 0, depth: 0 }.inline(text, &Style::default(), &mut out);
    out
}

/// A message made only of emoji (and spaces) is drawn large.
pub fn is_jumbo(blocks: &[Block]) -> bool {
    let [Block::Paragraph(runs)] = blocks else { return false };
    let segments: Vec<Segment> = runs
        .iter()
        .map(|r| match &r.content {
            Content::Emoji { grapheme, url } => Segment::Emoji { grapheme: grapheme.clone(), url: url.clone() },
            Content::CustomEmoji { name, url } => Segment::CustomEmoji { name: name.clone(), url: url.clone() },
            Content::Text(t) => Segment::Text(t.clone()),
            _ => Segment::Mention(Id(0)),
        })
        .collect();
    message_text::is_jumbo(&segments)
}

impl Parser {
    fn blocks(&mut self, text: &str) -> Vec<Block> {
        let mut out = Vec::new();
        let mut rest = text;
        loop {
            let Some(open) = find_fence(rest, 0) else {
                self.lines(rest, &mut out);
                return out;
            };
            let Some(close) = find_fence(rest, open + 3) else {
                // No closing fence: Discord shows it literally.
                self.lines(rest, &mut out);
                return out;
            };
            self.lines(&rest[..open], &mut out);
            out.push(code_block(&rest[open + 3..close]));
            rest = &rest[close + 3..];
        }
    }

    /// Everything that isn't a fenced code block, line by line.
    fn lines(&mut self, text: &str, out: &mut Vec<Block>) {
        let mut paragraph: Vec<&str> = Vec::new();
        let mut quote: Vec<&str> = Vec::new();
        let mut list: Vec<ListItem> = Vec::new();
        let mut lines = text.split('\n').peekable();
        let quotes = self.depth < MAX_QUOTE_DEPTH;
        while let Some(line) = lines.next() {
            if quotes && (line == ">>>" || line.starts_with(">>> ")) {
                self.flush(&mut paragraph, &mut quote, &mut list, out);
                let mut inner = vec![line.strip_prefix(">>>").unwrap_or("").trim_start()];
                inner.extend(lines.by_ref());
                out.push(Block::Quote(self.quoted(inner.join("\n").trim())));
                return;
            }
            if let Some(q) = line.strip_prefix("> ").or((line == ">").then_some("")).filter(|_| quotes) {
                self.flush_text_and_list(&mut paragraph, &mut list, out);
                quote.push(q);
                continue;
            }
            if let Some((level, rest)) = heading(line) {
                self.flush(&mut paragraph, &mut quote, &mut list, out);
                out.push(Block::Heading { level, runs: self.runs(rest) });
                continue;
            }
            if let Some(rest) = line.strip_prefix("-# ") {
                self.flush(&mut paragraph, &mut quote, &mut list, out);
                out.push(Block::Subtext(self.runs(rest)));
                continue;
            }
            if let Some((depth, number, rest)) = list_item(line) {
                self.flush_quote(&mut quote, out);
                self.flush_paragraph(&mut paragraph, out);
                list.push(ListItem { depth, number, runs: self.runs(rest) });
                continue;
            }
            self.flush_quote(&mut quote, out);
            self.flush_list(&mut list, out);
            paragraph.push(line);
        }
        self.flush(&mut paragraph, &mut quote, &mut list, out);
    }

    fn flush(&mut self, paragraph: &mut Vec<&str>, quote: &mut Vec<&str>, list: &mut Vec<ListItem>, out: &mut Vec<Block>) {
        self.flush_quote(quote, out);
        self.flush_text_and_list(paragraph, list, out);
    }

    fn flush_text_and_list(&mut self, paragraph: &mut Vec<&str>, list: &mut Vec<ListItem>, out: &mut Vec<Block>) {
        self.flush_paragraph(paragraph, out);
        self.flush_list(list, out);
    }

    fn flush_paragraph(&mut self, lines: &mut Vec<&str>, out: &mut Vec<Block>) {
        let text = lines.join("\n");
        lines.clear();
        let text = text.trim_matches('\n');
        if !text.is_empty() {
            out.push(Block::Paragraph(self.runs(text)));
        }
    }

    fn flush_quote(&mut self, lines: &mut Vec<&str>, out: &mut Vec<Block>) {
        if lines.is_empty() {
            return;
        }
        let inner = self.quoted(&lines.join("\n"));
        lines.clear();
        out.push(Block::Quote(inner));
    }

    /// The blocks inside a quote, one level deeper.
    fn quoted(&mut self, text: &str) -> Vec<Block> {
        self.depth += 1;
        let inner = self.blocks(text);
        self.depth -= 1;
        inner
    }

    fn flush_list(&mut self, items: &mut Vec<ListItem>, out: &mut Vec<Block>) {
        if !items.is_empty() {
            out.push(Block::List(std::mem::take(items)));
        }
    }

    fn runs(&mut self, text: &str) -> Vec<Run> {
        let mut out = Vec::new();
        self.inline(text, &Style::default(), &mut out);
        out
    }

    /// Appends the runs for `s` under `style`, merging nothing: adjacent text
    /// of the same style stays as the pieces the delimiters cut it into.
    fn inline(&mut self, s: &str, style: &Style, out: &mut Vec<Run>) {
        let mut plain = String::new();
        let mut i = 0;
        while i < s.len() {
            let prev = s[..i].chars().next_back();
            if let Some((runs, len)) = self.construct(&s[i..], prev, style) {
                flush_plain(&mut plain, style, out);
                out.extend(runs);
                i += len;
                continue;
            }
            // Escapes and atomic tokens that must reach the tokenizer whole.
            let rest = &s[i..];
            let c = rest.chars().next().unwrap_or(' ');
            if c == '\\' {
                if let Some(next) = rest[1..].chars().next().filter(char::is_ascii_punctuation) {
                    // Stays text even if it would start a mention or an emoji.
                    plain.push(if next == '<' { ESCAPED_ANGLE } else { next });
                    i += 1 + next.len_utf8();
                    continue;
                }
                // An escaped emoji is the plain character: asking for its text form
                // keeps it from turning into a picture.
                if let Some(next) = rest[1..].chars().next().filter(|c| emojis::get(c.encode_utf8(&mut [0; 4])).is_some()) {
                    plain.push(next);
                    plain.push('\u{FE0E}');
                    i += 1 + next.len_utf8();
                    continue;
                }
            }
            if c == '<' {
                if let Some(len) = atomic_token(rest) {
                    plain.push_str(&rest[..len]);
                    i += len;
                    continue;
                }
            }
            plain.push(c);
            i += c.len_utf8();
        }
        flush_plain(&mut plain, style, out);
    }

    /// A construct starting at the beginning of `rest`: code span, spoiler,
    /// emphasis, link, mention or timestamp.
    fn construct(&mut self, rest: &str, prev: Option<char>, style: &Style) -> Option<(Vec<Run>, usize)> {
        let c = rest.chars().next()?;
        match c {
            '`' => code_span(rest, style),
            '|' if rest.starts_with("||") => {
                let close = find_close(rest, 2, "||")?;
                let group = style.spoiler.unwrap_or_else(|| {
                    self.next_spoiler += 1;
                    self.next_spoiler - 1
                });
                let inner = Style { spoiler: Some(group), ..style.clone() };
                Some((self.nested(&rest[2..close], &inner), close + 2))
            }
            '*' | '_' | '~' => self.emphasis(rest, prev, style),
            '<' => self.angle(rest, style),
            '[' => self.masked_link(rest, style),
            'h' if style.link.is_none() && prev.is_none_or(|p| !p.is_alphanumeric()) => {
                let len = bare_url_len(rest)?;
                let url = rest[..len].to_string();
                if let Some((channel, _)) = crate::links::linked_message(&url) {
                    return Some((vec![Run { content: Content::MessageLink { channel, url }, style: style.clone() }], len));
                }
                let linked = Style { link: Some(url.clone()), ..style.clone() };
                Some((vec![Run { content: Content::Text(url), style: linked }], len))
            }
            _ => None,
        }
    }

    fn nested(&mut self, text: &str, style: &Style) -> Vec<Run> {
        let mut out = Vec::new();
        self.inline(text, style, &mut out);
        out
    }

    fn emphasis(&mut self, rest: &str, prev: Option<char>, style: &Style) -> Option<(Vec<Run>, usize)> {
        let tries: [(&str, fn(&mut Style)); 6] = [
            ("***", |s| {
                s.bold = true;
                s.italic = true;
            }),
            ("**", |s| s.bold = true),
            ("__", |s| s.underline = true),
            ("~~", |s| s.strike = true),
            ("*", |s| s.italic = true),
            ("_", |s| s.italic = true),
        ];
        // `_ _`: italic nothing, the usual way to post a blank line. Discord
        // draws no text for it.
        if let Some(after) = rest.strip_prefix('_') {
            let spaces = after.len() - after.trim_start_matches(' ').len();
            if spaces > 0 && after[spaces..].starts_with('_') && !prev.is_some_and(char::is_alphanumeric) {
                return Some((Vec::new(), 1 + spaces + 1));
            }
        }
        for (delim, apply) in tries {
            if !rest.starts_with(delim) {
                continue;
            }
            let single = delim.len() == 1;
            let after = rest[delim.len()..].chars().next()?;
            if after.is_whitespace() || (single && after == delim.chars().next()?) {
                continue;
            }
            // snake_case_names are not italic.
            if delim == "_" && prev.is_some_and(char::is_alphanumeric) {
                continue;
            }
            let Some(close) = find_close(rest, delim.len(), delim) else { continue };
            let mut inner = style.clone();
            apply(&mut inner);
            return Some((self.nested(&rest[delim.len()..close], &inner), close + delim.len()));
        }
        None
    }

    /// `<@&role>`, `<#channel>`, `<t:unix:style>`, `<https://autolink>`.
    fn angle(&mut self, rest: &str, style: &Style) -> Option<(Vec<Run>, usize)> {
        let end = rest.find('>')?;
        let inner = &rest[1..end];
        let run = |content| vec![Run { content, style: style.clone() }];
        if let Some(id) = inner.strip_prefix("@&").and_then(parse_id) {
            return Some((run(Content::Role(id)), end + 1));
        }
        if let Some(id) = inner.strip_prefix('#').and_then(parse_id) {
            return Some((run(Content::Channel(id)), end + 1));
        }
        if let Some((name, id)) = inner.strip_prefix('/').and_then(|c| c.rsplit_once(':'))
            && parse_id(id).is_some()
            && !name.trim().is_empty()
        {
            return Some((run(Content::Command(format!("/{name}"))), end + 1));
        }
        if let Some(spec) = inner.strip_prefix("t:") {
            let (unix, fmt) = spec.split_once(':').unwrap_or((spec, "f"));
            let style_char = fmt.chars().next().filter(|c| "tTdDfFRsS".contains(*c)).filter(|_| fmt.len() == 1)?;
            return Some((run(Content::Timestamp { unix: unix.parse().ok()?, style: style_char }), end + 1));
        }
        if is_url(inner) && style.link.is_none() {
            let linked = Style { link: Some(inner.to_string()), ..style.clone() };
            return Some((vec![Run { content: Content::Text(inner.to_string()), style: linked }], end + 1));
        }
        None
    }

    /// `[label](https://url)`.
    fn masked_link(&mut self, rest: &str, style: &Style) -> Option<(Vec<Run>, usize)> {
        if style.link.is_some() {
            return None;
        }
        let label_end = find_close(rest, 1, "]")?;
        let target = rest[label_end + 1..].strip_prefix('(')?;
        let target_end = target.find(')')?;
        let url = target[..target_end].trim();
        // `[label](<url>)` and `[label](url "title")`.
        let url = url.strip_prefix('<').and_then(|u| u.strip_suffix('>')).unwrap_or(url);
        let url = url.split_whitespace().next()?;
        if !is_url(url) {
            return None;
        }
        let linked = Style { link: Some(url.to_string()), ..style.clone() };
        let consumed = label_end + 1 + 1 + target_end + 1;
        Some((self.nested(&rest[1..label_end], &linked), consumed))
    }
}

/// Moves `plain` into `out`, cutting out emoji and user mentions.
fn flush_plain(plain: &mut String, style: &Style, out: &mut Vec<Run>) {
    for segment in message_text::segments(plain) {
        let content = match segment {
            Segment::Text(t) => {
                push_text(t.replace(ESCAPED_ANGLE, "<"), style, out);
                continue;
            }
            Segment::Emoji { grapheme, url } => Content::Emoji { grapheme, url },
            Segment::CustomEmoji { name, url } => Content::CustomEmoji { name, url },
            Segment::Mention(id) => Content::User(id),
        };
        out.push(Run { content, style: style.clone() });
    }
    plain.clear();
}

/// Text as runs, with `@everyone` and `@here` cut out as their own.
fn push_text(text: String, style: &Style, out: &mut Vec<Run>) {
    let mut rest = text.as_str();
    while let Some((at, word)) = ["@everyone", "@here"].iter().filter_map(|w| Some((rest.find(w)?, *w))).min() {
        if at > 0 {
            out.push(Run { content: Content::Text(rest[..at].to_string()), style: style.clone() });
        }
        out.push(Run { content: Content::Everyone(word), style: style.clone() });
        rest = &rest[at + word.len()..];
    }
    if !rest.is_empty() {
        out.push(Run { content: Content::Text(rest.to_string()), style: style.clone() });
    }
}

fn parse_id(s: &str) -> Option<Id> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok().map(Id))?
}

fn is_url(s: &str) -> bool {
    (s.starts_with("https://") || s.starts_with("http://")) && !s.contains(char::is_whitespace) && s.len() > 8
}

/// `<@123>`, `<@!123>`, `<:name:123>`, `<a:name:123>` at the start of `s`:
/// left to the emoji/mention tokenizer, but never cut up by markdown first.
fn atomic_token(s: &str) -> Option<usize> {
    let end = s.find('>')?;
    let token = &s[..=end];
    matches!(message_text::segments(token).as_slice(), [Segment::Mention(_) | Segment::CustomEmoji { .. }]).then_some(end + 1)
}

/// Where links are in text being typed: bare ones (also inside `<…>`), and the label
/// of a `[label](https://…)`, so the message box can colour them as it's written.
pub fn link_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut found = Vec::new();
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        if rest.starts_with('[')
            && let Some(close) = rest.find("](")
            && !rest[..close].contains('\n')
            && let Some(len) = bare_url_len(&rest[close + 2..])
            && rest[close + 2 + len..].starts_with(')')
        {
            found.push(at + 1..at + close);
            at += close + 2 + len + 1;
            continue;
        }
        let word_start = text[..at].chars().next_back().is_none_or(|c| c.is_whitespace() || matches!(c, '<' | '(' | '"' | '\''));
        if word_start && let Some(mut len) = bare_url_len(rest) {
            // `<https://…>`: the closing mark isn't part of it.
            if text[..at].ends_with('<')
                && let Some(close) = rest[..len].find('>')
            {
                len = close;
            }
            found.push(at..at + len);
            at += len;
            continue;
        }
        at += rest.chars().next().map_or(1, char::len_utf8);
    }
    found
}

/// Length of a bare `http(s)://` link at the start of `s`. Punctuation that
/// ends a sentence, and an unbalanced closing parenthesis, are not part of it.
fn bare_url_len(s: &str) -> Option<usize> {
    if !(s.starts_with("https://") || s.starts_with("http://")) {
        return None;
    }
    let mut end = s.find(|c: char| c.is_whitespace() || c == '<').unwrap_or(s.len());
    loop {
        let url = &s[..end];
        let Some(last) = url.chars().next_back() else { return None };
        let balanced = last != ')' || url.matches('(').count() >= url.matches(')').count();
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"') || !balanced {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    (end > "https://".len()).then_some(end)
}

/// A `` `code` `` span (any run of backticks, closed by an equal run).
fn code_span(rest: &str, style: &Style) -> Option<(Vec<Run>, usize)> {
    let n = rest.bytes().take_while(|&b| b == b'`').count();
    let mut j = n;
    while j < rest.len() {
        if rest.as_bytes()[j] == b'`' {
            let run = rest[j..].bytes().take_while(|&b| b == b'`').count();
            if run == n {
                let inner = &rest[n..j];
                let inner = match inner.strip_prefix(' ').and_then(|i| i.strip_suffix(' ')) {
                    Some(trimmed) if !trimmed.is_empty() => trimmed,
                    _ => inner,
                };
                if inner.is_empty() {
                    return None;
                }
                let code = Style { code: true, ..style.clone() };
                return Some((vec![Run { content: Content::Text(inner.to_string()), style: code }], j + n));
            }
            j += run;
        } else {
            j += 1;
        }
    }
    None
}

/// Byte offset of the first `delim` after `from` that closes a construct
/// opened at the start of `s`: not escaped, not inside a code span, with
/// something between, and for single `*`/`_` not part of a doubled one.
fn find_close(s: &str, from: usize, delim: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let d = delim.as_bytes()[0];
    let mut j = from;
    while j < s.len() {
        let b = bytes[j];
        if b == b'\\' {
            j += 1 + s[j + 1..].chars().next().map_or(0, char::len_utf8);
            continue;
        }
        if b == b'`' && delim != "]" {
            let n = s[j..].bytes().take_while(|&x| x == b'`').count();
            j = match code_span(&s[j..], &Style::default()) {
                Some((_, len)) => j + len,
                None => j + n,
            };
            continue;
        }
        if s[j..].starts_with(delim) {
            let run = s[j..].bytes().take_while(|&x| x == d).count();
            let single = delim.len() == 1 && delim != "]";
            if single && run > 1 {
                j += run;
                continue;
            }
            let before = s[..j].chars().next_back();
            let after = s[j + delim.len()..].chars().next();
            let ok = j > from
                && (!single || before.is_some_and(|c| !c.is_whitespace()))
                && (delim != "_" || after.is_none_or(|c| !c.is_alphanumeric()));
            if ok {
                return Some(j);
            }
            j += delim.len();
            continue;
        }
        j += s[j..].chars().next().map_or(1, char::len_utf8);
    }
    None
}

/// Offset of an unescaped ``` at or after `from`.
fn find_fence(s: &str, from: usize) -> Option<usize> {
    let mut start = from;
    while let Some(at) = s[start..].find("```") {
        let at = start + at;
        if !s[..at].ends_with('\\') {
            return Some(at);
        }
        start = at + 3;
    }
    None
}

/// The text between two fences: an optional language line, then the code.
fn code_block(inner: &str) -> Block {
    if let Some((first, body)) = inner.split_once('\n') {
        let is_lang = !first.is_empty() && first.chars().all(|c| c.is_ascii_alphanumeric() || "+#_-.".contains(c));
        if is_lang {
            return Block::Code { lang: Some(first.to_string()), text: body.trim_end_matches('\n').to_string() };
        }
        if first.trim().is_empty() {
            return Block::Code { lang: None, text: body.trim_end_matches('\n').to_string() };
        }
    }
    Block::Code { lang: None, text: inner.trim_matches('\n').to_string() }
}

/// `# `, `## `, `### ` headers.
fn heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    let text = line[hashes..].strip_prefix(' ')?;
    ((1..=3).contains(&hashes) && !text.trim().is_empty()).then_some((hashes as u8, text))
}

/// `- item`, `* item` or `1. item`, indented two spaces per level.
fn list_item(line: &str) -> Option<(u8, Option<u64>, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let body = &line[indent..];
    let depth = (indent / 2).min(3) as u8;
    if let Some(text) = body.strip_prefix("- ").or_else(|| body.strip_prefix("* ")) {
        return Some((depth, None, text));
    }
    let digits = body.bytes().take_while(u8::is_ascii_digit).count();
    let text = body[digits..].strip_prefix(". ")?;
    (digits > 0 && digits <= 9).then(|| (depth, body[..digits].parse().ok(), text))
}

/// What a timestamp token reads as in en-US, as Discord shows it.
pub fn format_timestamp<Tz: TimeZone>(unix: i64, style: char, now: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let Some(at) = DateTime::from_timestamp(unix, 0).map(|t| t.with_timezone(&now.timezone())) else {
        return "Invalid Date".into();
    };
    let time = |with_seconds: bool| {
        let h = match at.hour() % 12 {
            0 => 12,
            h => h,
        };
        let ampm = if at.hour() < 12 { "AM" } else { "PM" };
        if with_seconds {
            format!("{h}:{:02}:{:02} {ampm}", at.minute(), at.second())
        } else {
            format!("{h}:{:02} {ampm}", at.minute())
        }
    };
    let long_date = at.format("%B %-d, %Y").to_string();
    match style {
        't' => time(false),
        'T' => time(true),
        'd' => at.format("%m/%d/%Y").to_string(),
        'D' => long_date,
        'F' => format!("{}, {long_date} {}", at.format("%A"), time(false)),
        'R' => relative(unix - now.timestamp()),
        's' => format!("{}, {}", at.format("%m/%d/%Y"), time(false)),
        'S' => format!("{}, {}", at.format("%m/%d/%Y"), time(true)),
        _ => format!("{long_date} {}", time(false)),
    }
}

/// "in 3 days" / "2 hours ago" for a signed number of seconds from now.
fn relative(delta: i64) -> String {
    let secs = delta.unsigned_abs();
    let unit = |n: u64, one: &str, many: &str| if n == 1 { one.to_string() } else { format!("{n} {many}") };
    let span = match secs {
        0..=59 => unit(secs, "a second", "seconds"),
        60..=3599 => unit(secs / 60, "a minute", "minutes"),
        3600..=86_399 => unit(secs / 3600, "an hour", "hours"),
        86_400..=2_591_999 => unit(secs / 86_400, "a day", "days"),
        2_592_000..=31_535_999 => unit(secs / 2_592_000, "a month", "months"),
        _ => unit(secs / 31_536_000, "a year", "years"),
    };
    if delta > 0 { format!("in {span}") } else { format!("{span} ago") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn text(s: &str, style: Style) -> Run {
        Run { content: Content::Text(s.into()), style }
    }

    fn plain_run(s: &str) -> Run {
        text(s, Style::default())
    }

    fn bold() -> Style {
        Style { bold: true, ..Style::default() }
    }

    fn italic() -> Style {
        Style { italic: true, ..Style::default() }
    }

    fn one(blocks: Vec<Block>) -> Vec<Run> {
        match blocks.into_iter().next() {
            Some(Block::Paragraph(runs)) => runs,
            other => panic!("not a paragraph: {other:?}"),
        }
    }

    fn para(s: &str) -> Vec<Run> {
        one(parse(s))
    }

    #[test]
    fn edited_mark_continues_the_last_line() {
        let runs = one(with_edited_mark(&parse("hello")).unwrap());
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[1], text(" (edited)", Style { muted: true, ..Style::default() }));
        assert!(with_edited_mark(&parse("```\ncode\n```")).is_none());
        assert!(with_edited_mark(&parse("> quote")).is_none());
        assert!(with_edited_mark(&[]).is_none());
    }

    #[test]
    fn plain_text_is_one_paragraph() {
        assert_eq!(parse("hello there"), [Block::Paragraph(vec![plain_run("hello there")])]);
        assert_eq!(parse(""), []);
        assert_eq!(parse("\n  \n"), []);
    }

    #[test]
    fn emphasis() {
        assert_eq!(para("a **b** c"), [plain_run("a "), text("b", bold()), plain_run(" c")]);
        assert_eq!(para("*it*"), [text("it", italic())]);
        assert_eq!(para("_it_"), [text("it", italic())]);
        assert_eq!(para("__under__"), [text("under", Style { underline: true, ..Style::default() })]);
        assert_eq!(para("~~gone~~"), [text("gone", Style { strike: true, ..Style::default() })]);
        assert_eq!(para("***both***"), [text("both", Style { bold: true, italic: true, ..Style::default() })]);
    }

    #[test]
    fn blank_italic_shows_nothing() {
        let text = |blocks: Vec<Block>| match &blocks[..] {
            [Block::Paragraph(runs)] => runs.iter().map(|r| match &r.content { Content::Text(t) => t.as_str(), _ => "?" }).collect::<String>(),
            _ => String::new(),
        };
        assert_eq!(text(parse("a _ _ b")), "a  b");
        assert_eq!(parse("snake_ _case"), [Block::Paragraph(vec![plain_run("snake_ _case")])]);
    }

    #[test]
    fn emphasis_nests() {
        assert_eq!(
            para("**a *b* c**"),
            [text("a ", bold()), text("b", Style { bold: true, italic: true, ..Style::default() }), text(" c", bold())]
        );
        assert_eq!(
            para("*a **b** c*"),
            [text("a ", italic()), text("b", Style { bold: true, italic: true, ..Style::default() }), text(" c", italic())]
        );
    }

    #[test]
    fn unclosed_or_spaced_delimiters_are_literal() {
        assert_eq!(para("**oops"), [plain_run("**oops")]);
        assert_eq!(para("2 * 3 * 4"), [plain_run("2 * 3 * 4")]);
        assert_eq!(para("a ** b ** c"), [plain_run("a ** b ** c")]);
        assert_eq!(para("~~"), [plain_run("~~")]);
        assert_eq!(para("**"), [plain_run("**")]);
    }

    #[test]
    fn snake_case_is_not_italic() {
        assert_eq!(para("my_snake_case_name"), [plain_run("my_snake_case_name")]);
        assert_eq!(para("_yes_ and_no_"), [text("yes", italic()), plain_run(" and_no_")]);
    }

    #[test]
    fn backslash_escapes() {
        assert_eq!(para(r"\*not italic\*"), [plain_run("*not italic*")]);
        assert_eq!(para(r"a \ b"), [plain_run(r"a \ b")]);
    }

    #[test]
    fn inline_code_is_literal() {
        let code = Style { code: true, ..Style::default() };
        assert_eq!(para("run `**x**` now"), [plain_run("run "), text("**x**", code.clone()), plain_run(" now")]);
        assert_eq!(para("``a ` b``"), [text("a ` b", code.clone())]);
        assert_eq!(para("` x `"), [text("x", code)]);
        assert_eq!(para("`unclosed"), [plain_run("`unclosed")]);
    }

    #[test]
    fn emphasis_cannot_close_inside_code() {
        assert_eq!(para("**a `**` b**"), [text("a ", bold()), text("**", Style { code: true, bold: true, ..Style::default() }), text(" b", bold())]);
    }

    #[test]
    fn fenced_code_blocks() {
        assert_eq!(parse("```rust\nfn main() {}\n```"), [Block::Code { lang: Some("rust".into()), text: "fn main() {}".into() }]);
        assert_eq!(parse("```\nplain\ntext\n```"), [Block::Code { lang: None, text: "plain\ntext".into() }]);
        assert_eq!(parse("```one line```"), [Block::Code { lang: None, text: "one line".into() }]);
        // A first line that is code, not a language.
        assert_eq!(parse("```let x = 1;\nlet y;```"), [Block::Code { lang: None, text: "let x = 1;\nlet y;".into() }]);
    }

    #[test]
    fn code_blocks_between_text() {
        assert_eq!(
            parse("before\n```\ncode\n```\nafter"),
            [
                Block::Paragraph(vec![plain_run("before")]),
                Block::Code { lang: None, text: "code".into() },
                Block::Paragraph(vec![plain_run("after")]),
            ]
        );
    }

    #[test]
    fn unclosed_fence_is_literal() {
        assert_eq!(parse("```oops"), [Block::Paragraph(vec![plain_run("```oops")])]);
    }

    #[test]
    fn escaped_fence_is_not_a_fence() {
        assert_eq!(parse(r"\```x```"), [Block::Paragraph(vec![plain_run("```x```")])]);
    }

    #[test]
    fn quotes() {
        assert_eq!(parse("> hello"), [Block::Quote(vec![Block::Paragraph(vec![plain_run("hello")])])]);
        assert_eq!(
            parse("> a\n> b\nc"),
            [Block::Quote(vec![Block::Paragraph(vec![plain_run("a\nb")])]), Block::Paragraph(vec![plain_run("c")])]
        );
        assert_eq!(parse(">hello"), [Block::Paragraph(vec![plain_run(">hello")])]);
    }

    #[test]
    fn block_quote_takes_the_rest() {
        assert_eq!(
            parse("intro\n>>> one\ntwo\n\n**three**"),
            [
                Block::Paragraph(vec![plain_run("intro")]),
                Block::Quote(vec![Block::Paragraph(vec![plain_run("one\ntwo\n\n"), text("three", bold())])]),
            ]
        );
    }

    #[test]
    fn headings_and_subtext() {
        assert_eq!(parse("# Big"), [Block::Heading { level: 1, runs: vec![plain_run("Big")] }]);
        assert_eq!(parse("## Mid **x**"), [Block::Heading { level: 2, runs: vec![plain_run("Mid "), text("x", bold())] }]);
        assert_eq!(parse("### Small"), [Block::Heading { level: 3, runs: vec![plain_run("Small")] }]);
        assert_eq!(parse("-# tiny"), [Block::Subtext(vec![plain_run("tiny")])]);
        // Not headers: four hashes, no space, nothing after.
        assert_eq!(parse("#### x"), [Block::Paragraph(vec![plain_run("#### x")])]);
        assert_eq!(parse("#tag"), [Block::Paragraph(vec![plain_run("#tag")])]);
        assert_eq!(parse("# "), [Block::Paragraph(vec![plain_run("#")])]);
    }

    #[test]
    fn lists() {
        let item = |depth, number, s: &str| ListItem { depth, number, runs: vec![plain_run(s)] };
        assert_eq!(parse("- a\n- b"), [Block::List(vec![item(0, None, "a"), item(0, None, "b")])]);
        assert_eq!(parse("* a\n  - nested"), [Block::List(vec![item(0, None, "a"), item(1, None, "nested")])]);
        assert_eq!(parse("1. one\n2. two"), [Block::List(vec![item(0, Some(1), "one"), item(0, Some(2), "two")])]);
        // `*italic*` is not a list item.
        assert_eq!(para("*italic* text"), [text("italic", italic()), plain_run(" text")]);
    }

    #[test]
    fn spoilers_share_a_group_per_pair() {
        let a = Style { spoiler: Some(0), ..Style::default() };
        let b = Style { spoiler: Some(1), ..Style::default() };
        assert_eq!(para("||one|| and ||two||"), [text("one", a), plain_run(" and "), text("two", b)]);
        let both = Style { spoiler: Some(0), bold: true, ..Style::default() };
        assert_eq!(para("||**x** y||"), [text("x", both), text(" y", Style { spoiler: Some(0), ..Style::default() })]);
        assert_eq!(para("||open"), [plain_run("||open")]);
    }

    #[test]
    fn masked_links_and_autolinks() {
        let link = |url: &str| Style { link: Some(url.into()), ..Style::default() };
        assert_eq!(para("[docs](https://x.dev/a)"), [text("docs", link("https://x.dev/a"))]);
        assert_eq!(para("see [**bold** docs](https://x.dev) ok"), [
            plain_run("see "),
            text("bold", Style { bold: true, link: Some("https://x.dev".into()), ..Style::default() }),
            text(" docs", link("https://x.dev")),
            plain_run(" ok"),
        ]);
        assert_eq!(para("go https://x.dev/a_b."), [plain_run("go "), text("https://x.dev/a_b", link("https://x.dev/a_b")), plain_run(".")]);
        assert_eq!(para("<https://x.dev>"), [text("https://x.dev", link("https://x.dev"))]);
        assert_eq!(para("(see https://x.dev/a_(b))"), [plain_run("(see "), text("https://x.dev/a_(b)", link("https://x.dev/a_(b)")), plain_run(")")]);
        // Not links: wrong scheme, brackets without a target.
        assert_eq!(para("[x](javascript:alert)"), [plain_run("[x](javascript:alert)")]);
        assert_eq!(para("[just brackets]"), [plain_run("[just brackets]")]);
    }

    #[test]
    fn links_are_found_while_typing() {
        let text = "see https://x.dev/a. and <https://y.dev> or [docs](https://z.dev)";
        let found: Vec<&str> = link_ranges(text).into_iter().map(|r| &text[r]).collect();
        assert_eq!(found, ["https://x.dev/a", "https://y.dev", "docs"]);
        assert!(link_ranges("nohttps://x.dev").is_empty(), "inside a word isn't a link");
    }

    #[test]
    fn urls_are_not_cut_by_markdown() {
        let url = "https://x.dev/a_b_c/__init__";
        let link = Style { link: Some(url.into()), ..Style::default() };
        assert_eq!(para(url), [text(url, link)]);
    }

    #[test]
    fn mentions_emoji_and_tokens() {
        assert_eq!(
            para("hi <@1> <@&2> <#3>"),
            [
                plain_run("hi "),
                Run { content: Content::User(Id(1)), style: Style::default() },
                plain_run(" "),
                Run { content: Content::Role(Id(2)), style: Style::default() },
                plain_run(" "),
                Run { content: Content::Channel(Id(3)), style: Style::default() },
            ]
        );
        let runs = para("**<@5>** 👋 <:snake_case_x:9>");
        assert_eq!(runs[0], Run { content: Content::User(Id(5)), style: bold() });
        assert!(matches!(runs[2].content, Content::Emoji { .. }));
        // Underscores inside a custom emoji name are not emphasis.
        assert!(matches!(&runs[4].content, Content::CustomEmoji { name, .. } if name == "snake_case_x"));
    }

    #[test]
    fn a_backslash_keeps_things_raw() {
        let plain = |t: &str| -> String {
            parse_inline(t).iter().map(|r| match &r.content { Content::Text(t) => t.clone(), other => format!("{other:?}") }).collect()
        };
        assert_eq!(plain("\\<@123>"), "<@123>");
        assert_eq!(plain("\\<:cat:12>"), "<:cat:12>");
        // The plain character: no picture, even though the text form is asked for.
        assert_eq!(plain("\\😂").replace('\u{FE0E}', ""), "😂");
    }

    #[test]
    fn previews_drop_block_marks_only() {
        assert_eq!(preview_line("-# **ada** has left"), "**ada** has left");
        assert_eq!(preview_line("### big"), "big");
        assert_eq!(preview_line("> - quoted item"), "quoted item");
        assert_eq!(preview_line("#hashtag"), "#hashtag");
        assert_eq!(preview_line("#### four"), "#### four");
        assert_eq!(preview_line("plain *text*"), "plain *text*");
    }

    #[test]
    fn timestamps() {
        let ts = |unix, style| Run { content: Content::Timestamp { unix, style }, style: Style::default() };
        assert_eq!(para("<t:1618953630:R>"), [ts(1618953630, 'R')]);
        assert_eq!(para("<t:1618953630>"), [ts(1618953630, 'f')]);
        assert_eq!(para("<t:1618953630:x>"), [plain_run("<t:1618953630:x>")]);
        assert_eq!(para("<t:abc>"), [plain_run("<t:abc>")]);
    }

    #[test]
    fn timestamp_formats() {
        let now = Utc.with_ymd_and_hms(2021, 4, 20, 20, 0, 0).unwrap();
        let t = |style| format_timestamp(1618953630, style, &now); // 2021-04-20 21:20:30 UTC
        assert_eq!(t('t'), "9:20 PM");
        assert_eq!(t('T'), "9:20:30 PM");
        assert_eq!(t('d'), "04/20/2021");
        assert_eq!(t('D'), "April 20, 2021");
        assert_eq!(t('f'), "April 20, 2021 9:20 PM");
        assert_eq!(t('F'), "Tuesday, April 20, 2021 9:20 PM");
        assert_eq!(t('R'), "in an hour");
        assert_eq!(format_timestamp(1618953630 - 4 * 86_400, 'R', &now), "3 days ago");
        assert_eq!(format_timestamp(now.timestamp() - 3600 - 1200, 'R', &now), "an hour ago");
        let midnight = Utc.with_ymd_and_hms(2021, 4, 20, 0, 5, 0).unwrap();
        assert_eq!(format_timestamp(midnight.timestamp(), 't', &now), "12:05 AM");
    }

    #[test]
    fn jumbo_emoji_only() {
        assert!(is_jumbo(&parse("👋 🎉")));
        assert!(!is_jumbo(&parse("hi 👋")));
        assert!(!is_jumbo(&parse("**👋**x")));
        assert!(!is_jumbo(&parse("<@1>")));
        assert!(!is_jumbo(&parse("👋\n\n# x")));
    }

    #[test]
    fn realistic_message() {
        let blocks = parse("# Release 1.2\nThanks **everyone**!\n> quoted _text_\n- fix\n- more ||spoiler||\n```sh\ncargo build\n```\n-# posted by <@1>");
        assert_eq!(blocks.len(), 6);
        assert!(matches!(blocks[0], Block::Heading { level: 1, .. }));
        assert!(matches!(blocks[1], Block::Paragraph(_)));
        assert!(matches!(blocks[2], Block::Quote(_)));
        assert!(matches!(blocks[3], Block::List(ref items) if items.len() == 2));
        assert!(matches!(blocks[4], Block::Code { .. }));
        assert!(matches!(blocks[5], Block::Subtext(_)));
    }

    fn quote_depth(blocks: &[Block]) -> usize {
        blocks
            .iter()
            .map(|b| match b {
                Block::Quote(inner) => 1 + quote_depth(inner),
                _ => 0,
            })
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn deep_quotes_stop_nesting() {
        let deep = format!("{}x", "> ".repeat(1000));
        let blocks = parse(&deep);
        assert_eq!(quote_depth(&blocks), 3);
        // What is deeper stays as text.
        let mut inner = &blocks;
        while let Some(Block::Quote(i)) = inner.first() {
            inner = i;
        }
        assert!(matches!(inner.first(), Some(Block::Paragraph(runs)) if runs.iter().any(|r| matches!(&r.content, Content::Text(t) if t.contains('>')))));
        let triple = format!("{}x", ">>> ".repeat(1000));
        assert_eq!(quote_depth(&parse(&triple)), 3);
        // Ordinary quotes are untouched.
        assert_eq!(quote_depth(&parse("> > hi")), 2);
    }
}
