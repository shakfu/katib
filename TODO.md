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
9. **Done.** **Export for GitHub.** Write a copy of the vault with `[[wiki]]` links
   turned into markdown links, which GitHub renders. gwiki: `export`.

## Later

10. **Done.** **Agent access (MCP).** A server through which an agent reads and edits
    notes. Writes use `save_guarded`, so an agent cannot overwrite a note
    changed since it read it. gwiki: `mcp`. Item 18: serve `katib-agent`'s
    `VaultTools`, which already guard writes this way.
11. **Done.** **Search ranking by heading and tag.** Index headings and tags as their own
    FTS columns and weight them above body text. Today only titles are
    weighted (`bm25(fts, 10.0, 1.0)`).

## Built-in agent

`crates/katib-agent` (vault tools, keys, the agent thread) and the panel in
`crates/katib/src/agent.rs`, over `minima-core` 0.7.1. Built and tested
headless; the panel has not been seen on screen.

12. **Check the panel by hand.** Open it (mod-shift-a), send a prompt, stop
    one, click a tool row's note, change provider and model in Settings,
    Agent. Check the layout at narrow widths and the input's growth. Not done
    on macOS: the terminal lacked Accessibility and Screen Recording.
    Also run "Export vault for GitHub..." and "Export vault as GitHub
    wiki..." from the palette: the folder picker and the result notice.
    In Settings, Agent: the model list for each provider, its search, and
    the text field when a provider has no list. Base URL: Enter applies it,
    a bad one shows why, and the model list follows it. Choose Ollama or
    llama.cpp with no key set, and send a prompt.
    In the panel: Escape in the input stops a prompt; a prompt sent while
    one runs shows as "Queued" and goes once it answers; Stop puts queued
    prompts back in the input.
    "New folder..." from the toolbar, File menu, palette and a folder's
    menu: the page opens, the folder shows in the tree, an existing folder
    is refused.
    Outside agents: choose each preset in Settings, Agent, and a custom
    command; a bad command shows why. Send a prompt to Claude Code (after
    its sign-in), answer a permission row with each option, Stop one while
    it asks, and check that an edit to a note shows in the open tab.
13. **Done.** **Model list in Settings.** A dropdown of the provider's models from
    `minima_core::cache::Models`, instead of a free text field. Today the
    field does nothing while the provider is "Automatic".
14. **Done.** **Base URL in Settings.** llama.cpp and Ollama work only on their default
    ports (8080, 11434). Store a base URL per provider in `agent.json` and pass
    it as `Choice::base_url`.
15. **Done.** **Moves wait for sync.** The agent's `move` does not wait for a sync cycle,
    as the window's moves do, so a cycle can commit half of a move's link
    rewrites. Route the agent's moves through the window, or hold the same
    lock.
16. **Done.** **Panel conveniences.** Escape stops a prompt; a prompt typed while one
    runs is queued (the session already queues); a refused prompt is dropped
    from the conversation (`Agent::forget_last_prompt`).
17. **Dropped.** **TLS without aws-lc.** minima-core stays on aws-lc-rs;
    a Windows build needs cmake and NASM, as aws-lc-rs documents.
18. **Done.** **MCP server.** `katib mcp <vault>`: `VaultTools` over stdio, for outside
    agents. Item 10.
19. **Done.** **LSP server.** `katib lsp`: wiki-link completion, go to definition,
    broken links as diagnostics, rename as move. All from `katib-core`.
20. **Done.** **ACP client.** Host outside agents (Claude Code, minima, Gemini CLI) in
    the agent panel, serving their `fs/*` requests through `VaultTools`.
21. **CHANGELOG entry** for the agent once item 12 passes, outside agents
    (ACP) included.
22. **Done.** **Delete tool.** `trash_note` exists; add it to `VaultTools` if the agent
    needs to delete.
