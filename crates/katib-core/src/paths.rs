//! Vault-relative paths for markdown links.

/// A vault-relative path for a markdown link `target` written in note `from`.
/// `None` if it climbs above the root. Percent escapes are decoded.
pub fn relative_path(from: &str, target: &str) -> Option<String> {
    let target = percent_decode(target);
    let mut parts: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        let mut dir: Vec<_> = from.split('/').collect();
        dir.pop();
        dir
    };
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_owned())
}

/// The link from note `from` to vault path `path`, relative to `from`'s folder.
pub fn relative_to(from: &str, path: &str) -> String {
    let mut dir: Vec<&str> = from.split('/').collect();
    dir.pop();
    let target: Vec<&str> = path.split('/').collect();
    let common = dir.iter().zip(&target).take_while(|(a, b)| a == b).count();
    let mut parts = vec![".."; dir.len() - common];
    parts.extend(&target[common..]);
    parts.join("/")
}

/// Escape what would end or confuse a link destination written without `<>`.
pub fn percent_encode(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            '%' => out.push_str("%25"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_round_trip() {
        assert_eq!(relative_to("a/b/note", "a/c/x.md"), "../c/x.md");
        assert_eq!(relative_to("note", "a/x.md"), "a/x.md");
        assert_eq!(relative_to("a/note", "x.md"), "../x.md");
        for (from, path) in [("a/b/n", "a/c/x y.md"), ("n", "x.md"), ("a/n", "b/c.png")] {
            let link = percent_encode(&relative_to(from, path));
            assert_eq!(relative_path(from, &link).as_deref(), Some(path));
        }
    }
}
