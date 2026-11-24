use std::ops::Range;

use crate::{
    front::{self, Front},
    md::{self, Kind, Node},
};

/// A parsed note. Byte ranges index into the source it was parsed from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    /// The front matter's `title`, else the first level-one heading.
    pub heading_title: Option<String>,
    pub links: Vec<Link>,
    pub front: Front,
    /// Checklist items, `- [ ] text`, in source order.
    pub items: Vec<Item>,
    /// Every heading's text, in source order.
    pub headings: Vec<String>,
}

/// A checklist item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// The byte offset of its `[ ]` or `[x]`.
    pub marker: usize,
    /// The item's line after the marker, trimmed.
    pub text: String,
    pub done: bool,
    /// An inline `due:YYYY-MM-DD`.
    pub due: Option<String>,
}

/// The date of a `due:YYYY-MM-DD` word in `text`.
pub fn due_word(text: &str) -> Option<String> {
    text.split_whitespace().find_map(|w| {
        let d = w.strip_prefix("due:")?;
        let b = d.as_bytes();
        let ok = b.len() == 10
            && b[4] == b'-'
            && b[7] == b'-'
            && b.iter()
                .enumerate()
                .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
        ok.then(|| d.to_owned())
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    /// `[[target#fragment|label]]`
    Wiki,
    /// `[label](target#fragment)` to a relative path.
    Markdown,
    /// A markdown link with a URL scheme, such as `https:` or `mailto:`.
    External,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub kind: LinkKind,
    pub target: String,
    pub fragment: Option<String>,
    pub label: Option<String>,
    pub range: Range<usize>,
}

/// Parse a note's markdown source.
pub fn parse(source: &str) -> Note {
    let mut note = Note {
        heading_title: None,
        links: Vec::new(),
        front: Front::default(),
        items: Vec::new(),
        headings: Vec::new(),
    };
    let nodes = md::parse(source);
    if let Some(Node {
        kind: Kind::FrontMatter,
        range,
        ..
    }) = nodes.first()
    {
        note.front = front::read(&source[range.clone()]);
        note.heading_title = note.front.title.clone();
    }
    walk(&nodes, source, false, &mut note);
    note.links.sort_by_key(|l| l.range.start);
    note
}

/// The byte offset of the heading a link `fragment` names: its text, ignoring
/// case, or its slug (`My Heading` as `my-heading`). The first match wins.
pub fn heading_offset(source: &str, fragment: &str) -> Option<usize> {
    fn find(nodes: &[Node], want: &str, want_slug: &str) -> Option<usize> {
        nodes.iter().find_map(|n| match n.kind {
            Kind::Heading(_) => {
                let text = n.plain_text();
                let text = text.trim();
                (text.to_lowercase() == want || slug(text) == want_slug).then_some(n.range.start)
            }
            _ => find(&n.children, want, want_slug),
        })
    }
    let want = fragment.trim().to_lowercase();
    find(&md::parse(source), &want, &slug(&want))
}

/// GitHub-style heading slug: lower case, spaces and hyphens to `-`, other
/// punctuation dropped.
fn slug(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            c if c.is_alphanumeric() || c == '_' => Some(c.to_lowercase().next().unwrap_or(c)),
            ' ' | '-' => Some('-'),
            _ => None,
        })
        .collect()
}

fn walk(nodes: &[Node], source: &str, in_link: bool, note: &mut Note) {
    for node in nodes {
        match &node.kind {
            Kind::Heading(level) => {
                let text = node.plain_text().trim().to_owned();
                if *level == 1 && note.heading_title.is_none() {
                    note.heading_title = Some(text.clone());
                }
                note.headings.push(text);
            }
            Kind::Item {
                checked: Some(done),
                ..
            } => note.items.extend(item(source, node.range.clone(), *done)),
            Kind::Link { url } => {
                note.links
                    .push(markdown_link(url, node.plain_text(), node.range.clone()));
            }
            // An image is a link to a file: moves rewrite it, attachments count it.
            Kind::Image { url, alt } => {
                note.links
                    .push(markdown_link(url, alt.clone(), node.range.clone()));
            }
            // The value has escapes resolved; scan the source so ranges stay exact.
            Kind::Text(_) => text_links(
                &source[node.range.clone()],
                node.range.start,
                !in_link,
                &mut note.links,
            ),
            _ => {}
        }
        let in_link = in_link || matches!(node.kind, Kind::Link { .. });
        walk(&node.children, source, in_link, note);
    }
}

