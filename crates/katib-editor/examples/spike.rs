//! Live preview spike window. `cargo run -p katib-editor --example spike [note.md]`
//! Edits are not saved.

use gpui_kit::{
    component::{ActiveTheme as _, v_flex},
    *,
};
use katib_editor::LiveEditor;

const SAMPLE: &str = "# Live preview spike\n\nA paragraph with **bold**, *italic*, `code`, a [link](https://example.com) and a [[Wiki page|wiki link]]. It is long enough to wrap at a normal window width, so moving up and down crosses soft line breaks inside one leaf.\n\n## A list\n\n- one\n- two with **bold**\n  - nested\n- [ ] a task\n\n> A quote.\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\nIME: type Japanese or Chinese here.\n";

/// The editor with a status line of its frame timing.
struct Spike {
    editor: Entity<LiveEditor>,
}

impl Render for Spike {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .size_full()
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
            .child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .border_t_1()
                    .border_color(theme.border)
                    .child(format!(
                        "spike, nothing is saved | {}",
                        self.editor.read(cx).stats()
                    )),
            )
    }
}

fn main() {
    let text = match std::env::args_os().nth(1) {
        Some(path) => std::fs::read_to_string(&path).unwrap_or_else(|e| {
            eprintln!("spike: {}: {e}", path.to_string_lossy());
            std::process::exit(1);
        }),
        None => SAMPLE.to_owned(),
    };
    application().with_assets(assets::Assets).run(move |cx| {
        init(cx);
        katib_editor::init(cx);
        open_window(WindowOptions::default(), cx, |window, cx| {
            let editor = cx.new(|cx| LiveEditor::new(text, window, cx));
            window.focus(&editor.focus_handle(cx), cx);
            // Redraw the status line whenever the editor redraws.
            cx.new(|cx| {
                cx.observe(&editor, |_, _, cx| cx.notify()).detach();
                Spike { editor }
            })
        })
        .expect("open the window");
    });
}
