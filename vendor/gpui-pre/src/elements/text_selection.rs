//! Rustcord: selecting text that isn't an input, opt-in with `InteractiveText::allow_select`.
//!
//! Each selectable text says where it is when it paints. One selection for the window runs
//! from an anchor to a focus, each a (text, byte index); texts are in reading order (top to
//! bottom, then left to right), so a drag from one into another takes in what's between, unless
//! [`SelectionPrefs::across`] keeps it to the text it started in. Each text paints its own share
//! of the highlight. A press only becomes a selection once the pointer's moved a few pixels, so a
//! plain click on a link still is one.

use std::collections::HashMap;
use std::ops::Range;
use std::time::{Duration, Instant};

use crate::{App, Bounds, GlobalElementId, Global, Hsla, Pixels, Point, SharedString, TextLayout, point, px, size};

/// How the app wants selecting to behave. Set it with `cx.set_global`.
#[derive(Clone, Copy, Debug)]
pub struct SelectionPrefs {
    /// A selection can run from one text into others. Off, it stays in the one it began in.
    pub across: bool,
    /// Copying takes each text's source (as given by `select_source`), whole, instead of
    /// the part of its visible text that's selected.
    pub copy_source: bool,
    /// The highlight behind selected text.
    pub color: Hsla,
}

impl Default for SelectionPrefs {
    fn default() -> Self {
        SelectionPrefs { across: true, copy_source: false, color: Hsla { h: 0.6, s: 0.8, l: 0.55, a: 0.35 } }
    }
}

impl Global for SelectionPrefs {}

/// How far the pointer moves before a press is a selection rather than a click.
const DRAG_START: f32 = 4.;
/// A text not painted for this long is taken to be gone (scrolled away, closed).
const FORGET_AFTER: Duration = Duration::from_secs(3);

struct Entry {
    bounds: Bounds<Pixels>,
    layout: TextLayout,
    text: SharedString,
    /// The source and the group it belongs to: texts cut from one thing share a group, and
    /// copying its source takes it once.
    source: Option<(u64, SharedString)>,
    painted: Instant,
}

type Key = GlobalElementId;

#[derive(Default)]
pub(crate) struct TextSelection {
    entries: HashMap<Key, Entry>,
    /// Pressed here, not yet dragged.
    pressed: Option<(Key, usize, Point<Pixels>)>,
    anchor: Option<(Key, usize)>,
    focus: Option<(Key, usize)>,
    dragging: bool,
    /// A selection was made since the button went down: the release isn't a click.
    pub(crate) dragged: bool,
}

impl Global for TextSelection {}

fn prefs(cx: &App) -> SelectionPrefs {
    cx.try_global::<SelectionPrefs>().copied().unwrap_or_default()
}

impl TextSelection {
    /// Notes where a selectable text is, as it paints.
    pub(crate) fn register(&mut self, key: &Key, bounds: Bounds<Pixels>, layout: &TextLayout, text: &SharedString, source: Option<&(u64, SharedString)>) {
        let now = Instant::now();
        self.entries.insert(key.clone(), Entry { bounds, layout: layout.clone(), text: text.clone(), source: source.cloned(), painted: now });
        let keep: [Option<&Key>; 2] = [self.anchor.as_ref().map(|a| &a.0), self.focus.as_ref().map(|f| &f.0)];
        let held: Vec<Key> = keep.into_iter().flatten().cloned().collect();
        self.entries.retain(|k, e| now.duration_since(e.painted) < FORGET_AFTER || held.contains(k));
    }

    /// Texts in reading order.
    fn order(&self) -> Vec<&Key> {
        let mut keys: Vec<&Key> = self.entries.keys().collect();
        keys.sort_by(|a, b| {
            let (a, b) = (&self.entries[*a].bounds, &self.entries[*b].bounds);
            (f32::from(a.top()), f32::from(a.left())).partial_cmp(&(f32::from(b.top()), f32::from(b.left()))).unwrap_or(std::cmp::Ordering::Equal)
        });
        keys
    }

    /// Where a point falls: in a text, or past one (the nearest above it, at its end).
    fn hit(&self, at: Point<Pixels>) -> Option<(Key, usize)> {
        let order = self.order();
        if let Some(key) = order.iter().find(|k| self.entries[**k].bounds.contains(&at)) {
            let entry = &self.entries[*key];
            let index = entry.layout.index_for_position(at).unwrap_or_else(|i| i);
            return Some(((*key).clone(), index.min(entry.text.len())));
        }
        // Between texts: the end of the last one that starts above the point, or the start of the first.
        let above = order.iter().rev().find(|k| self.entries[**k].bounds.top() <= at.y);
        match above {
            Some(key) => {
                let entry = &self.entries[*key];
                let index = if at.y > entry.bounds.bottom() { entry.text.len() } else { entry.layout.index_for_position(at).unwrap_or_else(|i| i).min(entry.text.len()) };
                Some(((*key).clone(), index))
            }
            None => order.first().map(|k| ((*k).clone(), 0)),
        }
    }

