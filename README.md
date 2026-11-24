# katib

A desktop notes app with wiki links. Notes are markdown files in a folder (a vault). katib syncs the vault across machines through git.

Built in Rust on [gpui-kit](https://gpui-kit.com). Runs on macOS, Linux and Windows. Design: [docs/dev/design.md](docs/dev/design.md). Changes:
[CHANGELOG.md](CHANGELOG.md).

## Principles

- Markdown files are the source of truth. The index is a cache rebuilt from them.

- The vault stays usable without katib: in another editor, with `git log`, on GitHub.

- No edit is lost to sync. A conflict is shown, never resolved silently.

- One user, several desktops. No shared editing, no phone client, no server.

## Features

### Editing

- **Live mode** (default): formatted text; markdown syntax shows only on the caret's block.

- **Raw mode**: highlighted markdown source. Switching keeps the caret position.

- **Guarded saves**: a note saves 1 s after typing stops. A save is refused if the file changed on disk since it was read.

- **Atomic writes** through a temporary file and a rename.

- **Draft recovery**: a failed or conflicting save goes to `.katib/drafts/` and is offered back on reopen.

- **External changes** reload within a second, unless the buffer has unsaved edits.

### Links and search

- `[[Title]]`, `[[path#Heading|label]]`, relative markdown links, bare URLs and email addresses.

- **Backlinks** panel under each note.

- **Link completion** after `[[`, by title or path.

- **Broken links** list, with close matches to retarget and "Create" for a missing note.

- **Full-text search** in SQLite FTS5. A title match (weight 10) ranks above a heading or tag (5) above body text (1).

- **Command palette** to open, search, create notes and run commands.

### Notes and folders

- Create, rename, move and delete notes; rename or move folders.

- A move rewrites every link to the moved notes, including reference-style definitions, and rebases the moved notes' own relative links.

- Delete moves the note to `.katib/trash/`.

- **Tabs**, each with its own autosave and back/forward history. The first tab is a fixed overview.

- **Daily notes** at `<journal folder>/YYYY-MM-DD.md`, from the toolbar calendar.

### Tasks and tags

- **Checklist items**: `- [ ] text`, with optional `due:YYYY-MM-DD`. Click the checkbox in live mode to tick it.

- **Task notes**: front matter `type: task` with `status` (open, doing, done), `priority` and `due`.

- One tasks list, open first, by due date. Overdue shows red; due within 7 days shows amber.

- "Promote" turns a checklist item into a task note.

- **Tags** from front matter `tags`, matched ignoring case. In the palette, `#` lists tags and `#name` lists tagged notes. Inline `#tag` is not read: it collides with headings and issue numbers.

### Overview

- Latest changes, with the last commit's author.

- Tasks.

- Stats: note count, broken links, most-linked notes, orphans, dead ends, tags.

### Attachments

- Paste or drop images and files, or use "Attach file...".

- Stored as `attachments/<hash prefix>/<sha256>-<name>`, so identical bytes are stored once. Files over 50 MB are refused.

- Remote images are not fetched, so reading a note contacts no server.

- "Remove unused attachments..." moves unlinked files to the trash.

- Optional **Git LFS** backend: `[objects] backend = "lfs"`. An unreachable LFS server delays attachments, not notes.

### Sync

Opt-in per vault. katib commits, fetches, merges and pushes with the `git` binary, so your SSH agent, credential helper and git config apply unchanged.

- A cycle starts after 30 s without edits, every 60 s, and on window focus.

- Merges run in memory (`git merge-tree --write-tree`). The working tree never holds conflict markers.

- **Conflicts** open a dialog: keep mine, keep theirs, or keep both.

- Offline retries back off from 60 s to 10 min. The status bar shows the sync state.

- Commits are named `katib: <hostname> <ISO 8601 time>`.

## Requirements

- Rust with edition 2024 support (1.85 or later).

- git 2.38 or later, for sync.

- git-lfs, only for the LFS backend.

## Usage

```sh
make run                      # opens demo/, a sample vault with sync off
make run VAULT=~/notes        # opens another vault
cargo run -p katib -- <vault> # the same, without make
```

A vault is any directory. To sync it, make it the top level of a git repository with a remote, then turn on sync in Settings or in `.katib/config.toml`:

```toml
sync = true

[objects]
backend = "git"   # or "lfs"
```

`.katib/config.toml` is committed, so every clone shares it. `.katib/cache/`, `drafts/` and `trash/` are ignored.

## Keys

`mod` is cmd on macOS and ctrl elsewhere.

| key | action |
|-|-|
| mod-p | command palette |
| mod-e | toggle live and raw mode |
| mod-n | new note |
| mod-s | save |
| mod-t / mod-w | new tab / close tab |
| ctrl-tab, mod-shift-] | next tab |
| ctrl-shift-tab, mod-shift-[ | previous tab |
| ctrl-- / ctrl-shift-- (macOS), alt-left / alt-right | back / forward |
| mod-shift-d | today's daily note |
| mod-\ | toggle sidebar |
| mod-shift-b | toggle backlinks |
| mod-, | settings |
| mod-click on a link | follow it, or create the missing note |

## Development

```sh
make build   # cargo build --workspace
make test    # cargo test --workspace
make check   # rustfmt and clippy, warnings as errors
```

| crate | owns |
|-|-|
| `katib-core` | vault, parsing, link resolution, index, guarded writes, moves, attachments, tasks |
| `katib-editor` | the live mode editor element |
| `katib-sync` | the git sync state machine |
| `katib` | the app: window, tree, palette, tabs, dialogs |

## License

MIT. See [LICENSE](LICENSE).
