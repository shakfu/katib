//! Source text to the `BlockNode` tree gpui-kit renders, with leaf-to-source maps.
//!
//! The source is cut into segments of whole lines, one per top-level block or
//! blank line. The segment holding the caret is emitted as raw lines, so its
//! markdown syntax is visible and leaf bytes equal source bytes. Other segments
//! are formatted; their leaves map back to the source piecewise.

use std::ops::Range;

use gpui_kit::base::text::editor_host::{
    BlockNode, CodeBlock, ImageNode, InlineNode, LinkMark, Paragraph, TextMark,
};
use katib_core::LinkKind;
use katib_core::md::{self, Kind, Node};

/// Where one keyed leaf's bytes come from in the source.
#[derive(Clone, Debug, PartialEq)]
pub struct LeafMap {
    pub key: u64,
    /// `(leaf bytes, source bytes)` runs where the two are the same text, in order.
    pub pieces: Vec<(Range<usize>, Range<usize>)>,
    /// The source bytes this leaf stands for, markup included.
    pub source: Range<usize>,
}

impl LeafMap {
    /// The source offset for a leaf offset. Between runs (hidden markup), the next run's start.
    pub fn to_source(&self, leaf: usize) -> usize {
        for (l, s) in &self.pieces {
            // `leaf == l.end` is left to the next run, which starts there too.
            if leaf < l.end || leaf <= l.start {
                return s.start + leaf.saturating_sub(l.start);
            }
        }
        self.pieces.last().map_or(self.source.start, |(_, s)| s.end)
    }

    /// The leaf offset for a source offset, if the leaf shows that byte.
    pub fn to_leaf(&self, source: usize) -> Option<usize> {
        self.pieces
            .iter()
            .find(|(_, s)| s.start <= source && source <= s.end)
            .map(|(l, s)| l.start + (source - s.start))
    }

    /// The leaf bytes showing any part of `source`, for painting a selection.
    pub fn leaf_ranges(&self, source: &Range<usize>) -> Vec<Range<usize>> {
        self.pieces
            .iter()
            .filter_map(|(l, s)| {
                let start = source.start.max(s.start);
                let end = source.end.min(s.end);
                (start < end).then(|| l.start + (start - s.start)..l.start + (end - s.start))
            })
            .collect()
    }
}

#[derive(Debug)]
pub struct Segment {
    /// Whole lines, without the final newline.
    pub source: Range<usize>,
    pub blocks: Vec<BlockNode>,
    pub raw: bool,
}

#[derive(Debug, Default)]
pub struct Projection {
    pub segments: Vec<Segment>,
    /// Every leaf, in document order.
    pub leaves: Vec<LeafMap>,
}

impl Projection {
    pub fn leaf(&self, key: u64) -> Option<&LeafMap> {
        self.leaves.iter().find(|l| l.key == key)
    }
}

/// Project `text` with the caret at byte `head`; `None` (the editor is not
/// focused) draws every block formatted.
pub fn project(text: &str, head: Option<usize>) -> Projection {
    let root = md::parse(text);
    let mut out = Projection::default();
    let mut keys = 0u64;
    for (source, nodes) in segments(text, &root) {
        let active = head.is_some_and(|h| source.start <= h && h <= source.end);
        let first_leaf = out.leaves.len();
        let formatted = if active || nodes.is_empty() {
            None
        } else {
            let mut cx = Cx {
                text,
                keys: &mut keys,
                leaves: &mut out.leaves,
            };
            nodes
                .iter()
                .map(|n| cx.block(n))
                .collect::<Option<Vec<_>>>()
        };
        let (blocks, raw) = match formatted {
            Some(blocks) => (blocks, false),
            None => {
                out.leaves.truncate(first_leaf);
                // Blank lines are drawn as empty lines, so vertical space follows
                // the source and every line has somewhere to put the caret.
                let heading = match nodes.as_slice() {
                    [
                        Node {
                            kind: Kind::Heading(level),
                            ..
                        },
                    ] => Some(*level),
                    _ => None,
                };
                (
                    raw_lines(text, source.clone(), heading, &mut keys, &mut out.leaves),
                    true,
                )
            }
        };
        out.segments.push(Segment {
            source,
            blocks,
            raw,
        });
    }
    out
}