    fn rank(&self, key: &Key) -> Option<usize> {
        self.order().iter().position(|k| *k == key)
    }

    /// The selection, start before end, as (key, index) pairs.
    fn ordered(&self) -> Option<((Key, usize), (Key, usize))> {
        let (a, f) = (self.anchor.clone()?, self.focus.clone()?);
        let (ra, rf) = (self.rank(&a.0)?, self.rank(&f.0)?);
        Some(if (ra, a.1) <= (rf, f.1) { (a, f) } else { (f, a) })
    }

    /// The part of `key`'s text that's selected, if any.
    pub(crate) fn range_in(&self, key: &Key) -> Option<Range<usize>> {
        let ((start_key, start), (end_key, end)) = self.ordered()?;
        let me = self.rank(key)?;
        let (rs, re) = (self.rank(&start_key)?, self.rank(&end_key)?);
        if me < rs || me > re {
            return None;
        }
        let len = self.entries.get(key)?.text.len();
        let from = if me == rs { start } else { 0 };
        let to = if me == re { end } else { len };
        (from < to).then_some(from..to.min(len))
    }

    pub(crate) fn press(&mut self, key: &Key, index: usize, at: Point<Pixels>, clicks: usize) {
        self.dragged = false;
        // A release that never came (the button let go outside the window) mustn't carry over.
        self.pressed = None;
        self.dragging = false;
        self.anchor = None;
        self.focus = None;
        let Some(entry) = self.entries.get(key) else { return };
        match clicks {
            2 => {
                let word = word_at(&entry.text, index);
                self.anchor = Some((key.clone(), word.start));
                self.focus = Some((key.clone(), word.end));
                self.dragged = true;
            }
            n if n >= 3 => {
                self.anchor = Some((key.clone(), 0));
                self.focus = Some((key.clone(), entry.text.len()));
                self.dragged = true;
            }
            _ => self.pressed = Some((key.clone(), index, at)),
        }
    }

    /// The pointer moved with the button down: a selection once it's gone far enough.
    /// Whether anything changed.
    pub(crate) fn drag(&mut self, at: Point<Pixels>, cx: &App) -> bool {
        self.drag_by(at, prefs(cx).across)
    }

    fn drag_by(&mut self, at: Point<Pixels>, across: bool) -> bool {
        if !self.dragging {
            let Some((key, index, from)) = self.pressed.clone() else { return false };
            let moved = (f32::from(at.x - from.x)).hypot(f32::from(at.y - from.y));
            if moved < DRAG_START {
                return false;
            }
            self.anchor = Some((key, index));
            self.dragging = true;
            self.dragged = true;
        }
        let Some(mut focus) = self.hit(at) else { return false };
        // Kept to the text it began in: clamp to its start or end.
        if !across
            && let Some((anchor_key, _)) = self.anchor.clone()
            && focus.0 != anchor_key
        {
            let before = self.rank(&focus.0) < self.rank(&anchor_key);
            let len = self.entries.get(&anchor_key).map_or(0, |e| e.text.len());
            focus = (anchor_key, if before { 0 } else { len });
        }
        let changed = self.focus.as_ref() != Some(&focus);
        self.focus = Some(focus);
        changed
    }

    pub(crate) fn release(&mut self) {
        self.pressed = None;
        self.dragging = false;
    }

    /// Whether `key` is the text a press or drag began in, the one that follows the pointer.
    pub(crate) fn drives(&self, key: &Key) -> bool {
        match (&self.pressed, self.dragging, &self.anchor) {
            (Some((pressed, ..)), false, _) => pressed == key,
            (_, true, Some((anchor, _))) => anchor == key,
            _ => false,
        }
    }

    pub(crate) fn is_dragging(&self) -> bool {
        self.dragging || self.pressed.is_some()
    }

    pub(crate) fn clear(&mut self) -> bool {
        let had = self.anchor.is_some();
        self.anchor = None;
        self.focus = None;
        had
    }

    fn has_selection(&self) -> bool {
        self.ordered().is_some_and(|((ak, ai), (fk, fi))| ak != fk || ai != fi)
    }

    /// What copying takes: the selected part of each text, a line apart, or each text's source.
    fn copied(&self, cx: &App) -> Option<String> {
        if !self.has_selection() {
            return None;
        }
        let source = prefs(cx).copy_source;
        let mut parts: Vec<String> = Vec::new();
        let mut groups: Vec<u64> = Vec::new();
        // Where the last text copied sat, to tell the next one beside it from one below it.
        let mut last: Option<Bounds<Pixels>> = None;
        for key in self.order() {
            let Some(range) = self.range_in(key) else { continue };
            let entry = &self.entries[key];
            let part = match (&entry.source, source) {
                (Some((group, raw)), true) => {
                    // The source is the whole thing, once however many of its texts are selected.
                    if groups.contains(group) {
                        last = Some(entry.bounds);
                        continue;
                    }
                    groups.push(*group);
                    raw.to_string()
                }
                _ => entry.text.get(range).unwrap_or_default().to_string(),
            };
            // Words of one line, laid out as separate texts, go back together; lines are a line apart.
            let beside_last = last.is_some_and(|prev| entry.bounds.top() < prev.bottom() && entry.bounds.left() >= prev.left());
            if beside_last && let Some(prev) = parts.last_mut() {
                prev.push_str(&part);
            } else {
                parts.push(part);
            }
            last = Some(entry.bounds);
        }
        (!parts.is_empty()).then(|| parts.join("\n"))
    }

