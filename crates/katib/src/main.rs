//! katib: a desktop notes app over a folder of markdown files.

mod agent;
mod app;
mod complete;
mod pane;

use std::{env, ffi::OsString, path::PathBuf, process};

use gpui_kit::*;
use katib_core::{Index, Vault, github};

const USAGE: &str = "usage: katib <vault directory>
       katib export [--wiki] <vault directory> <destination>
       katib mcp <vault directory>
       katib lsp [vault directory]";

fn main() {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if args.len() > 1 && args[0] == "export" {
        export(&args[1..]);
    }
    if args.len() == 2 && args[0] == "mcp" {
        mcp(&args[1]);
    }
    if matches!(args.len(), 1 | 2) && args[0] == "lsp" {
        lsp(args.get(1));
    }
    let [root] = args.as_slice() else {
        fail(USAGE);
    };
    let root = PathBuf::from(root);
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

/// `katib mcp <vault>`: the agent's vault tools for an outside agent, over stdio.
fn mcp(root: &OsString) -> ! {
    let vault = Vault::open(root).unwrap_or_else(|e| fail(e));
    let tools = katib_agent::VaultTools::open(vault).unwrap_or_else(|e| fail(format!("{e:#}")));
    let (stdin, stdout) = (std::io::stdin().lock(), std::io::stdout().lock());
    katib_agent::mcp::serve(&tools, stdin, stdout).unwrap_or_else(|e| fail(format!("{e:#}")));
    process::exit(0);
}

/// `katib lsp [vault]`: a language server for other editors, over stdio. Without a vault, the
/// workspace the editor opens.
fn lsp(root: Option<&OsString>) -> ! {
    let (stdin, stdout) = (std::io::stdin().lock(), std::io::stdout().lock());
    katib_lsp::serve(root.map(PathBuf::from), stdin, stdout)
        .unwrap_or_else(|e| fail(format!("{e:#}")));
    process::exit(0);
}

/// `katib export [--wiki] <vault> <dest>`: a copy GitHub renders, links intact.
fn export(args: &[OsString]) -> ! {
    let (style, rest) = match args.split_first() {
        Some((flag, rest)) if flag == "--wiki" => (github::Style::Wiki, rest),
        _ => (github::Style::Repository, args),
    };
    let [root, dest] = rest else {
        fail(USAGE);
    };
    let vault = Vault::open(root).unwrap_or_else(|e| fail(e));
    let mut index = Index::open(&vault).unwrap_or_else(|e| fail(e));
    index.sync(&vault).unwrap_or_else(|e| fail(e));
    let report = github::export(&vault, &index, dest.as_ref(), style).unwrap_or_else(|e| fail(e));
    for u in &report.unlinked {
        eprintln!("{}: no single note for {}", u.id, u.link);
    }
    println!(
        "Exported {} notes and {} files to {}",
        report.notes,
        report.files,
        PathBuf::from(dest).display()
    );
    process::exit(0);
}

fn fail(e: impl std::fmt::Display) -> ! {
    eprintln!("katib: {e}");
    process::exit(1);
}
