# GitHub export

How `katib export` and "Export vault for GitHub/as GitHub wiki" write links.
Code: `crates/katib-core/src/github.rs`.

## Repository

GitHub's file view renders CommonMark, where `[[wiki]]` is literal text. Each
wiki link becomes a relative markdown link to the note's file, resolved as
katib resolves it. Headings take GitHub's slug (`#second-heading`). Markdown
links and files are copied unchanged.

## Wiki

Checked on a live GitHub wiki on 2026-11-26, with a probe of 18 link forms:

| Probe | Result |
|-|-|
| `sub/Deep.md` | served at `/wiki/Deep`; folders are not in page URLs |
| `[[sub/Deep]]`, `[[/sub/Deep]]` | broken: link to `sub-Deep`, `-sub-Deep` |
| `[[Deep]]` from the root | works |
| `[[Twin]]` with `Twin.md` and `sub/Twin.md` | the root one, from either folder |
| `[[label\|Page]]` | works; label first, unlike katib |
| `[[Page#heading]]`, `[x](Page#heading)` | both work |
| `[x](Page)` | works: relative to `/wiki/` |
| `[x](Page.md)` | serves the raw source |
| `![i](images/a.svg)` from `sub/Deep.md` | works; `../images/a.svg` does not |
| YAML front matter | drawn as a rule and a `title: ...` heading |

So a wiki export:

- writes every note at the top level, under a page name unique ignoring case:
  its file name, a folder page its folder's name, the root README `Home`.
  Clashes take the id with `/` as `-`.
- writes links as markdown links to page names, `[label](Page#slug)`, one
  syntax for both exports.
- writes file paths from the wiki root, since every page's URL is there.
- drops front matter, writing its `title` as a level-one heading.
- writes `Home.md`, a list of every note, when no note takes the name
  `Home`. GitHub shows `Home` as the wiki's front page.

## Destination

Empty apart from dot-entries, or a git work tree with nothing uncommitted or
ignored, whose other entries are removed first. A wiki clone is the second
kind, so notes deleted since the last export are deleted there too.
