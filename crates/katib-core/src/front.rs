//! Front matter fields: reading the few katib uses, and setting one in place.
//!
//! Not a YAML parser. It reads top-level `key: value` lines, and `tags` as a
//! flow list (`[a, b]`), a comma list, or a block list (`- a` lines). Other
//! lines are kept as they are when a field is set.

/// The fields katib reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Front {
    pub title: Option<String>,
    pub tags: Vec<String>,
    /// `type`; `task` makes the note a task.
    pub kind: Option<String>,
    pub status: Option<String>,
    pub priority: Option<String>,
    pub due: Option<String>,
}

/// The fields of a front matter block, delimiters included or not.
pub fn read(block: &str) -> Front {
    let mut front = Front::default();
    let mut lines = block.lines().peekable();
    while let Some(line) = lines.next() {
        if line.starts_with([' ', '\t', '-']) || is_delimiter(line) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        let scalar = || unquote(value).filter(|v| !v.is_empty());
        match key.trim() {
            "title" => front.title = scalar(),
            "type" => front.kind = scalar(),
            "status" => front.status = scalar(),
            "priority" => front.priority = scalar(),
            "due" => front.due = scalar(),
            "tags" if value.is_empty() => {
                while let Some(item) = lines.peek().and_then(|l| l.trim_start().strip_prefix("- "))
                {
                    front
                        .tags
                        .extend(unquote(item.trim()).filter(|t| !t.is_empty()));
                    lines.next();
                }
            }
            "tags" => {
                let inner = value
                    .strip_prefix('[')
                    .and_then(|v| v.strip_suffix(']'))
                    .unwrap_or(value);
                front.tags = split_flow(inner)
                    .into_iter()
                    .filter_map(|t| unquote(t.trim()))
                    .filter(|t| !t.is_empty())
                    .collect();
            }
            _ => {}
        }
    }
    front
}

fn is_delimiter(line: &str) -> bool {
    matches!(line.trim_end(), "---" | "...")
}

/// The items of a flow list's inside, split at commas outside quotes.
fn split_flow(inner: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut quote, mut escaped) = (0, None, false);
    for (i, c) in inner.char_indices() {
        match (quote, c) {
            (Some('"'), '\\') if !escaped => {
                escaped = true;
                continue;
            }
            (Some(q), c) if c == q && !escaped => quote = None,
            (None, '"' | '\'') => quote = Some(c),
            (None, ',') => {
                out.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        escaped = false;
    }
    out.push(&inner[start..]);
    out
}

/// A scalar's text: quotes removed, and escapes resolved in double quotes.
fn unquote(value: &str) -> Option<String> {
    if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            out.push(if c == '\\' {
                chars.next().unwrap_or(c)
            } else {
                c
            });
        }
        return Some(out);
    }
    let v = value
        .strip_prefix('\'')
        .and_then(|v| v.strip_suffix('\''))
        .unwrap_or(value);
    Some(v.to_owned())
}