/// The checklist item whose list item spans `range`: its marker is the first
/// `[ ]`, `[x]` or `[X]` on its first line.
fn item(source: &str, range: Range<usize>, done: bool) -> Option<Item> {
    let s = &source[range.clone()];
    let line = &s[..s.find('\n').unwrap_or(s.len())];
    let at = ["[ ]", "[x]", "[X]"]
        .iter()
        .filter_map(|m| line.find(m))
        .min()?;
    let text = line[at + 3..].trim().to_owned();
    Some(Item {
        marker: range.start + at,
        due: due_word(&text),
        text,
        done,
    })
}

fn markdown_link(url: &str, label: String, range: Range<usize>) -> Link {
    let kind = if has_scheme(url) {
        LinkKind::External
    } else {
        LinkKind::Markdown
    };
    let (target, fragment) = match (kind, url.split_once('#')) {
        (LinkKind::Markdown, Some((t, f))) => (t.to_owned(), Some(f.to_owned())),
        _ => (url.to_owned(), None),
    };
    Link {
        kind,
        target,
        fragment,
        label: (!label.is_empty()).then_some(label),
        range,
    }
}

/// RFC 3986 scheme: a letter, then letters, digits, `+`, `-` or `.`, then `:`.
fn has_scheme(url: &str) -> bool {
    let Some((scheme, _)) = url.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Append the links in `text`, a slice of a note starting at byte `offset`:
/// wiki links, then, with `urls`, the bare URLs and email addresses outside
/// them. Sorted by position.
pub fn text_links(text: &str, offset: usize, urls: bool, out: &mut Vec<Link>) {
    let first = out.len();
    wiki_links(text, offset, out);
    if urls {
        scan_gaps(text, offset, first, out, bare_urls);
        scan_gaps(text, offset, first, out, emails);
    }
}

/// Run `scan` on the parts of `text` no link in `out[first..]` covers, then sort those links.
fn scan_gaps(
    text: &str,
    offset: usize,
    first: usize,
    out: &mut Vec<Link>,
    scan: fn(&str, usize, &mut Vec<Link>),
) {
    out[first..].sort_by_key(|l| l.range.start);
    let taken: Vec<_> = out[first..].iter().map(|l| l.range.clone()).collect();
    let mut at = offset;
    for r in taken {
        scan(&text[at - offset..r.start - offset], at, out);
        at = r.end;
    }
    scan(&text[at - offset..], at, out);
    out[first..].sort_by_key(|l| l.range.start);
}

/// Append GFM email autolinks: `[A-Za-z0-9._+-]+@` and a dotted domain of
/// `[A-Za-z0-9_-]` that does not end in `-` or `_`; the target gains `mailto:`.
fn emails(text: &str, offset: usize, out: &mut Vec<Link>) {
    let local = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_' | b'+');
    let domain = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_');
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(at) = text[from..].find('@').map(|i| from + i) {
        from = at + 1;
        let start = bytes[..at]
            .iter()
            .rposition(|&c| !local(c))
            .map_or(0, |i| i + 1);
        let mut end = bytes[at + 1..]
            .iter()
            .position(|&c| !domain(c))
            .map_or(text.len(), |i| at + 1 + i);
        // A trailing period ends the sentence, not the address.
        while end > at + 1 && bytes[end - 1] == b'.' {
            end -= 1;
        }
        let host = &text[at + 1..end];
        if start == at
            || !host.contains('.')
            || host.split('.').any(str::is_empty)
            || host.ends_with(['-', '_'])
        {
            continue;
        }
        out.push(Link {
            kind: LinkKind::External,
            target: format!("mailto:{}", &text[start..end]),
            fragment: None,
            label: None,
            range: offset + start..offset + end,
        });
        from = end;
    }
}

/// Append GFM autolink literals: `http://`, `https://` or `www.` and a valid
/// domain, after a line start, whitespace or one of `*_~(`.
/// <https://github.github.com/gfm/#autolinks-extension->
fn bare_urls(text: &str, offset: usize, out: &mut Vec<Link>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < text.len() {
        let starts = i == 0
            || matches!(
                bytes[i - 1],
                b' ' | b'\t' | b'\n' | b'*' | b'_' | b'~' | b'('
            );
        let prefix = ["https://", "http://", "www."].into_iter().find(|p| {
            bytes.len() - i >= p.len() && bytes[i..i + p.len()].eq_ignore_ascii_case(p.as_bytes())
        });
        if let (true, Some(prefix)) = (starts, prefix)
            && let Some(len) = autolink_len(&text[i..], prefix)
        {
            let url = &text[i..i + len];
            out.push(Link {
                kind: LinkKind::External,
                target: if prefix == "www." {
                    format!("http://{url}")
                } else {
                    url.to_owned()
                },
                fragment: None,
                label: None,
                range: offset + i..offset + i + len,
            });
            i += len;
            continue;
        }
        i += text[i..].chars().next().map_or(1, char::len_utf8);
    }
}

/// The length of the autolink at the start of `text`, if its domain is valid.
fn autolink_len(text: &str, prefix: &str) -> Option<usize> {
    let run_end = |from: usize| {
        text[from..]
            .find(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.')))
            .map_or(text.len(), |i| from + i)
    };
    let mut domain_from = if prefix == "www." { 0 } else { prefix.len() };
    let mut domain_end = run_end(domain_from);
    // `https://user@host`: skip the user part (RFC 3986 userinfo), which GFM does not.
    if prefix != "www." && text[domain_end..].starts_with('@') {
        domain_from = domain_end + 1;
        domain_end = run_end(domain_from);
    }
    // A valid domain: non-empty segments, at least one period, no `_` in the last two.
    let segments: Vec<_> = text[domain_from..domain_end]
        .trim_end_matches('.')
        .split('.')
        .collect();
    if segments.len() < 2
        || segments.iter().any(|s| s.is_empty())
        || segments[segments.len() - 2..]
            .iter()
            .any(|s| s.contains('_'))
    {
        return None;
    }
    let mut end = text[domain_end..]
        .find(|c: char| c.is_whitespace() || c == '<')
        .map_or(text.len(), |i| domain_end + i);
    // Trailing punctuation, an unbalanced `)` and an entity reference are not part of it.
    loop {
        let url = &text[..end];
        let trimmed = if url.ends_with(['?', '!', '.', ',', ':', '*', '_', '~'])
            || (url.ends_with(')') && url.matches(')').count() > url.matches('(').count())
        {
            end - 1
        } else if url.ends_with(';')
            && let Some(amp) = url.rfind('&')
            && url[amp + 1..end - 1]
                .chars()
                .all(|c| c.is_ascii_alphanumeric())
            && amp + 1 < end - 1
        {
            amp
        } else {
            break;
        };
        end = trimmed;
    }
    (end > prefix.len()).then_some(end)
}

fn wiki_links(text: &str, offset: usize, out: &mut Vec<Link>) {
    let mut rest = 0;
    while let Some(open) = text[rest..].find("[[") {
        let start = rest + open;
        let inner_start = start + 2;
        let Some(close) = text[inner_start..].find("]]") else {
            return;
        };
        let inner = &text[inner_start..inner_start + close];
        let end = inner_start + close + 2;
        rest = end;
        if inner.contains('[') || inner.trim().is_empty() {
            continue;
        }
        let (reference, label) = match inner.split_once('|') {
            Some((r, l)) => (r, Some(l.trim().to_owned())),
            None => (inner, None),
        };
        let (target, fragment) = match reference.split_once('#') {
            Some((t, f)) => (t.trim().to_owned(), Some(f.trim().to_owned())),
            None => (reference.trim().to_owned(), None),
        };
        out.push(Link {
            kind: LinkKind::Wiki,
            target,
            fragment,
            label: label.filter(|l| !l.is_empty()),
            range: offset + start..offset + end,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(
        kind: LinkKind,
        target: &str,
        fragment: Option<&str>,
        label: Option<&str>,
    ) -> (LinkKind, String, Option<String>, Option<String>) {
        (
            kind,
            target.into(),
            fragment.map(Into::into),
            label.map(Into::into),
        )
    }

    fn links(source: &str) -> Vec<(LinkKind, String, Option<String>, Option<String>)> {
        parse(source)
            .links
            .into_iter()
            .map(|l| (l.kind, l.target, l.fragment, l.label))
            .collect()
    }

    #[test]
    fn title_is_first_level_one_heading() {
        assert_eq!(
            parse("intro\n\n## Sub\n\n# Main *title*\n\n# Second")
                .heading_title
                .as_deref(),
            Some("Main title")
        );
        assert_eq!(parse("## only h2").heading_title, None);
    }

    #[test]
    fn front_matter_is_not_body() {
        let note = parse("---\ntags: x\nsee: \"[[Hidden]]\"\n---\n# Body\n");
        assert_eq!(note.heading_title.as_deref(), Some("Body"));
        assert!(note.links.is_empty());
    }

    #[test]
    fn front_matter_title_comes_first() {
        let title = |s| parse(s).heading_title;
        assert_eq!(
            title("---\ntitle: \"Mine\"\n---\n# Body\n").as_deref(),
            Some("Mine")
        );
        assert_eq!(title("---\ntitle: 'It'\n---\n").as_deref(), Some("It"));
        assert_eq!(
            title("---\nsubtitle: no\ntitle:\n---\n# H\n").as_deref(),
            Some("H")
        );
    }

    #[test]
    fn wiki_link_forms() {
        assert_eq!(
            links("See [[Page]], [[dir/page#Heading|the label]] and [[ #only ]]."),
            vec![
                link(LinkKind::Wiki, "Page", None, None),
                link(
                    LinkKind::Wiki,
                    "dir/page",
                    Some("Heading"),
                    Some("the label")
                ),
                link(LinkKind::Wiki, "", Some("only"), None),
            ]
        );
    }

    #[test]
    fn wiki_link_range_is_exact() {
        let source = "a *b* [[Target|x]] c";
        let note = parse(source);
        assert_eq!(&source[note.links[0].range.clone()], "[[Target|x]]");
    }

    #[test]
    fn code_is_not_scanned() {
        assert!(links("`[[no]]`\n\n```\n[[no]]\n```\n").is_empty());
    }

    #[test]
    fn malformed_wiki_links_are_ignored() {
        assert!(links("[[]] [[ ]] [[open").is_empty());
    }

    #[test]
    fn markdown_links() {
        assert_eq!(
            links("[a](../b.md#rules) [c](https://x.org/p#q) [](dir/)"),
            vec![
                link(LinkKind::Markdown, "../b.md", Some("rules"), Some("a")),
                link(LinkKind::External, "https://x.org/p#q", None, Some("c")),
                link(LinkKind::Markdown, "dir/", None, None),
            ]
        );
    }

    #[test]
    fn scheme_detection() {
        assert!(has_scheme("mailto:a@b"));
        assert!(has_scheme("git+ssh://h/r"));
        assert!(!has_scheme("notes/a:b.md"));
        assert!(!has_scheme("1x:y"));
        assert!(!has_scheme(":y"));
        assert!(!has_scheme("relative.md"));
    }

    #[test]
    fn bare_urls_are_external_links() {
        let source =
            "see https://x.org/a_(b). and (www.y.org/p), not a.www.z.org or http://localhost";
        assert_eq!(
            links(source),
            vec![
                link(LinkKind::External, "https://x.org/a_(b)", None, None),
                link(LinkKind::External, "http://www.y.org/p", None, None),
            ]
        );
        let note = parse(source);
        assert_eq!(&source[note.links[1].range.clone()], "www.y.org/p");
    }

    #[test]
    fn bare_url_edges() {
        assert!(links("www.x_y.org http:// https://x.").is_empty());
        assert_eq!(
            links("**https://x.org/q?a=1&amp;**"),
            vec![link(LinkKind::External, "https://x.org/q?a=1", None, None)]
        );
    }

    #[test]
    fn bare_urls_are_not_found_in_links_code_or_wiki_links() {
        assert_eq!(
            links("[https://x.org](y.md) `https://c.org` [[https://w.org]]"),
            vec![
                link(LinkKind::Markdown, "y.md", None, Some("https://x.org")),
                link(LinkKind::Wiki, "https://w.org", None, None),
            ]
        );
    }

    #[test]
    fn bare_emails_are_mailto_links() {
        let source = "mail a.b+c@x.org. or (me@y.co.uk), not a@b, c@d-, @e.org or f@g..org";
        assert_eq!(
            links(source),
            vec![
                link(LinkKind::External, "mailto:a.b+c@x.org", None, None),
                link(LinkKind::External, "mailto:me@y.co.uk", None, None),
            ]
        );
        assert_eq!(&source[parse(source).links[0].range.clone()], "a.b+c@x.org");
    }

    #[test]
    fn an_address_inside_a_url_is_part_of_the_url() {
        assert_eq!(
            links("https://u@x.org/p and [[me@x.org]]"),
            vec![
                link(LinkKind::External, "https://u@x.org/p", None, None),
                link(LinkKind::Wiki, "me@x.org", None, None),
            ]
        );
    }

    #[test]
    fn heading_offset_by_text_or_slug() {
        let s = "intro\n\n## Set Up, Again\n\n> # Quoted\n";
        let at = s.find("## Set").unwrap();
        assert_eq!(heading_offset(s, "set up, again"), Some(at));
        assert_eq!(heading_offset(s, "set-up-again"), Some(at));
        assert_eq!(
            heading_offset(s, "Quoted"),
            Some(s.find("# Quoted").unwrap())
        );
        assert_eq!(heading_offset(s, "intro"), None);
    }

    #[test]
    fn checklist_items_and_due_dates() {
        let src = "# T\n\n- [ ] write due:2026-12-01\n- [x] done item\n- plain\n  - [X] nested\n";
        let items = parse(src).items;
        let got: Vec<_> = items
            .iter()
            .map(|i| {
                (
                    &src[i.marker..i.marker + 3],
                    i.text.as_str(),
                    i.done,
                    i.due.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("[ ]", "write due:2026-12-01", false, Some("2026-12-01")),
                ("[x]", "done item", true, None),
                ("[X]", "nested", true, None),
            ]
        );
        assert_eq!(due_word("due:2026-1-01"), None);
    }
}
