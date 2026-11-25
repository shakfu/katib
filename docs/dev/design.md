# katib design

Status: proposal, 2026-11-24. Nothing is built yet.

katib is a desktop notes app with wiki links. One user keeps one vault on several machines, and katib keeps the copies in sync through git. gwiki (`~/projects/personal/gwiki`) is the feature reference, not a constraint.

Decided:

- **One user, several desktops.** macOS, Linux, Windows. No phone, no shared editing.
- **Markdown files are the source of truth.** The index is a cache rebuilt from them.
- **katib syncs through git** to a remote the user controls, automatically.
- **Notes are edited in live preview by default**: markdown drawn as formatted text, syntax shown only where the cursor is. A raw markdown mode is one toggle away.
- **Attachments go through an object-store interface**, so large files can stay out of git later.
- **First version:** links, backlinks, search, the command palette. Tasks and vim editing come later.

Section 10 lists what is still open.

## 1. Goal

- Write and read notes in a native window, in live preview.
- Link notes with `[[wiki]]` and markdown links. See backlinks and broken links.
- Find any note within a keystroke.
- Edit on one machine and find the edit on another, without running a command.
- Never lose an edit to sync. A conflict is shown, never resolved silently.
- The vault stays usable without katib: in another editor, with `git log`, on GitHub.

Non-goals:

- Shared editing by several people, or real-time collaboration.
- Phone clients. GPUI has no iOS or Android target ([gpui-kit.com](https://gpui-kit.com)).
- Character-level merging (a CRDT). Git merges by line, which suffices for one writer.
- Hosting a server.

## 2. Vault layout

```text
vault/                    a git repository
  .katib/
    config.toml           committed: vault options, object store backend
    .gitignore            committed: cache/, drafts/, objects/ when external
    cache/                ignored: index.db
    drafts/               ignored: unsaved buffers, for crash recovery
  attachments/            committed pointers or files; see section 6
  inbox.md
  projects/katib.md
```

A note is any `.md` file outside `.katib/`. Its id is its path without `.md`. Its title is front matter `title`, else its first level-one heading, else its file name, as in gwiki.

Machine-local settings (the window, the theme, the last vault) go in the platform config directory (`dirs::config_dir()/katib/`), never in the vault.

## 3. Crates

A cargo workspace. Dependencies point downward only; `katib-core` has no GPUI dependency, so it can be tested and reused by a CLI.

| crate | owns |
|-|-|
| `katib-core` | vault discovery, note parsing, link resolution, the index, guarded writes, moves that rewrite links |
| `katib-sync` | the git sync state machine (section 5) |
| `katib-store` | the `ObjectStore` trait and its backends (section 6) |
| `katib-editor` | the live preview editor element (section 4) |
| `katib` | the gpui-kit app: windows, tree, palette, backlinks, settings |

Chosen libraries, each with the alternative it beat:

- **`pulldown-cmark`** for parsing, in the indexer and the editor, through its offset iterator. Over `markdown` (markdown-rs), which `TextView` uses: it took 6.9 ms to parse 32 KiB against 0.20 ms (release build, M-series Mac; [editor-spike.md](editor-spike.md)), and the editor reparses on every keystroke. The editor builds its own `BlockNode`s, so `TextView`'s parser does not decide what it draws. katib splits `[[...]]`, bare URLs and email addresses (GFM autolink literals, which pulldown-cmark lacks) out of text events itself. `katib_core::md` builds one tree from the events for both.
- **`rusqlite` with the bundled SQLite and FTS5** for the index and full-text search, in one file. Over `tantivy`, which is a second store to keep consistent with the link index.
- **The `git` binary**, through `std::process::Command`, for sync. Over `git2` (libgit2), because the binary uses the user's SSH agent, credential helper and config unchanged. Requires git 2.38 or later for `merge-tree --write-tree`.
- **`notify`** to watch the vault for changes made outside katib.

## 4. Editing

### 4.1 Modes

One buffer, two presentations of it:

- **Live** (default). Formatted text. Markdown syntax on the cursor's block stays visible and editable. Elsewhere it is hidden or drawn as its result: a heading at its size, a link as its label, a checkbox as a box, an image inline, a table as a grid. Front matter is hidden until the caret enters it (Up from the first line). HTML and footnote definitions stay raw.
- **Raw.** The markdown source with highlighting, in gpui-kit's `Editor`.

The toggle is per window, with a default in the machine settings. Switching keeps the cursor at the same byte offset. Both modes edit the same text, so a save writes exactly what the user typed, never a re-serialization.

### 4.2 What gpui-kit provides

gpui-kit has no live preview component. Read from the v0.11.0 source (`crates/base/src/input`, `crates/base/src/text`):

- **The input engine** (`Input`, `Textarea`, `Editor`) lays out every row at one line height (`base/element.rs`: row y is `line_height * row`). Headings cannot be drawn larger, and images cannot sit in the text.
  - Text decorations restyle a byte range with a GPUI `HighlightStyle` (color, weight, italic, underline, strikethrough, background; no font size). They move with edits (`editor/decorations.rs`).
  - Inline tokens draw a byte range as one atomic element, which must fit one row (`base/inline_tokens.rs`).
  - Folds hide whole lines only (`editor/display_map/fold_map.rs`).
  - No event reports a caret move; `InputEvent` has `Change`, `PressEnter`, `Focus`, `Blur`.
- **`editor_host`** (`gpui_kit::base::text::editor_host`, since 0.8.0, [#3580](https://github.com/longbridge/gpui-kit/pull/3580)) is a rendering API for editors that keep their own document. The editor passes a `BlockNode` tree, the tree `TextView` draws, with a `u64` key on each text leaf. `collect_leaf_geometry` returns a `LeafGeometry` per key: `index_for_position`, `position_for_index`, `caret_for_position`, `line_height`, `atom_at`. Selection and other ranges are painted per key. Headings keep their sizes; images and tables render as in `TextView`.
  - The editor owns editing, undo, selection, focus, IME (platform text input) and UTF-16 conversion.
  - Its only user in the repository is one 81-line test (`crates/base/tests/text_host.rs`).

### 4.3 Three ways to build it

| | A: block of inputs | B: `editor_host` | C: custom element |
|-|-|-|-|
| how | One `Textarea` per markdown block, with a font size per block type. Decorations style spans. Tokens hide syntax off the caret. | katib builds a `BlockNode` tree from the source on each edit. The cursor's block is emitted as source, the others as formatted nodes. `LeafGeometry` places the caret. | A GPUI element written from text layout up. |
| reveals syntax | per span, within a block | per block, or per span if katib emits mixed leaves | per span |
| headings, images | heading sizes yes; images as separate elements between inputs | yes, as `TextView` draws them | yes, all new code |
| katib writes | moving the cursor between inputs; selection and undo across inputs | input handling: keys, movement, selection, undo, IME | everything in B plus layout, wrapping, painting |
| main risk | a selection or undo that spans blocks | IME and caret movement are new code; the API has one user | size |

Recommendation: **B.** gpui-kit built `editor_host` for this purpose. It gives one document model, so selection and undo across blocks need no special case, which A cannot avoid. It leaves layout to gpui-kit, which C cannot.

The first step is a spike that answers three questions in a test window:

1. Can the caret move by character and by visual line using `LeafGeometry` alone?
2. Does IME composition (for example Japanese input on macOS) work through GPUI's `EntityInputHandler`, given the leaf bounds?
3. Does rebuilding the `BlockNode` tree on each keystroke stay under one frame (8 ms) for a 32 KiB note?

A "no" on 1 or 2 moves the editor to A. Status of each: [editor-spike.md](editor-spike.md). The `katib-editor` interface is a buffer and a cursor, so the app does not change when the element does.

Raw mode does not depend on the spike; it ships on `Editor` regardless.

### 4.4 Writes

Taken from gwiki's design:

- A save carries the text of the file as it was read. If the file on disk holds other text, the save is refused and the buffer is kept. Full text over a hash: the base text is in memory anyway, and equal text cannot collide.
- Writes go through a temporary file and a rename, so a crash leaves the old file or the new one.
- A note saves 1 s after typing stops, and at least every 10 s while it continues. A save that conflicts or fails writes the buffer to `.katib/drafts/`, offered back when the note next opens. A crash loses at most the unsaved seconds.
- On a conflict, autosave stops and the note shows "Keep mine" and "Take the file's".
- A file changed on disk (by sync, another editor, or git) reloads within a second, unless its buffer has unsaved edits. Then the buffer is marked instead.

## 5. Sync

### 5.1 States

```text
          edit
 Clean ---------> Dirty
   ^                |  no edit for 30 s, or window closes
   |                v
   |            Committing   git add -A; git commit
   |                |
   |                v
   |            Fetching     git fetch
   |                |
   |                v
   |            Merging      git merge-tree --write-tree HEAD @{u}
   |           /          \
   |    clean /            \ conflict
   |         v              v
   |     Applying         Conflicted  (user resolves, section 5.3)
   |         |              |
   |         v              v
   +------ Pushing <--------+
```

- **Committing** happens only after every open buffer is saved; an unsaved buffer delays the sync, it is never committed half-way.
- **Fetching** also runs on a timer (default 60 s) and when the window gains focus, so edits from another machine arrive while this one is idle.
- **Merging** uses `git merge-tree --write-tree` ([git docs](https://git-scm.com/docs/git-merge-tree)). It computes the merge without touching the working tree or the index. The working tree is therefore never left mid-merge, and the editor never sees conflict markers on disk.
- **Applying** writes the merge commit (`git commit-tree`) and moves the working tree to it with `git merge --ff-only`, which refuses to overwrite uncommitted changes. Files open in a clean buffer reload; a dirty buffer cannot exist here, by the rule for Committing.
- **Pushing** failing because the remote moved returns to Fetching. A network failure leaves the commits local and retries with backoff; the status bar shows the count of unpushed commits.

Commits are named `katib: <hostname> <ISO 8601 time>`. One commit per quiet period keeps history readable without squashing.

As built (`crates/katib-sync`, `crates/katib/src/app/sync.rs`): sync is opt-in; a vault enables it with `sync = true` in `.katib/config.toml`, so opening some other repository never commits it. Every cycle commits whatever is saved, so a fetch-timer cycle also commits; the 30 s quiet period only decides when a cycle starts early. Closing the window commits without fetching or pushing. Offline retries back off from 60 s to 10 min. katib adds `*.katib-tmp` to `.git/info/exclude`, so a save in flight is never committed.

### 5.2 Why not rebase

A rebase replays local commits one at a time and can stop on each. A merge resolves once. Linear history matters little in a notes vault.

### 5.3 Conflicts

`merge-tree` lists the conflicted paths. For each one, katib shows the two versions side by side, with the common ancestor available, and offers: keep mine, keep theirs, keep both (theirs saved as `<name> (conflict <host>).md`), or edit the merged text. Editing the merged text is not built yet; "keep both" then editing by hand covers it. The resolved tree is built in a scratch index (`GIT_INDEX_FILE`), never the real one. Sync for the vault pauses until every path is resolved. Nothing is pushed while a conflict is open.

Two conflicts are not text conflicts and get their own handling:

- **Edited here, deleted there.** Keep the edited file; record the deletion as undone.
- **Renamed on both machines.** Keep both names; the link checker then reports the duplicates.

### 5.4 Setup

katib does not store credentials. The user creates the remote and its access (SSH key or credential helper) outside katib, as for any repository. `katib` offers: open an existing clone, clone from a URL, or start a vault with no remote (sync off until one is added). Only the first is built: a vault that is a repository's top level syncs, anything else runs with sync off.

## 6. Attachments

### 6.1 Requirement

Images and PDFs must not swell the git history, but notes must keep plain links that other editors can follow.

### 6.2 Design

Attachments are content-addressed: named by the SHA-256 of their bytes. A note links to the local path, which is always a real file:

```markdown
![diagram](attachments/3f/3f9a...c2-diagram.png)
```

Where the bytes live is set by the vault's backend in `.katib/config.toml`:

```toml
[objects]
backend = "git"        # "git" | "lfs" | "store"
# url = "s3://bucket/vault"   for backend = "store"
```

| backend | bytes stored | committed | needs |
|-|-|-|-|
| `git` (default) | in the repository | the file | nothing |
| `lfs` | the LFS server of the remote | an LFS pointer | `git-lfs` installed; a remote that supports LFS, within its quota |
| `store` | an S3-compatible bucket, a directory, or SFTP | a manifest entry: hash, size, name | credentials for the store |

`katib-store` defines one trait (`put`, `get`, `has`, by hash) and the `store` backend implements it with the [`object_store`](https://docs.rs/object_store) crate, which already covers S3, GCS, Azure and local directories. For `store`, `attachments/` is gitignored and filled on demand from the manifest; a missing object is fetched when a note that links to it opens.

Content addressing makes sync safe for every backend: two machines adding the same file produce the same name, and an object is never modified, only added. Deleting unreferenced objects is a separate, explicit command.

### 6.3 What ships first

The `git` and `lfs` backends and the content-addressed naming. As built (`katib_core::attach`): pasting an image or files, dropping files, or "Attach file..." stores each file and links it at the caret; files over 50 MB are refused. Reading and hashing run off the main thread. With `backend = "lfs"`, storing an attachment adds `attachments/** filter=lfs ...` to `.gitattributes`, and sync runs `git lfs install --local`; sync stays off if git-lfs is missing, so attachments are never committed as plain files by mistake. `backend = "store"` is refused until it is built.

With LFS, a fast-forward checks attachments out as pointers (`GIT_LFS_SKIP_SMUDGE=1`), then `git lfs pull` downloads them. An unreachable LFS server therefore delays attachments but not notes; the status bar says so and the next cycle retries. Turning on `lfs` stages already-committed attachments again (`git add --renormalize`), so the next commit stores them as pointers; history is not rewritten. Storing a file whose copy on disk is still a pointer writes the real bytes. Pushing uploads objects through git-lfs's pre-push hook. Tests cover two clones of a local remote, including a lost object that downloads once restored.

Live mode draws local images; remote images stay alt text, since fetching them would tell their server each time a note is read. "Remove unused attachments..." moves unlinked files to `.katib/trash/`. The naming is the part that is costly to change later, since it appears in every note's links. `store` is added when a vault needs it; it changes no note.

## 7. Links and the index

As in gwiki, by reference to `~/projects/personal/gwiki/docs/dev/wiki-design.md`:

- `[[Title]]`, `[[path#Heading|label]]`, and relative markdown links. Resolution order: path, then folder page, then title, then file name; an ambiguous link lists its candidates.
- A folder's `README.md` is its page. `[[dir]]`, `[[dir/]]` and a markdown link to `dir` or `dir/` reach it. A note `dir.md` wins over `dir/README.md` for `[[dir]]`; `[[dir/]]` names only the page. A missing `[[dir/]]` creates `dir/README.md`.
- The index holds notes, headings, links, tags and the FTS5 text. It re-reads a file whose size or modification time differs, and rebuilds when its schema version differs, so it is never synced. The window reads it while a background thread syncs it (SQLite WAL).
- Moving a note rewrites every link to it, in one commit. Wiki links take the new file name when it is unambiguous, else the full path; a link by an explicit title still resolves and is left alone. The moved note's own relative links, images included, are rebased onto its new folder. Reference-style links are rewritten in their definition line. Moving a folder moves every file in it, and rewrites links into it from outside, including images. A link written as a folder stays a folder link when its page moves. The rename dialog previews the notes a move will rewrite, computed by the same code with nothing written. Each rewritten file is written with the same guard as a save. A move waits while a sync runs, so its writes land in one cycle.
- Deleting a note moves it to `.katib/trash/` (gitignored), keeping its folder; sync then commits the deletion. Folders emptied by a move or delete are removed.

## 8. The window

Built from gpui-kit 0.11.0 components:

- `TitleBar` on every platform, drawn by the window, holding the `Toolbar` and the note's title. On Linux this uses client-side decorations.
- `Toolbar`: sidebar, palette, new note, the daily-note `Calendar` in a `Popover`, mode, settings, and a `DropdownMenu` of note commands.
- A `Tree` of the vault, with a context menu per note and folder.
- `TabBar`: one tab per open note. Opening a note shows its tab if it has one, else replaces the active tab's note; "new tab" (cmd-t, or the tree menu) opens beside it. Each tab autosaves on its own; sync waits until every tab is saved.
- The editor (section 4), with a backlinks panel under it.
- Link completion: typing after `[[` offers notes by title or path, title prefixes first; tab or enter writes the reference (the title when it names one note, else the path) and closes the link. Live mode draws its own menu; raw mode uses the editor's `CompletionProvider`.
- Back and forward per tab (ctrl-- and ctrl-shift-- on macOS, alt-left and alt-right elsewhere): each tab remembers up to 100 (note, caret) places. A move renames the notes in the history; a deleted note is dropped when reached.
- "Broken links...": every wiki or markdown link that reaches no note or file, several notes, or outside the vault. A missing note offers up to three close matches (edit distance within a third of the name, or containing it) and "Create"; an ambiguous one offers each match. A repair rewrites that one link, keeping its heading and label, and is refused if the note changed since the list was made.
- Tasks: checklist items (`- [ ] text`, with an optional `due:YYYY-MM-DD`) in any note, and task notes (front matter `type: task` with `status` open, doing or done, `priority`, `due`). "Tasks..." lists both, open first, then by due date; overdue dates show in red, those within 7 days in amber. A row ticks an item or sets a task note's status, and "Promote" turns an item into a task note beside its note, replacing the item with a link. In live mode, clicking a checkbox ticks it as one undo step.
- Tags: front matter `tags`, as `[a, b]`, `a, b` or a `- a` list; inline `#tag` is not read, as in gwiki, since it collides with headings and issue numbers. Tags match ignoring case. The palette lists tags for `#` and the notes tagged for `#name`; "Edit tags..." rewrites the field as a flow list; the status bar shows the note's tags.
- Front matter is read and written by `katib_core::front`, which handles top-level `key: value` lines only and leaves other lines as they are. The index schema is version 2: `tags` and `items` tables, and task fields on `notes`.
- Overview: the first tab, fixed, shown when no note tab is selected. Its sections: Latest (notes by file time, with the last commit's author from one `git log -n500` over the vault, or "not committed"), Tasks, and Stats (note count, broken links, most-linked notes, orphans, dead ends, tags). A link to itself does not count. Stats resolve every link, so they are computed when the overview opens, on Refresh, and after an index sync while it shows. A note opened from the overview gets a new tab.
- Search ranks a match in the title (weight 10) above one in a heading or tag (5) above body text (1), as separate FTS5 columns (index schema 3).
- `Command` as the palette: open a note, search, create, or run a command. A command row shows its key binding.
- `StatusBar`: the note, its save state, words, indexing, and the sync state, which runs a cycle when clicked.
- `Settings` in a `Dialog`: sync on or off, the attachment backend, the journal folder. They are written to `.katib/config.toml`, so they are committed with the vault; changing sync or the backend restarts sync.
- `AlertDialog` to confirm deletes; `Dialog` for names and conflicts.
- A menu bar whose items dispatch the same actions as the keys: native on macOS, which keeps the system's menu search, shortcut rebinding and accessibility; on Windows and Linux an `AppMenuBar` at the start of the title bar. The title bar's close button saves first, as the OS close does.

Daily notes are `<journal folder>/<YYYY-MM-DD>.md`, `journal/` by default; cmd-shift-d opens today's.

## 9. Testing

- `katib-core`: unit tests on parsing, resolution and guarded writes, over vaults built in temporary directories.
- `katib-sync`: tests against two clones of a bare repository in a temporary directory, one per simulated machine. Cases: clean merge, same-line conflict, edit against delete, push rejected, network loss.
- `katib-store`: the trait's tests run against every backend; `store` runs against a local directory.
- `katib-editor`: buffer and cursor tests without a window; the rendering is checked by hand from a list.
- `make test` runs `cargo test --workspace`.

## 10. Open

- **Which editor approach** (section 4.3). Decided by the spike's three questions.
- **gpui-kit churn.** It is pre-1.0 and pins an exact GPUI snapshot (`gpui-pre =0.3.14` for 0.11.0). katib pins the gpui-kit minor and upgrades deliberately.
- **Commit interval.** 30 s idle is a guess. Too short makes noise; too long widens the window for conflicts.
- **One vault or several** open at once.
- **Encryption of the remote.** Out of scope now. If needed, `git-crypt` or an encrypted object store, not katib code.