/// Cut `text` into line-aligned segments: one per run of top-level nodes that
/// share lines, and one per line no node covers.
fn segments<'a>(text: &str, nodes: &'a [Node]) -> Vec<(Range<usize>, Vec<&'a Node>)> {
    let line_start = |o: usize| text[..o].rfind('\n').map_or(0, |i| i + 1);
    let line_end = |o: usize| text[o..].find('\n').map_or(text.len(), |i| o + i);
    let mut out: Vec<(Range<usize>, Vec<&Node>)> = Vec::new();
    let mut cursor = 0;
    let push_blank = |out: &mut Vec<_>, from: usize, to: usize| {
        let mut at = from;
        while at < to {
            let end = line_end(at);
            out.push((at..end, Vec::new()));
            at = end + 1;
        }
    };
    for node in nodes {
        let p = &node.range;
        let start = line_start(p.start);
        let end = line_end(
            p.end
                .max(p.start + 1)
                .min(text.len())
                .saturating_sub(1)
                .max(p.start),
        );
        if let Some(last) = out
            .last_mut()
            .filter(|(r, n)| !n.is_empty() && start <= r.end)
        {
            last.0.end = last.0.end.max(end);
            last.1.push(node);
        } else {
            push_blank(&mut out, cursor, start);
            out.push((start..end, vec![node]));
        }
        cursor = (end + 1).min(text.len() + 1);
    }
    push_blank(&mut out, cursor, text.len());
    // A trailing newline (or an empty text) leaves an empty last line for the caret.
    if cursor <= text.len()
        && (text.is_empty() || text.ends_with('\n'))
        && out.last().is_none_or(|(r, _)| r.end < text.len())
    {
        out.push((text.len()..text.len(), Vec::new()));
    }
    out
}

fn raw_lines(
    text: &str,
    source: Range<usize>,
    heading: Option<u8>,
    keys: &mut u64,
    leaves: &mut Vec<LeafMap>,
) -> Vec<BlockNode> {
    let mut blocks = Vec::new();
    let mut start = source.start;
    loop {
        let end = text[start..source.end]
            .find('\n')
            .map_or(source.end, |i| start + i);
        let line = &text[start..end];
        *keys += 1;
        let mut para = Paragraph::default();
        if !line.is_empty() {
            para.push(
                InlineNode::new(line.to_owned()).marks(vec![(0..line.len(), TextMark::default())]),
            );
        }
        let para = para.with_leaf_key(*keys);
        leaves.push(LeafMap {
            key: *keys,
            pieces: vec![(0..line.len(), start..end)],
            source: start..end,
        });
        blocks.push(match heading {
            Some(level) if start == source.start => BlockNode::Heading {
                level,
                children: para,
                span: None,
            },
            _ => BlockNode::Paragraph(para),
        });
        if end >= source.end {
            return blocks;
        }
        start = end + 1;
    }
}

/// Runs where `value` and `source` hold the same bytes, matched greedily.
/// Source bytes the value lacks (escapes, indentation, fences) are skipped.
fn align(value: &str, source: &str) -> Vec<(Range<usize>, Range<usize>)> {
    let (v, s) = (value.as_bytes(), source.as_bytes());
    let (mut i, mut j) = (0, 0);
    let mut out: Vec<(Range<usize>, Range<usize>)> = Vec::new();
    while i < v.len() && j < s.len() {
        if v[i] == s[j] || (v[i] == b' ' && s[j] == b'\n') {
            match out.last_mut() {
                Some((l, r)) if l.end == i && r.end == j => {
                    l.end += 1;
                    r.end += 1;
                }
                _ => out.push((i..i + 1, j..j + 1)),
            }
            i += 1;
        }
        j += 1;
    }
    out
}

struct Cx<'a> {
    text: &'a str,
    keys: &'a mut u64,
    leaves: &'a mut Vec<LeafMap>,
}

/// A paragraph under construction: its rendered text length and its runs.
struct Inline {
    para: Paragraph,
    len: usize,
    pieces: Vec<(Range<usize>, Range<usize>)>,
}