/// A scalar written so YAML reads it back as the same string.
pub fn scalar(value: &str) -> String {
    let plain = !value.is_empty()
        && !value.starts_with([
            ' ', '-', '[', '{', '"', '\'', '#', '&', '*', '!', '|', '>', '%', '@',
        ])
        && !value.ends_with(' ')
        && !value.contains(": ")
        && !value.contains(" #")
        && !value.contains([',', '[', ']']);
    if plain {
        value.to_owned()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// A flow list of scalars: `[a, "b c"]`.
pub fn list(values: &[String]) -> String {
    let items: Vec<String> = values.iter().map(|v| scalar(v)).collect();
    format!("[{}]", items.join(", "))
}

/// `source` with top-level front matter `key` set to `value` (already
/// written as YAML), or removed when `value` is `None`. A block list under the
/// key is replaced too. A note without front matter gets a block.
pub fn set(source: &str, key: &str, value: Option<&str>) -> String {
    let line = value.map(|v| format!("{key}: {v}"));
    let Some(body_start) = source.strip_prefix("---\n").map(|_| 4) else {
        return match line {
            Some(line) => format!("---\n{line}\n---\n{source}"),
            None => source.to_owned(),
        };
    };
    // The closing delimiter's line start; no closing line means no front matter.
    let mut offset = body_start;
    let mut close = None;
    for l in source[body_start..].split_inclusive('\n') {
        if is_delimiter(l.trim_end_matches('\n')) {
            close = Some(offset);
            break;
        }
        offset += l.len();
    }
    let Some(close) = close else {
        return match line {
            Some(line) => format!("---\n{line}\n---\n{source}"),
            None => source.to_owned(),
        };
    };
    let mut out: Vec<String> = Vec::new();
    let mut lines = source[body_start..close].lines().peekable();
    let mut written = false;
    while let Some(l) = lines.next() {
        let is_key = !l.starts_with([' ', '\t', '-'])
            && l.split_once(':').is_some_and(|(k, _)| k.trim() == key);
        if !is_key {
            out.push(l.to_owned());
            continue;
        }
        // Drop the old value's block list lines.
        while lines
            .peek()
            .is_some_and(|n| n.trim_start().starts_with("- ") || n.starts_with([' ', '\t']))
        {
            lines.next();
        }
        if let Some(line) = &line
            && !written
        {
            out.push(line.clone());
            written = true;
        }
    }
    if let Some(line) = line
        && !written
    {
        out.push(line);
    }
    if out.iter().all(|l| l.trim().is_empty()) {
        // Nothing left: drop the block, closing line included.
        let after = source[close..]
            .find('\n')
            .map_or(source.len(), |i| close + i + 1);
        return source[after..].to_owned();
    }
    format!("---\n{}\n{}", out.join("\n"), &source[close..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_fields_and_every_tag_form() {
        let f = read(
            "---\ntitle: \"T\"\ntype: task\nstatus: doing\ndue: 2026-12-01\ntags: [a, \"b c\"]\n---\n",
        );
        assert_eq!(f.title.as_deref(), Some("T"));
        assert_eq!(f.kind.as_deref(), Some("task"));
        assert_eq!(f.status.as_deref(), Some("doing"));
        assert_eq!(f.due.as_deref(), Some("2026-12-01"));
        assert_eq!(f.tags, ["a", "b c"]);
        assert_eq!(read("tags: x, y\n").tags, ["x", "y"]);
        assert_eq!(read("tags:\n  - x\n  - 'y'\ntitle: z\n").tags, ["x", "y"]);
        assert_eq!(read("tags:\ntitle: z\n").title.as_deref(), Some("z"));
        // A nested key is not top-level.
        assert_eq!(read("meta:\n  title: no\n").title, None);
    }

    #[test]
    fn set_replaces_adds_and_removes() {
        let src = "---\ntitle: T\ntags:\n  - old\n---\n# Body\n";
        assert_eq!(
            set(src, "tags", Some("[a, b]")),
            "---\ntitle: T\ntags: [a, b]\n---\n# Body\n"
        );
        assert_eq!(
            set(src, "status", Some("done")),
            "---\ntitle: T\ntags:\n  - old\nstatus: done\n---\n# Body\n"
        );
        assert_eq!(set(src, "tags", None), "---\ntitle: T\n---\n# Body\n");
        assert_eq!(
            set("# Body\n", "tags", Some("[a]")),
            "---\ntags: [a]\n---\n# Body\n"
        );
        assert_eq!(set("# Body\n", "tags", None), "# Body\n");
        assert_eq!(set("---\ntags: [a]\n---\n# B\n", "tags", None), "# B\n");
        // An unclosed block is body text, so a new block goes in front.
        assert_eq!(set("---\nx\n", "a", Some("1")), "---\na: 1\n---\n---\nx\n");
    }

    #[test]
    fn scalars_are_quoted_when_needed() {
        assert_eq!(scalar("plain tag"), "plain tag");
        assert_eq!(scalar("a: b"), "\"a: b\"");
        assert_eq!(scalar("#x"), "\"#x\"");
        assert_eq!(list(&["a".into(), "b, c".into()]), "[a, \"b, c\"]");
        let tags = ["x y".to_owned(), "b, c".to_owned(), "q\"t".to_owned()];
        let back = read(&format!("tags: {}\n", list(&tags)));
        assert_eq!(back.tags, tags);
    }
}
