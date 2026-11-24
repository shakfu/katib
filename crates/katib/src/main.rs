//! katib: a desktop notes app over a folder of markdown files.

mod app;
mod complete;
mod pane;

use std::{env, path::PathBuf, process};

use gpui_kit::*;
use katib_core::{Index, Vault};

fn main() {
    let Some(root) = env::args_os().nth(1).map(PathBuf::from) else {
        fail("usage: katib <vault directory>");
    };
    let vault = Vault::open(&root).unwrap_or_else(|e| fail(e));
    let index = Index::open(&vault).unwrap_or_else(|e| fail(e));
    application().with_assets(assets::Assets).run(move |cx| {
        init(cx);
        katib_editor::init(cx);
        app::bind_keys(cx);
        app::set_menus(cx);
        let window_bounds = Some(WindowBounds::centered(None, size(px(1100.), px(760.)), cx));
        // The window draws its title bar, holding the toolbar (and, off
        // macOS, the menus). On Linux that needs client-side decorations, or
        // the compositor adds a title bar of its own.
        let options = WindowOptions {
            window_bounds,
            window_decorations: cfg!(target_os = "linux").then_some(WindowDecorations::Client),
            ..component::TitleBar::window_options()
        };
        open_window(options, cx, |window, cx| {
            cx.new(|cx| app::Katib::new(vault, index, window, cx))
        })
        .expect("open the window");
        // katib has one window; closing it ends the app. The window saves on close.
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}

fn fail(e: impl std::fmt::Display) -> ! {
    eprintln!("katib: {e}");
    process::exit(1);
}