impl Cx<'_> {
    fn key(&mut self) -> u64 {
        *self.keys += 1;
        *self.keys
    }

    /// A block, or `None` when it has no formatted form here and the segment stays raw.
    fn block(&mut self, node: &Node) -> Option<BlockNode> {
        let blocks = |cx: &mut Self| {
            node.children
                .iter()
                .map(|c| cx.block(c))
                .collect::<Option<Vec<_>>>()
        };
        Some(match &node.kind {
            Kind::Paragraph => BlockNode::Paragraph(self.paragraph(node)),
            Kind::Heading(level) => BlockNode::Heading {
                level: *level,
                children: self.paragraph(node),
                span: None,
            },
            Kind::Blockquote => BlockNode::Blockquote {
                children: blocks(self)?,
                span: None,
            },
            Kind::List { start } => BlockNode::List {
                children: blocks(self)?,
                ordered: start.is_some(),
                start: start.and_then(|n| u32::try_from(n).ok()),
                span: None,
            },
            Kind::Item { checked, spread } => BlockNode::ListItem {
                children: blocks(self)?,
                spread: *spread,
                checked: *checked,
                task_marker: None,
                span: None,
            },
            Kind::CodeBlock { lang, value } => {
                let src = node.range.clone();
                let slice = &self.text[src.clone()];
                // A fence line is not code; skip it so `align` cannot match the info string.
                let body = if slice.starts_with("```") || slice.starts_with("~~~") {
                    slice.find('\n').map_or(src.end, |i| src.start + i + 1)
                } else {
                    src.start
                };
                let key = self.key();
                let pieces = align(value, &self.text[body..src.end])
                    .into_iter()
                    .map(|(l, s)| (l, body + s.start..body + s.end))
                    .collect();
                self.leaves.push(LeafMap {
                    key,
                    pieces,
                    source: src,
                });
                BlockNode::CodeBlock(
                    CodeBlock::from_code(value.clone(), lang.clone()).with_leaf_key(key),
                )
            }
            Kind::Rule => BlockNode::HorizontalRule { span: None },
            _ => return None,
        })
    }

    fn paragraph(&mut self, node: &Node) -> Paragraph {
        let key = self.key();
        let mut inline = Inline {
            para: Paragraph::default(),
            len: 0,
            pieces: Vec::new(),
        };
        for child in &node.children {
            self.inline(child, &TextMark::default(), &mut inline);
        }
        self.leaves.push(LeafMap {
            key,
            pieces: inline.pieces,
            source: node.range.clone(),
        });
        inline.para.with_leaf_key(key)
    }

    fn inline(&mut self, node: &Node, mark: &TextMark, out: &mut Inline) {
        let src = node.range.clone();
        let source = &self.text[src.clone()];
        match &node.kind {
            Kind::Text(value) => self.text_with_links(value, src, mark, out),
            Kind::Strong => self.children(&node.children, &mark.clone().bold(), out),
            Kind::Emphasis => self.children(&node.children, &mark.clone().italic(), out),
            Kind::Strikethrough => {
                self.children(&node.children, &mark.clone().strikethrough(), out)
            }
            Kind::Link { url } => {
                let link = mark.clone().link(LinkMark {
                    url: url.clone().into(),
                    ..Default::default()
                });
                self.children(&node.children, &link, out);
            }
            Kind::Code(value) => push(out, value, source, src.start, &mark.clone().code()),
            // Local images are drawn; a remote one stays alt text, since fetching
            // it would tell its server whenever this note is read.
            Kind::Image { url, alt } if !has_scheme(url) => push_image(out, url, alt),
            Kind::Image { alt, .. } => push(out, alt, source, src.start, &mark.clone().italic()),
            Kind::Break => push(out, "\n", "\n", src.start, mark),
            _ => push(out, source, source, src.start, mark),
        }
    }

    fn children(&mut self, children: &[Node], mark: &TextMark, out: &mut Inline) {
        for child in children {
            self.inline(child, mark, out);
        }
    }

    /// A text node, with each `[[target|label]]` drawn as its label and marked as
    /// a link, and each bare URL or email address marked as one.
    fn text_with_links(
        &mut self,
        value: &str,
        src: Range<usize>,
        mark: &TextMark,
        out: &mut Inline,
    ) {
        let slice = &self.text[src.clone()];
        let mut links = Vec::new();
        katib_core::text_links(slice, src.start, mark.link.is_none(), &mut links);
        if links.is_empty() {
            return push(out, value, slice, src.start, mark);
        }
        let mut at = src.start;
        for link in links {
            let before = &self.text[at..link.range.start];
            push(out, before, before, at, mark);
            let whole = &self.text[link.range.clone()];
            if link.kind == LinkKind::External {
                let link_mark = mark.clone().link(LinkMark {
                    url: link.target.clone().into(),
                    ..Default::default()
                });
                push(out, whole, whole, link.range.start, &link_mark);
                at = link.range.end;
                continue;
            }
            let shown = link
                .label
                .clone()
                .unwrap_or_else(|| whole[2..whole.len() - 2].trim().to_owned());
            let url = format!("wiki:{}", link.target);
            let link_mark = mark.clone().link(LinkMark {
                url: url.into(),
                ..Default::default()
            });
            // Map the label to where it is written: after the `|`, or the whole reference.
            let label_at = whole
                .rfind(shown.as_str())
                .map_or(link.range.start, |i| link.range.start + i);
            push(
                out,
                &shown,
                &self.text[label_at..link.range.end],
                label_at,
                &link_mark,
            );
            at = link.range.end;
        }
        let rest = &self.text[at..src.end];
        push(out, rest, rest, at, mark);
    }
}

