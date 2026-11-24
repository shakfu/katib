use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation as _;

/// The edited text, its selection, and the IME composition range, in UTF-8 bytes.
#[derive(Clone, Debug, Default)]
pub struct Document {
    text: String,
    /// `anchor..head` may run backwards; `head` is where the caret is drawn.
    anchor: usize,
    head: usize,
    marked: Option<Range<usize>>,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    /// Whether the next edit may merge into the last undo step.
    open_group: bool,
}

/// One undo step: `old` at `at` became `new`, with the selection before and after.
#[derive(Clone, Debug)]
struct Edit {
    at: usize,
    old: String,
    new: String,
    before: (usize, usize),
    after: (usize, usize),
}

/// Undo history is cut to this many steps.
const UNDO_LIMIT: usize = 1000;

impl Document {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Default::default()
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn head(&self) -> usize {
        self.head
    }

    pub fn selection(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    pub fn is_reversed(&self) -> bool {
        self.head < self.anchor
    }

    pub fn marked(&self) -> Option<Range<usize>> {
        self.marked.clone()
    }

    /// Replace the whole text, as on a reload; clears the history.
    pub fn set_text(&mut self, text: impl Into<String>, head: usize) {
        self.text = text.into();
        self.marked = None;
        self.undo.clear();
        self.redo.clear();
        self.open_group = false;
        self.move_to(head, false);
    }

    /// Replace the whole text as one undo step, keeping earlier history (a
    /// switch back from raw mode). Only the changed middle is recorded.
    pub fn replace_all(&mut self, text: &str, head: usize) {
        if text != self.text {
            let (old, new) = (self.text.as_bytes(), text.as_bytes());
            let mut pre = old.iter().zip(new).take_while(|(a, b)| a == b).count();
            while !self.text.is_char_boundary(pre) || !text.is_char_boundary(pre) {
                pre -= 1;
            }
            let room = (old.len() - pre).min(new.len() - pre);
            let mut suf = old
                .iter()
                .rev()
                .zip(new.iter().rev())
                .take(room)
                .take_while(|(a, b)| a == b)
                .count();
            while !self.text.is_char_boundary(old.len() - suf)
                || !text.is_char_boundary(new.len() - suf)
            {
                suf -= 1;
            }
            let end = old.len() - suf;
            self.seal();
            self.replace(Some(pre..end), &text[pre..new.len() - suf]);
            self.seal();
        }
        self.move_to(head, false);
    }

    /// End the current undo step, so the next edit starts a new one.
    pub fn seal(&mut self) {
        self.open_group = false;
    }

    pub fn undo(&mut self) -> bool {
        let Some(edit) = self.undo.pop() else {
            return false;
        };
        self.text
            .replace_range(edit.at..edit.at + edit.new.len(), &edit.old);
        (self.anchor, self.head) = edit.before;
        self.marked = None;
        self.open_group = false;
        self.redo.push(edit);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(edit) = self.redo.pop() else {
            return false;
        };
        self.text
            .replace_range(edit.at..edit.at + edit.old.len(), &edit.new);
        (self.anchor, self.head) = edit.after;
        self.marked = None;
        self.open_group = false;
        self.undo.push(edit);
        true
    }

    /// Apply one edit and record it, merging typing, deleting and IME
    /// composition into the open step.
    fn splice(&mut self, range: Range<usize>, new: &str) {
        let before = (self.anchor, self.head);
        let old = self.text[range.clone()].to_owned();
        self.text.replace_range(range.clone(), new);
        self.redo.clear();
        let edit = Edit {
            at: range.start,
            old,
            new: new.to_owned(),
            before,
            after: before,
        };
        if self.open_group
            && let Some(last) = self.undo.last_mut()
            && merge(last, &edit)
        {
            return;
        }
        self.undo.push(edit);
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.open_group = true;
    }

    /// Record the selection after the last edit, for redo.
    fn settle(&mut self) {
        if let Some(last) = self.undo.last_mut() {
            last.after = (self.anchor, self.head);
        }
    }

    /// Move the caret; with `extend`, keep the anchor and select up to it.
    pub fn move_to(&mut self, offset: usize, extend: bool) {
        self.head = self.clamp(offset);
        if !extend {
            self.anchor = self.head;
        }
    }

    pub fn select(&mut self, range: Range<usize>, reversed: bool) {
        let (start, end) = (self.clamp(range.start), self.clamp(range.end));
        (self.anchor, self.head) = if reversed { (end, start) } else { (start, end) };
    }

    /// Replace `range` (default: the composition, else the selection) and put the caret after it.
    pub fn replace(&mut self, range: Option<Range<usize>>, new: &str) {
        let range = self.edit_range(range);
        self.splice(range.clone(), new);
        self.move_to(range.start + new.len(), false);
        self.marked = None;
        self.settle();
    }

    /// Replace as [`Self::replace`] and mark the new text as an IME composition.
    /// `selected` is relative to the new text, in bytes.
    pub fn replace_and_mark(
        &mut self,
        range: Option<Range<usize>>,
        new: &str,
        selected: Option<Range<usize>>,
    ) {
        let range = self.edit_range(range);
        self.splice(range.clone(), new);
        let start = range.start;
        self.marked = (!new.is_empty()).then(|| start..start + new.len());
        match selected {
            Some(sel) => self.select(
                start + sel.start.min(new.len())..start + sel.end.min(new.len()),
                false,
            ),
            None => self.move_to(start + new.len(), false),
        }
        self.settle();
    }

    pub fn unmark(&mut self) {
        self.marked = None;
    }

    /// Delete the selection, or the grapheme before the caret.
    pub fn backspace(&mut self) {
        let range = match self.selection() {
            r if r.is_empty() => self.prev_boundary(self.head)..self.head,
            r => r,
        };
        self.replace(Some(range), "");
    }

    /// Delete the selection, or the grapheme after the caret.
    pub fn delete(&mut self) {
        let range = match self.selection() {
            r if r.is_empty() => self.head..self.next_boundary(self.head),
            r => r,
        };
        self.replace(Some(range), "");
    }

    pub fn prev_boundary(&self, offset: usize) -> usize {
        self.text[..offset]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    pub fn next_boundary(&self, offset: usize) -> usize {
        self.text[offset..]
            .graphemes(true)
            .next()
            .map_or(self.text.len(), |g| offset + g.len())
    }

    pub fn line_start(&self, offset: usize) -> usize {
        self.text[..offset].rfind('\n').map_or(0, |i| i + 1)
    }

    pub fn line_end(&self, offset: usize) -> usize {
        self.text[offset..]
            .find('\n')
            .map_or(self.text.len(), |i| offset + i)
    }

    pub fn to_utf16(&self, offset: usize) -> usize {
        self.text[..self.clamp(offset)].encode_utf16().count()
    }

    pub fn from_utf16(&self, offset: usize) -> usize {
        let mut units = 0;
        for (i, c) in self.text.char_indices() {
            if units >= offset {
                return i;
            }
            units += c.len_utf16();
        }
        self.text.len()
    }

    pub fn range_to_utf16(&self, r: &Range<usize>) -> Range<usize> {
        self.to_utf16(r.start)..self.to_utf16(r.end)
    }

    pub fn range_from_utf16(&self, r: &Range<usize>) -> Range<usize> {
        self.from_utf16(r.start)..self.from_utf16(r.end)
    }

    fn edit_range(&self, range: Option<Range<usize>>) -> Range<usize> {
        let r = range
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection());
        self.clamp(r.start)..self.clamp(r.end.max(r.start))
    }

    /// Clamp to the text and down to a char boundary, so no edit splits a code point.
    fn clamp(&self, mut offset: usize) -> usize {
        offset = offset.min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }
}

/// Merge `next` into `last` when it continues it: typing on, deleting
/// backwards or forwards from the same place, or replacing an IME composition.
/// Typing whitespace after a word ends the step, so undo goes a word at a time.
fn merge(last: &mut Edit, next: &Edit) -> bool {
    let typing = last.old.is_empty()
        && next.old.is_empty()
        && next.at == last.at + last.new.len()
        && !(next.new.starts_with(char::is_whitespace) && !last.new.ends_with(char::is_whitespace));
    let back = last.new.is_empty() && next.new.is_empty() && next.at + next.old.len() == last.at;
    let forward = last.new.is_empty() && next.new.is_empty() && next.at == last.at;
    let compose = next.at == last.at && next.old == last.new;
    if typing {
        last.new.push_str(&next.new);
    } else if back {
        last.at = next.at;
        last.old.insert_str(0, &next.old);
    } else if forward {
        last.old.push_str(&next.old);
    } else if compose {
        last.new.clone_from(&next.new);
    } else {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_replaces_selection() {
        let mut d = Document::new("hello world");
        d.select(0..5, false);
        d.replace(None, "bye");
        assert_eq!(d.text(), "bye world");
        assert_eq!(d.selection(), 3..3);
    }

    #[test]
    fn backspace_and_delete_take_whole_graphemes() {
        let mut d = Document::new("ae\u{301}x");
        d.move_to(4, false);
        d.backspace();
        assert_eq!(d.text(), "ax");
        d.move_to(0, false);
        d.delete();
        assert_eq!(d.text(), "x");
        d.backspace();
        assert_eq!(d.text(), "x");
    }

    #[test]
    fn ime_composition_replaces_itself_then_commits() {
        let mut d = Document::new("a");
        d.move_to(1, false);
        d.replace_and_mark(None, "k", None);
        d.replace_and_mark(None, "\u{304b}", None);
        assert_eq!(d.text(), "a\u{304b}");
        assert_eq!(d.marked(), Some(1..4));
        d.replace(None, "\u{6f22}");
        assert_eq!(d.text(), "a\u{6f22}");
        assert_eq!(d.marked(), None);
        assert_eq!(d.head(), 4);
    }

    #[test]
    fn utf16_round_trip() {
        let d = Document::new("a\u{1F600}b\u{e9}");
        assert_eq!(d.to_utf16(5), 3);
        assert_eq!(d.from_utf16(3), 5);
        for offset in [0, 1, 5, 6, 8] {
            assert_eq!(d.from_utf16(d.to_utf16(offset)), offset);
        }
    }

    #[test]
    fn offsets_inside_a_code_point_are_clamped() {
        let mut d = Document::new("\u{e9}");
        d.move_to(1, false);
        assert_eq!(d.head(), 0);
        // A range that starts inside a code point widens to include it.
        d.replace(Some(1..2), "x");
        assert_eq!(d.text(), "x");
    }

    #[test]
    fn reversed_selection() {
        let mut d = Document::new("abcdef");
        d.move_to(4, false);
        d.move_to(1, true);
        assert_eq!(d.selection(), 1..4);
        assert!(d.is_reversed());
        assert_eq!(d.head(), 1);
    }

    #[test]
    fn lines() {
        let d = Document::new("ab\ncd\n");
        assert_eq!((d.line_start(4), d.line_end(4)), (3, 5));
        assert_eq!((d.line_start(6), d.line_end(6)), (6, 6));
    }

    fn typed(d: &mut Document, s: &str) {
        for c in s.chars() {
            d.replace(None, &c.to_string());
        }
    }

    #[test]
    fn undo_goes_a_word_at_a_time() {
        let mut d = Document::new("");
        typed(&mut d, "one two");
        assert!(d.undo());
        assert_eq!(d.text(), "one");
        assert!(d.undo());
        assert_eq!(d.text(), "");
        assert!(!d.undo());
        assert!(d.redo());
        assert!(d.redo());
        assert_eq!(d.text(), "one two");
        assert_eq!(d.head(), 7);
    }

    #[test]
    fn a_caret_move_seals_the_step() {
        let mut d = Document::new("");
        typed(&mut d, "ab");
        d.seal();
        typed(&mut d, "cd");
        d.undo();
        assert_eq!(d.text(), "ab");
    }

    #[test]
    fn backspaces_merge_and_restore_the_selection() {
        let mut d = Document::new("hello");
        d.move_to(5, false);
        d.backspace();
        d.backspace();
        assert_eq!(d.text(), "hel");
        d.undo();
        assert_eq!(d.text(), "hello");
        assert_eq!(d.selection(), 5..5);
        d.select(1..4, false);
        d.replace(None, "");
        d.undo();
        assert_eq!((d.text(), d.selection()), ("hello", 1..4));
    }

    #[test]
    fn an_ime_composition_is_one_step() {
        let mut d = Document::new("a");
        d.move_to(1, false);
        d.replace_and_mark(None, "k", None);
        d.replace_and_mark(None, "\u{304b}", None);
        d.replace(None, "\u{6f22}");
        assert_eq!(d.text(), "a\u{6f22}");
        d.undo();
        assert_eq!(d.text(), "a");
    }

    #[test]
    fn an_edit_after_undo_drops_redo() {
        let mut d = Document::new("");
        typed(&mut d, "a");
        d.undo();
        typed(&mut d, "b");
        assert!(!d.redo());
        assert_eq!(d.text(), "b");
    }

    #[test]
    fn replace_all_is_one_step_after_earlier_history() {
        let mut d = Document::new("");
        typed(&mut d, "caf\u{e9} one");
        d.replace_all("caf\u{e8} one two", 0);
        assert_eq!(d.head(), 0);
        assert!(d.undo());
        assert_eq!(d.text(), "caf\u{e9} one");
        assert!(d.undo());
        assert_eq!(d.text(), "caf\u{e9}");
        // Equal text records nothing.
        d.replace_all("caf\u{e9}", 1);
        assert!(d.undo());
        assert_eq!(d.text(), "");
    }
}
