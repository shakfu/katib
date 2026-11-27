# Live preview spike

Status: built 2026-11-24; questions 1 and 2 await a manual run. Tests approach B of [design.md](design.md) section 4.3.

```sh
cargo run -p katib-editor --example spike [note.md]          # the window; nothing is saved
cargo run --release -p katib-editor --example timing         # question 3
```

## How it works

- The source is cut into segments of whole lines, one per top-level block or blank line (`project.rs`).

- The segment holding the caret is emitted as raw lines, one keyed `Paragraph` each; a heading keeps its level. Leaf bytes equal source bytes there, so the caret and IME need no mapping.

- Other segments are converted to `BlockNode`s. Each leaf records `(leaf bytes, source bytes)` runs, for clicks and selection painting.

- Tables, HTML and front matter have no formatted form yet; their segments stay raw.

- Every blank line is drawn as an empty line, with no other gap between blocks, so vertical space follows the source. Hiding them made a return on a blank line look like it did nothing.

- `EditorElement` calls `collect_leaf_geometry` around the child's prepaint, then draws the caret from `LeafGeometry::position_for_index` and registers `ElementInputHandler`.

- Up and down use only the last frame's geometry: the next visual line inside the leaf, else the next leaf in document order, at a kept goal x.

## Question 3: rebuild cost

32,928 bytes of mixed headings, paragraphs, lists and code; 200 runs, caret moved each run; release build.

| step | median | p95 |
|-|-|-|
| `markdown::to_mdast` (markdown-rs) | 6.90 ms | 7.53 ms |
| `project` on markdown-rs | 8.22 ms | 8.79 ms |
| `katib_core::md::parse` (pulldown-cmark) | 0.43 ms | 0.43 ms |
| `project` on pulldown-cmark | 1.34 ms | 1.44 ms |

markdown-rs spent the 8 ms budget before layout began, so katib moved to pulldown-cmark. The `md::parse` figure includes building the tree, so it exceeds the 0.20 ms of bare events. Layout and paint are not in these numbers; the status bar of the spike window shows the whole frame (render to paint) and the worst frame so far.

## Questions 1 and 2: manual checklist

Run the window with the sample text, then with a real note. For each, note pass or fail:

1. Click inside bold text, a link label and a code block; the caret lands at the clicked character, and that block turns raw.

2. Left and right across `**` and `[[...]]` in a raw block move one character at a time.

3. Up and down through a paragraph that wraps: the caret stays near the same x on each visual line.

4. Up and down across a heading, a list, the code block and the blank lines between them; no line is skipped and the caret does not jump to the document's start or end.

5. Shift-up, shift-down and a mouse drag across several blocks: the selection is painted in every block it covers.

6. Japanese or Chinese IME (macOS: add an input source, switch with ctrl-space): the composition is shown while typing, the candidate window opens next to the caret, and committing replaces the composition.

7. The emoji picker (ctrl-cmd-space) inserts at the caret.

8. `frame` and `worst` in the status bar on a 32 KiB note while typing.

Known gaps, outside the three questions: no caret blink, and a raw block draws without syntax colouring. Undo and scrolling to the caret were added after the spike; saving is the app's (`crates/katib/src/pane.rs`).
