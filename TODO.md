# TODO

## Critical

## High

## Medium

## Low

### Moves and export

- [ ] **Export for GitHub.** Write a copy of the vault with `[[wiki]]` links turned into markdown links, which GitHub renders. gwiki: `export`.

### Later

- [ ] **Agent access (MCP).** A server through which an agent reads and edits notes. Writes use `save_guarded`, so an agent cannot overwrite a note changed since it read it. gwiki: `mcp`. Item 18: serve `katib-agent`'s `VaultTools`, which already guard writes this way.

### Built-in agent

- [ ] **Check the panel by hand.** Open it (mod-shift-a), send a prompt, stop one, click a tool row's note, change provider and model in Settings, Agent. Check the layout at narrow widths and the input's growth. Not done on macOS: the terminal lacked Accessibility and Screen Recording.

- [ ] **Model list in Settings.** A dropdown of the provider's models from `minima_core::cache::Models`, instead of a free text field. Today the field does nothing while the provider is "Automatic".

- [ ] **Base URL in Settings.** llama.cpp and Ollama work only on their default ports (8080, 11434). Store a base URL per provider in `agent.json` and pass it as `Choice::base_url`.

- [ ] **Moves wait for sync.** The agent's `move` does not wait for a sync cycle, as the window's moves do, so a cycle can commit half of a move's link rewrites. Route the agent's moves through the window, or hold the same lock.

- [ ] **Panel conveniences.** Escape stops a prompt; a prompt typed while one runs is queued (the session already queues); a refused prompt is dropped from the conversation (`Agent::forget_last_prompt`).

- [ ] **TLS without aws-lc.** `minima-core` uses reqwest's `rustls` feature, whose aws-lc-rs provider needs cmake and NASM to build on Windows, and katib then builds two crypto libraries. Switch minima-core to ring (about 15 lines) and publish.

- [ ] **MCP server.** `katib mcp <vault>`: `VaultTools` over stdio, for outside agents. Item 10.

- [ ] **LSP server.** `katib lsp`: wiki-link completion, go to definition, broken links as diagnostics, rename as move. All from `katib-core`.

- [ ] **ACP client.** Host outside agents (Claude Code, minima, Gemini CLI) in the agent panel, serving their `fs/*` requests through `VaultTools`.

- [ ] **Delete tool.** `trash_note` exists; add it to `VaultTools` if the agent needs to delete.