    /// Whether a point is on any selectable text.
    pub(crate) fn over_text(&self, at: Point<Pixels>) -> bool {
        self.entries.values().any(|e| e.bounds.contains(&at))
    }
}

/// The word around `index`: letters, digits and underscores, or the one other character there.
fn word_at(text: &str, index: usize) -> Range<usize> {
    let index = index.min(text.len());
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let start = text[..index].char_indices().rev().take_while(|(_, c)| is_word(*c)).last().map_or(index, |(i, _)| i);
    let end = text[index..].char_indices().find(|(_, c)| !is_word(*c)).map_or(text.len(), |(i, _)| index + i);
    if start == end {
        let next = text[index..].chars().next().map_or(0, char::len_utf8);
        return index..(index + next).min(text.len());
    }
    start..end
}

/// The rectangles that cover `range` of a laid-out text whose box is `bounds`.
pub(crate) fn rects(layout: &TextLayout, bounds: Bounds<Pixels>, range: Range<usize>) -> Vec<Bounds<Pixels>> {
    let line = layout.line_height();
    let (Some(from), Some(to)) = (layout.position_for_index(range.start), layout.position_for_index(range.end)) else { return Vec::new() };
    if (from.y - to.y).abs() < line / 2. {
        return vec![Bounds::new(from, size(to.x - from.x, line))];
    }
    let mut out = vec![Bounds::new(from, size(bounds.right() - from.x, line))];
    let mut y = from.y + line;
    while to.y - y > line / 2. {
        out.push(Bounds::new(point(bounds.left(), y), size(bounds.size.width, line)));
        y += line;
    }
    out.push(Bounds::new(point(bounds.left(), to.y), size((to.x - bounds.left()).max(px(0.)), line)));
    out
}

/// The text selected anywhere in the app, as copying would take it.
pub fn selected_text(cx: &App) -> Option<String> {
    cx.try_global::<TextSelection>()?.copied(cx)
}

/// Lets go of the selection. Whether there was one.
pub fn clear_text_selection(cx: &mut App) -> bool {
    cx.try_global::<TextSelection>().is_some() && cx.global_mut::<TextSelection>().clear()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &'static str) -> Key {
        GlobalElementId(std::sync::Arc::from([crate::ElementId::Name(name.into())]))
    }

    fn texts(lines: &[(&'static str, &'static str)]) -> TextSelection {
        let mut selection = TextSelection::default();
        for (i, (name, text)) in lines.iter().enumerate() {
            let bounds = Bounds::new(point(px(0.), px(20. * i as f32)), size(px(100.), px(18.)));
            selection.register(&key(name), bounds, &TextLayout::default(), &(*text).into(), None);
        }
        selection
    }

    #[test]
    fn a_selection_takes_the_ends_of_the_texts_it_touches_and_all_between() {
        let mut selection = texts(&[("a", "first one"), ("b", "second"), ("c", "third text")]);
        selection.anchor = Some((key("c"), 5));
        selection.focus = Some((key("a"), 6));
        assert_eq!(selection.range_in(&key("a")), Some(6..9));
        assert_eq!(selection.range_in(&key("b")), Some(0..6));
        assert_eq!(selection.range_in(&key("c")), Some(0..5), "dragging upward works the same");
    }

    #[test]
    fn a_press_is_not_a_selection_until_the_pointer_moves() {
        let mut selection = texts(&[("a", "some text")]);
        selection.press(&key("a"), 2, point(px(10.), px(5.)), 1);
        assert!(!selection.drag_by(point(px(12.), px(5.)), true), "two pixels is still a click");
        assert!(!selection.dragged);
        assert!(selection.range_in(&key("a")).is_none());
    }

    #[test]
    fn double_and_triple_clicks_pick_a_word_and_the_whole_text() {
        let mut selection = texts(&[("a", "hello big world")]);
        selection.press(&key("a"), 7, point(px(0.), px(0.)), 2);
        assert_eq!(selection.range_in(&key("a")), Some(6..9));
        selection.press(&key("a"), 7, point(px(0.), px(0.)), 3);
        assert_eq!(selection.range_in(&key("a")), Some(0..15));
    }

    #[test]
    fn words_are_found_around_an_index() {
        assert_eq!(word_at("hello big world", 7), 6..9);
        assert_eq!(word_at("hello big world", 0), 0..5);
        assert_eq!(word_at("a, b", 1), 1..2, "on punctuation: just it");
        assert_eq!(word_at("snake_case ok", 3), 0..10);
    }
}
