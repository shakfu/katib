# TODO

Features gwiki has and katib lacks, in priority order. Done items stay
listed, marked, so the numbers hold.

## Linking and navigation

1. **Done.** **Link completion.** Typing `[[` suggests note titles at the caret. Reuse the
   palette's title and path matching. gwiki: language server completion.
2. **Done.** **Back and forward.** A history of (note, caret offset) per tab, with
   back and forward keys and toolbar buttons. gwiki: `ctrl-o`.
3. **Done.** **Broken links, with repairs.** List every link that resolves to
   `Target::Missing`, with close matches to retarget it and "create" as the
   other choice. The index already holds the links. gwiki: `check --fix`.

## Tasks and tags

4. **Done.** **Tasks.**
   - Click a checkbox in live mode to toggle it (`Kind::Item { checked }` is
     parsed and drawn, but not interactive).
   - Index `- [ ]` items from every note.
   - Task notes: front matter `type: task` with `status`, `priority`, `due`.
   - A tasks view, overdue and due soon first.
   - Promote a checklist item to a task note.
   gwiki: `tasks`, `done`, `promote`.
5. **Done.** **Tags.** Index front matter `tags`; search and list by tag. gwiki: `tag`,
   `untag`.

## Views

6. **Done.** **Overview.** Shown when no note is open, or as a tab:
   - latest: notes by last change, with the last commit's author;
   - tasks: item 4's view;
   - stats: orphans, dead ends, most-linked notes, tags.
7. **Done.** **Folder pages.** A folder's `README.md` is its page: `[[folder]]` and a
   link to `folder/` reach it, and the tree opens it from the folder's row.

## Moves and export

8. **Done.** **Preview a move.** The rename dialog lists the notes whose links it will
   rewrite before confirming. gwiki: `mv --dry-run`.
9. **Export for GitHub.** Write a copy of the vault with `[[wiki]]` links
   turned into markdown links, which GitHub renders. gwiki: `export`.

## Later

10. **Agent access (MCP).** A server through which an agent reads and edits
    notes. Writes use `save_guarded`, so an agent cannot overwrite a note
    changed since it read it. gwiki: `mcp`.
11. **Done.** **Search ranking by heading and tag.** Index headings and tags as their own
    FTS columns and weight them above body text. Today only titles are
    weighted (`bm25(fts, 10.0, 1.0)`).
