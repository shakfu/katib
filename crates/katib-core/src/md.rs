//! A markdown tree with source byte ranges, built from pulldown-cmark events.
//!
//! pulldown-cmark over markdown-rs: 0.20 ms against 6.90 ms on a 32 KiB note,
//! and the editor re-parses on every keystroke (docs/dev/editor-spike.md).

use std::ops::Range;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, LinkType, Options, Parser, Tag};

#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub kind: Kind,
    /// Source bytes, markup included. Block ranges exclude trailing whitespace.
    pub range: Range<usize>,
    pub children: Vec<Node>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Paragraph,
    Heading(u8),
    Blockquote,
    List {
        start: Option<u64>,
    },
    Item {
        checked: Option<bool>,
        /// Whether the item's text is in paragraphs (a loose list).
        spread: bool,
    },
    CodeBlock {
        lang: Option<String>,
        value: String,
    },
    Rule,
    FrontMatter,
    /// A GFM table: its head (one row of cells), then its body rows.
    Table {
        aligns: Vec<Align>,
    },
    TableHead,
    TableRow,
    /// Holds inlines, as a paragraph does.
    TableCell,
    /// A block with no variant here: HTML, footnote definitions.
    Other,
    /// Adjacent text and soft breaks, merged; the value has escapes resolved.
    Text(String),
    Emphasis,
    Strong,
    Strikethrough,
    Link {
        url: String,
    },
    Image {
        url: String,
        alt: String,
    },
    Code(String),
    Break,
    /// An inline with no variant here (HTML, footnote references), shown as source.
    Raw,
}

/// A table column's alignment, from its delimiter row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    None,
    Left,
    Center,
    Right,
}

impl Kind {
    fn is_inline(&self) -> bool {
        matches!(
            self,
            Kind::Text(_)
                | Kind::Emphasis
                | Kind::Strong
                | Kind::Strikethrough
                | Kind::Link { .. }
                | Kind::Image { .. }
                | Kind::Code(_)
                | Kind::Break
                | Kind::Raw
        )
    }
}

impl Node {
    fn new(kind: Kind, range: Range<usize>) -> Self {
        Self {
            kind,
            range,
            children: Vec::new(),
        }
    }

    /// The text a reader sees, without markup.
    pub fn plain_text(&self) -> String {
        match &self.kind {
            Kind::Text(v) | Kind::Code(v) => v.clone(),
            Kind::Image { alt, .. } => alt.clone(),
            _ => self.children.iter().map(Node::plain_text).collect(),
        }
    }
}

/// Parse `source` into its top-level blocks. GFM tables, strikethrough, task
/// lists and footnotes are on, as is YAML front matter.
pub fn parse(source: &str) -> Vec<Node> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    let mut stack = vec![Node::new(Kind::Other, 0..source.len())];
    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        let leaf = match event {
            Event::Start(tag) => {
                stack.push(Node::new(kind(tag), range));
                continue;
            }
            Event::End(_) => {
                let node = stack.pop().expect("events nest");
                let node = finish(node, source);
                stack.last_mut().expect("root stays").children.push(node);
                continue;
            }
            Event::Text(t) => {
                push_text(stack.last_mut().expect("root stays"), &t, range);
                continue;
            }
            Event::SoftBreak => {
                push_text(stack.last_mut().expect("root stays"), "\n", range);
                continue;
            }
            Event::TaskListMarker(checked) => {
                if let Some(Kind::Item { checked: c, .. }) = stack
                    .iter_mut()
                    .rev()
                    .map(|n| &mut n.kind)
                    .find(|k| matches!(k, Kind::Item { .. }))
                {
                    *c = Some(checked);
                }
                continue;
            }
            Event::Code(t) => Node::new(Kind::Code(t.into_string()), range),
            Event::HardBreak => Node::new(Kind::Break, range),
            Event::Rule => Node::new(Kind::Rule, trim_end(source, range)),
            Event::Html(_)
            | Event::InlineHtml(_)
            | Event::FootnoteReference(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => Node::new(Kind::Raw, range),
        };
        stack.last_mut().expect("root stays").children.push(leaf);
    }
    stack.pop().map(|root| root.children).unwrap_or_default()
}

fn kind(tag: Tag) -> Kind {
    match tag {
        Tag::Paragraph => Kind::Paragraph,
        Tag::Heading { level, .. } => Kind::Heading(level as u8),
        Tag::BlockQuote(_) => Kind::Blockquote,
        Tag::CodeBlock(code) => Kind::CodeBlock {
            lang: match code {
                CodeBlockKind::Fenced(info) => info.split_whitespace().next().map(str::to_owned),
                CodeBlockKind::Indented => None,
            },
            value: String::new(),
        },
        Tag::List(start) => Kind::List { start },
        Tag::Item => Kind::Item {
            checked: None,
            spread: false,
        },
        Tag::Emphasis => Kind::Emphasis,
        Tag::Strong => Kind::Strong,
        Tag::Strikethrough => Kind::Strikethrough,
        Tag::Link {
            link_type,
            dest_url,
            ..
        } => Kind::Link {
            // pulldown-cmark drops the scheme of `<a@b.c>`; restore it so it reads as external.
            url: match link_type {
                LinkType::Email => format!("mailto:{dest_url}"),
                _ => dest_url.into_string(),
            },
        },
        Tag::Image { dest_url, .. } => Kind::Image {
            url: dest_url.into_string(),
            alt: String::new(),
        },
        Tag::MetadataBlock(_) => Kind::FrontMatter,
        Tag::Table(aligns) => Kind::Table {
            aligns: aligns
                .into_iter()
                .map(|a| match a {
                    Alignment::None => Align::None,
                    Alignment::Left => Align::Left,
                    Alignment::Center => Align::Center,
                    Alignment::Right => Align::Right,
                })
                .collect(),
        },
        Tag::TableHead => Kind::TableHead,
        Tag::TableRow => Kind::TableRow,
        Tag::TableCell => Kind::TableCell,
        Tag::Superscript | Tag::Subscript => Kind::Raw,
        Tag::HtmlBlock
        | Tag::FootnoteDefinition(_)
        | Tag::DefinitionList
        | Tag::DefinitionListTitle
        | Tag::DefinitionListDefinition => Kind::Other,
    }
}

