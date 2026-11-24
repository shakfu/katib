# Editing

The block holding the caret shows its markdown; other blocks show formatted.
Press cmd-e to switch to raw markdown and back.

Text can be **bold**, *italic*, ~~struck~~ or `code`.

- [ ] a task
- [x] a finished task
  - a nested item

> A quote.

```rust
fn main() {
    println!("hello from katib");
}
```

## Attachments

Paste an image, drop a file on the note, or use "Attach file..." in the palette (cmd-p).
The file is stored under `attachments/`, named by the SHA-256 of its bytes:

![blocks](attachments/03/03811276fde1385123df9895bf943343db3127858008581fe909f5bb8e6da7e9-blocks.svg)

## Undo

Cmd-z undoes a word at a time; cmd-shift-z redoes.

## Saving

Notes save one second after typing stops. Back to [[Welcome]].
