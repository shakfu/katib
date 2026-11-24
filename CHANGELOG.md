# Changelog

All notable changes to katib. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Design detail is in
[docs/dev/design.md](docs/dev/design.md).

## Unreleased

### Added

- **Note management.** Create, rename, move and delete notes, and rename or move folders, from the tree's context menu or the palette. A move rewrites every link to the moved notes, including reference-style definitions, and rebases the moved notes' own relative links. Delete moves the note to `.katib/trash/`.

- **Attachments.** Paste or drop images and files, or use "Attach file...". Files are stored under `attachments/<hash>/` named by SHA-256, so the same bytes are stored once; files over 50 MB are refused. "Remove unused attachments..." moves unlinked files to the trash. Remote images are not fetched, so reading a note tells no server.

- **Git LFS backend.** With `[objects] backend = "lfs"`, attachments are committed as LFS pointers. Sync fast-forwards with `GIT_LFS_SKIP_SMUDGE=1` and then runs `git lfs pull`, so an unreachable LFS server delays attachments but not notes. Turning LFS on moves already-committed attachments to LFS on the next commit, without rewriting history. Sync stays off if git-lfs is missing, rather than committing attachments as plain files.

- **Tabs.** One tab per open note, each with its own autosave and back/forward history (ctrl-- and ctrl-shift-- on macOS, alt-left and alt-right elsewhere). The first tab is a fixed overview.

- **Overview.** Recent changes with the last commit's author, tasks, and link stats: most-linked notes, orphans, dead ends, tags, and the broken link count.

- **Link completion.** Typing after `[[` offers notes by title or path, in both live and raw mode.

- **Broken links.** "Broken links..." lists links that reach no note or file, several notes, or outside the vault, with close matches to retarget each and "Create" for a missing note.

- **Tasks.** Checklist items (`- [ ] text`, optional `due:YYYY-MM-DD`) and task notes (front matter `type: task` with `status`, `priority`, `due`), listed together, open first, by due date. Items tick from the list or by clicking the checkbox in live mode; "Promote" turns an item into a task note.

- **Tags.** Front matter `tags`, matched ignoring case. Type `#` in the palette to list tags, `#name` for the notes carrying one; "Edit tags..." rewrites the field. Inline `#tag` is not read, since it collides with headings and issue numbers.

- **Daily notes.** A calendar in the toolbar opens or creates `<journal folder>/YYYY-MM-DD.md`; cmd-shift-d opens today's.

- **Settings.** Sync, the attachment backend and the journal folder, written to `.katib/config.toml` with its comments kept, so every clone shares them.

- **Window chrome.** A title bar holding the toolbar on every platform, a status bar (save state, words, tags, sync), native menus on macOS and an in-window menu bar on Windows and Linux. Palette commands show their key bindings.

### Changed

- Search weights a match in the title (10) over a heading or tag (5) over body text (1). The index schema is now version 3; each vault's index rebuilds on first open.

- `.katib/config.toml` is read in one place, `katib_core::config`. An unknown backend value is reported, not guessed at.

### Fixed

- After a folder move, the sidebar kept expansion state for the old path, and a folder that later reappeared at that path opened expanded. Expansion now follows the move, and state for vanished folders is dropped.

## 0.1.0 (4f9e4e4)

First version: the live and raw markdown editor, the SQLite index with backlinks and full-text search, the palette, the folder tree, guarded atomic saves with draft recovery, and opt-in git sync with in-memory merges and a conflict dialog.