/// Merge text into a preceding text node, as markdown-rs and CommonMark's AST do.
fn push_text(parent: &mut Node, text: &str, range: Range<usize>) {
    if let Some(Node {
        kind: Kind::Text(value),
        range: r,
        ..
    }) = parent.children.last_mut()
    {
        value.push_str(text);
        r.end = range.end;
    } else {
        parent
            .children
            .push(Node::new(Kind::Text(text.to_owned()), range));
    }
}

fn finish(mut node: Node, source: &str) -> Node {
    match &mut node.kind {
        Kind::CodeBlock { value, .. } => {
            *value = std::mem::take(&mut node.children)
                .iter()
                .map(Node::plain_text)
                .collect();
        }
        Kind::Image { alt, .. } => {
            *alt = std::mem::take(&mut node.children)
                .iter()
                .map(Node::plain_text)
                .collect();
        }
        Kind::FrontMatter => node.children.clear(),
        Kind::Item { spread, .. } => {
            *spread = node.children.iter().any(|c| c.kind == Kind::Paragraph);
            node.children = wrap_inlines(std::mem::take(&mut node.children));
        }
        _ => {}
    }
    if !node.kind.is_inline() {
        node.range = trim_end(source, node.range);
    }
    node
}

/// A tight list item holds inlines directly; wrap each run in a paragraph.
fn wrap_inlines(children: Vec<Node>) -> Vec<Node> {
    let mut out: Vec<Node> = Vec::new();
    let mut open = false;
    for child in children {
        if !child.kind.is_inline() {
            open = false;
            out.push(child);
            continue;
        }
        match out.last_mut() {
            Some(para) if open => {
                para.range.end = child.range.end;
                para.children.push(child);
            }
            _ => {
                let mut para = Node::new(Kind::Paragraph, child.range.clone());
                para.children.push(child);
                out.push(para);
                open = true;
            }
        }
    }
    out
}

fn trim_end(source: &str, range: Range<usize>) -> Range<usize> {
    let end = range.start + source[range.clone()].trim_end().len();
    range.start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(nodes: &[Node], source: &str) -> Vec<String> {
        nodes
            .iter()
            .map(|n| format!("{:?} {:?}", n.kind, &source[n.range.clone()]))
            .collect()
    }

    #[test]
    fn block_ranges_exclude_trailing_newlines() {
        let s = "# T\n\npara\nnext\n\n- a\n- b\n\n\n";
        let nodes = parse(s);
        assert_eq!(
            nodes
                .iter()
                .map(|n| &s[n.range.clone()])
                .collect::<Vec<_>>(),
            ["# T", "para\nnext", "- a\n- b"]
        );
    }

    #[test]
    fn text_merges_across_brackets_escapes_and_soft_breaks() {
        let s = "a [[Page]] \\*b\nc";
        let para = &parse(s)[0];
        assert_eq!(
            shape(&para.children, s),
            [r#"Text("a [[Page]] *b\nc") "a [[Page]] \\*b\nc""#]
        );
    }

    #[test]
    fn tight_items_get_paragraphs() {
        let s = "- [x] done *now*\n- b\n";
        let list = &parse(s)[0];
        let item = &list.children[0];
        assert_eq!(
            item.kind,
            Kind::Item {
                checked: Some(true),
                spread: false
            }
        );
        assert_eq!(shape(&item.children, s), [r#"Paragraph "done *now*""#]);
    }

    #[test]
    fn code_block_value_and_lang() {
        let s = "```rust title\nlet a;\n```\n";
        assert_eq!(
            parse(s)[0].kind,
            Kind::CodeBlock {
                lang: Some("rust".into()),
                value: "let a;\n".into()
            }
        );
    }

    #[test]
    fn tables_have_aligned_columns_and_inline_cells() {
        let s = "| a | *b* |\n|:-|-:|\n| [[c]] | d |\n";
        let table = &parse(s)[0];
        assert_eq!(
            table.kind,
            Kind::Table {
                aligns: vec![Align::Left, Align::Right]
            }
        );
        let rows: Vec<Vec<String>> = table
            .children
            .iter()
            .map(|row| {
                row.children
                    .iter()
                    .map(|cell| format!("{:?} {}", cell.kind, s[cell.range.clone()].trim()))
                    .collect()
            })
            .collect();
        assert_eq!(
            rows,
            [
                ["TableCell a", "TableCell *b*"],
                ["TableCell [[c]]", "TableCell d"]
            ]
        );
        assert_eq!(table.children[0].kind, Kind::TableHead);
        assert_eq!(table.children[1].kind, Kind::TableRow);
        assert_eq!(
            table.children[0].children[1].children[0].kind,
            Kind::Emphasis
        );
    }

    #[test]
    fn email_autolink_has_mailto() {
        let para = &parse("<a@b.c>")[0];
        assert_eq!(
            para.children[0].kind,
            Kind::Link {
                url: "mailto:a@b.c".into()
            }
        );
    }
}
