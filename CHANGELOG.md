# Changelog

All notable changes to katib. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Design detail is in [docs/dev/design.md](docs/dev/design.md).

## 0.2.0

### Added

- **Note management.** Create, rename, move and delete notes, and rename or move folders, from the tree's context menu or the palette. A move rewrites every link to the moved notes, including reference-style definitions, and rebases the moved notes' own relative links. Delete moves the note to `.katib/trash/`. "New folder..." creates a folder with its page, `README.md`, since git keeps no empty folder and the tree lists notes.

- **Attachments.** Paste or drop images and files, or use "Attach file...". Files are stored under `attachments/<hash>/` named by SHA-256, so the same bytes are stored once; files over 50 MB are refused. "Remove unused attachments..." moves unlinked files to the trash. Remote images are not fetched, so reading a note tells no server.

- **Git LFS backend.** With `[objects] backend = "lfs"`, attachments are committed as LFS pointers. Sync fast-forwards with `GIT_LFS_SKIP_SMUDGE=1` and then runs `git lfs pull`, so an unreachable LFS server delays attachments but not notes. Turning LFS on moves already-committed attachments to LFS on the next commit, without rewriting history. Sync stays off if git-lfs is missing, rather than committing attachments as plain files.

- **Folder pages.** A folder's `README.md` is its page, and folder links reach it as GitHub renders them; the rules are in [design.md section 7](docs/dev/design.md#7-links-and-the-index). In the tree, clicking a folder opens its page, which is not listed as a child. The folder's menu creates, renames or deletes the page. A link that matched a note by title or file name, such as `[[folder]]` reaching `other/folder.md`, moves to the page once `folder/README.md` exists.

- **Tables and front matter in live mode.** GFM tables draw as grids, with links and emphasis in cells. Front matter is hidden until the caret enters it; Up from the first line reaches it. In a table, write a wiki link label as `[[Page\|label]]`, since a bare `|` splits the cell; links read and moves keep that form.

- **Export with Quarto.** "Export note..." renders the open note to HTML, PDF, Word, EPUB or GitHub markdown in `.katib/out/`, and opens it. Code cells are shown, not run, so neither R nor Python is needed. "Run and export note..." runs them first, in the note's folder. Running is a separate command, not a default, because synced notes can carry other people's code. Links to other notes become their label, since a single file has nowhere for them to go. The note's front matter reaches Quarto, so options such as `format: typst: toc: true` and `bibliography:` apply. Files the front matter names are copied along with it. In front matter, `${VAULT}` is the vault root and `${HOME}` your home folder, as in `reference-doc: ${VAULT}/templates/house.docx`; templates in the vault sync with it. Other names are refused rather than read from the environment, since a synced note could otherwise copy a secret into a document you share. Vault-wide defaults go in `.katib/quarto.yml`, which is committed with the vault; a note's front matter overrides them. Needs [Quarto](https://quarto.org) on PATH.

- **Export for GitHub.** `katib export <vault> <dest>`, or "Export vault for GitHub...", writes a copy of the vault with wiki links as relative markdown links, which GitHub's file view renders. `--wiki`, or "Export vault as GitHub wiki...", writes it for a GitHub wiki instead: notes at the top level under unique page names, since wiki page URLs ignore folders. A vault with no root README gets a generated `Home` page that lists every note. Front matter, which a wiki shows as text, is dropped; its `title` becomes the heading. Links that match no single note are left as text and listed. The destination must be empty, or a git work tree with nothing uncommitted, whose files are replaced; so a wiki clone can be exported into, then committed. Rules: [docs/dev/github-export.md](docs/dev/github-export.md).

- **MCP server.** `katib mcp <vault>` serves the vault over the Model Context Protocol's stdio transport, for agents outside katib: list, read, search, backlinks, write, edit, move and delete. Replacing or deleting a note requires reading it first and fails if it changed since, so an agent never overwrites an edit it has not seen. Moves exclude a sync cycle across processes through a lock file in `.katib/cache/`, so a commit never holds half a move. The server does not commit; the window's sync does.

- **LSP server.** `katib lsp [vault]` gives other editors wiki link completion, go to definition, broken links as diagnostics, and rename as a move that rewrites every link to the note. Rename sends the edits and the file rename for the editor to apply, rather than writing files under its open buffers. It is refused while a note it would rewrite has unsaved changes, since its edits are computed from the files.

- **Move preview.** The rename dialog lists the notes a move will rewrite, or why it would be refused, as you type.

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

- Search weights a match in the title (10) over a heading or tag (5) over body text (1). The index schema is now version 4; each vault's index rebuilds on first open.

- `.katib/config.toml` is read in one place, `katib_core::config`. An unknown backend value is reported, not guessed at.

### Fixed

- A push rejected on every cycle made sync retry about twice a second, with no limit. That happens when fetch and push reach different remotes, for example with a `pushurl` set. Sync now retries a rejected push at once up to 3 times in a row, then backs off as it does offline, and the status bar says so.

- After a folder move, the sidebar kept expansion state for the old path, and a folder that later reappeared at that path opened expanded. Expansion now follows the move, and state for vanished folders is dropped.

## 0.1.0 (4f9e4e4)

First version: the live and raw markdown editor, the SQLite index with backlinks and full-text search, the palette, the folder tree, guarded atomic saves with draft recovery, and opt-in git sync with in-memory merges and a conflict dialog.