/// Append an image. It is one atom (`U+FFFC`, 3 bytes) in the leaf, with no
/// source run, so the caret skips it as it skips hidden markup.
fn push_image(out: &mut Inline, url: &str, alt: &str) {
    let mut image = ImageNode::default();
    image.url = url.to_owned().into();
    image.alt = (!alt.is_empty()).then(|| alt.to_owned().into());
    out.para.push(InlineNode::image(image));
    out.len += '\u{fffc}'.len_utf8();
}

/// Whether `url` names a scheme (`https:`, `data:`), not a path.
fn has_scheme(url: &str) -> bool {
    url.split_once(':').is_some_and(|(scheme, _)| {
        scheme.len() > 1
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    })
}

/// Append `value`, drawn from `source` starting at byte `offset`.
fn push(out: &mut Inline, value: &str, source: &str, offset: usize, mark: &TextMark) {
    if value.is_empty() {
        return;
    }
    // A soft line break draws as a space; both are one byte, so offsets hold.
    let shown = if value == "\n" {
        value.to_owned()
    } else {
        value.replace('\n', " ")
    };
    for (l, s) in align(value, source) {
        out.pieces.push((
            out.len + l.start..out.len + l.end,
            offset + s.start..offset + s.end,
        ));
    }
    out.para
        .push(InlineNode::new(shown.clone()).marks(vec![(0..shown.len(), mark.clone())]));
    out.len += shown.len();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(p: &Projection) -> Vec<(String, bool, usize)> {
        p.segments
            .iter()
            .map(|s| (format!("{:?}", s.source), s.raw, s.blocks.len()))
            .collect()
    }

    #[test]
    fn segments_are_whole_lines() {
        let text = "# T\n\npara one\npara two\n\n- a\n- b\n";
        let p = project(text, Some(0));
        assert_eq!(
            kinds(&p),
            [
                ("0..3".into(), true, 1),    // heading, with the caret: raw
                ("4..4".into(), true, 1),    // blank line, drawn empty
                ("5..22".into(), false, 1),  // paragraph, formatted
                ("23..23".into(), true, 1),  // blank
                ("24..31".into(), false, 1), // list
                ("32..32".into(), true, 1),  // empty last line
            ]
        );
    }

    #[test]
    fn caret_on_a_blank_line_shows_it() {
        let p = project("a\n\nb", Some(2));
        assert_eq!(kinds(&p)[1], ("2..2".into(), true, 1));
        let p = project("", Some(0));
        assert_eq!(kinds(&p), [("0..0".into(), true, 1)]);
        let p = project("a\n", Some(2));
        assert_eq!(kinds(&p)[1], ("2..2".into(), true, 1));
    }

    #[test]
    fn raw_leaves_are_the_source() {
        let text = "x\n\n- a\n- b";
        let p = project(text, Some(5));
        let raw: Vec<_> = p.leaves.iter().map(|l| &text[l.source.clone()]).collect();
        assert_eq!(raw, ["x", "", "- a", "- b"]);
        let leaf = &p.leaves[3];
        assert_eq!(leaf.to_leaf(text.len()), Some(3));
        assert_eq!(leaf.to_source(1), 8);
    }

    #[test]
    fn formatted_leaf_maps_around_hidden_markup() {
        let text = "x\n\nsee **bold** and [[Page|label]] end";
        let p = project(text, Some(0));
        let leaf = p.leaves.last().unwrap();
        // Rendered: "see bold and label end"
        assert_eq!(leaf.to_source(4), text.find("bold").unwrap());
        assert_eq!(leaf.to_source(13), text.find("label").unwrap());
        assert_eq!(leaf.to_leaf(text.find("end").unwrap()), Some(19));
        assert_eq!(leaf.to_leaf(text.find("**").unwrap()), Some(4));
    }

    #[test]
    fn selection_maps_to_shown_bytes_only() {
        let text = "x\n\na *b* c";
        let p = project(text, Some(0));
        let leaf = p.leaves.last().unwrap();
        // Source "a *b* c" selected whole; rendered "a b c".
        assert_eq!(leaf.leaf_ranges(&(3..text.len())), [0..2, 2..3, 3..5]);
    }

    #[test]
    fn code_block_maps_its_body() {
        let text = "x\n\n```rust\nlet r = 1;\n```";
        let p = project(text, Some(0));
        let leaf = p.leaves.last().unwrap();
        assert_eq!(leaf.to_source(0), text.find("let").unwrap());
    }

    #[test]
    fn unsupported_blocks_stay_raw() {
        let text = "x\n\n| a |\n|---|\n| b |";
        let p = project(text, Some(0));
        assert!(p.segments.last().unwrap().raw);
        assert_eq!(p.segments.last().unwrap().blocks.len(), 3);
    }

    #[test]
    fn keys_are_unique() {
        let text = "# a\n\n- b\n  - c\n\n> d\n\ne";
        let p = project(text, Some(0));
        let mut keys: Vec<_> = p.leaves.iter().map(|l| l.key).collect();
        keys.dedup();
        assert_eq!(keys.len(), p.leaves.len());
    }

    #[test]
    fn reference_definitions_stay_visible() {
        // pulldown-cmark emits no event for a definition; its line must not read as blank.
        let text = "x\n\n[a]: https://a.org\n";
        let p = project(text, Some(0));
        assert_eq!(kinds(&p)[2], ("3..21".into(), true, 1));
    }

    #[test]
    fn link_in_a_tight_list_maps_its_label() {
        let text = "x\n\n- [demo](https://python.org) end";
        let p = project(text, Some(0));
        let leaf = p.leaves.last().unwrap();
        // Rendered: "demo end"
        assert_eq!(leaf.to_source(0), text.find("demo").unwrap());
        assert_eq!(leaf.to_leaf(text.find("end").unwrap()), Some(5));
    }

    #[test]
    fn bare_url_is_marked_as_a_link() {
        let text = "x\n\nsee www.a.org now";
        let p = project(text, Some(0));
        let leaf = p.leaves.last().unwrap();
        assert_eq!(leaf.to_leaf(text.find("now").unwrap()), Some(14));
        let BlockNode::Paragraph(para) = &p.segments.last().unwrap().blocks[0] else {
            panic!("a paragraph")
        };
        assert!(format!("{para:?}").contains("http://www.a.org"));
    }

    #[test]
    fn bare_email_is_marked_as_a_link() {
        let text = "x\n\nask a@b.org now";
        let p = project(text, Some(0));
        let BlockNode::Paragraph(para) = &p.segments.last().unwrap().blocks[0] else {
            panic!("a paragraph")
        };
        assert!(format!("{para:?}").contains("mailto:a@b.org"));
    }

    #[test]
    fn each_blank_line_is_its_own_empty_line() {
        // Return on a blank line must add a line the caret can be seen on.
        let p = project("a\n\n\n\n", Some(4));
        let lines: Vec<_> = kinds(&p).into_iter().skip(1).collect();
        assert_eq!(
            lines,
            [
                ("2..2".into(), true, 1),
                ("3..3".into(), true, 1),
                ("4..4".into(), true, 1),
                ("5..5".into(), true, 1),
            ]
        );
    }

    #[test]
    fn without_a_caret_every_block_is_formatted() {
        let text = "# Title\n\nbody **bold**\n";
        let p = project(text, None);
        let shown: Vec<_> = p
            .segments
            .iter()
            .filter(|s| !s.source.is_empty())
            .map(|s| s.raw)
            .collect();
        assert_eq!(shown, [false, false]);
        // With the caret in the heading, the heading shows its markdown.
        assert!(project(text, Some(0)).segments[0].raw);
    }

    #[test]
    fn a_local_image_is_one_atom_and_a_remote_one_is_text() {
        let text = "x\n\na ![p](img/p.png) b ![r](https://x.org/r.png)";
        let p = project(text, Some(0));
        let leaf = p.leaves.last().unwrap();
        // Rendered: "a " + atom (3 bytes) + " b " + "r".
        assert_eq!(leaf.to_leaf(text.find(" b").unwrap()), Some(5));
        // The remote image's alt text maps back to where it is written.
        assert_eq!(leaf.to_source(8), text.find("[r]").unwrap() + 1);
        assert!(has_scheme("https://x") && !has_scheme("c:/x") && !has_scheme("img/p.png"));
    }
}
