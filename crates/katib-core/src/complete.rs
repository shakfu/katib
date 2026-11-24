//! Wiki link completion: where the reference being typed starts, and the edit
//! that completes it. Both editors use these; the index supplies the notes.

use std::ops::Range;

/// The wiki reference the caret at `head` is typing: the byte offset just
/// after its `[[`, and the text typed so far. `None` outside an open `[[` on
/// the caret's line, and once a `#` or `|` ends the reference.
pub fn wiki_query(text: &str, head: usize) -> Option<(usize, &str)> {
    if !text.is_char_boundary(head) {
        return None;
    }
    let line_start = text[..head].rfind('\n').map_or(0, |i| i + 1);
    let before = &text[line_start..head];
    let open = before.rfind("[[")?;
    let query = &before[open + 2..];
    if query.contains("]]") || query.contains(['#', '|', '[', ']']) {
        return None;
    }
    Some((line_start + open + 2, query))
}

/// The edit that completes the reference typed from `start` to `head` as
/// `reference`: the range to replace and its text, which closes the link
/// unless `]]` already follows the caret. The caret goes after the `]]`.
pub fn completion_edit(
    text: &str,
    start: usize,
    head: usize,
    reference: &str,
) -> (Range<usize>, String) {
    if text[head..].starts_with("]]") {
        (start..head + 2, format!("{reference}]]"))
    } else {
        (start..head, format!("{reference}]]"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_reference_being_typed() {
        let t = "see [[Pro";
        assert_eq!(wiki_query(t, t.len()), Some((6, "Pro")));
        assert_eq!(wiki_query("[[", 2), Some((2, "")));
        // The last `[[` on the caret's line counts; an earlier line does not.
        assert_eq!(wiki_query("[[a]] [[b", 9), Some((8, "b")));
        assert_eq!(wiki_query("[[a\nb", 5), None);
        // A closed link, a heading or a label ends the reference.
        assert_eq!(wiki_query("[[a]] b", 7), None);
        assert_eq!(wiki_query("[[a#h", 5), None);
        assert_eq!(wiki_query("[[a|l", 5), None);
        assert_eq!(wiki_query("plain", 5), None);
    }

    #[test]
    fn the_edit_closes_the_link_once() {
        assert_eq!(
            completion_edit("[[pro", 2, 5, "projects/katib"),
            (2..5, "projects/katib]]".into())
        );
        // Typed inside `[[]]`: the existing brackets are replaced, not doubled.
        assert_eq!(
            completion_edit("[[pro]] x", 2, 5, "katib"),
            (2..7, "katib]]".into())
        );
    }
}
