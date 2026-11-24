//! Wiki link completion for both editors, from the index.

use std::{cell::RefCell, rc::Rc};

use gpui_kit::component::{
    RopeExt as _,
    input::{CompletionProvider, Rope},
};
use gpui_kit::*;
use katib_core::{
    Index, Suggestion,
    complete::{completion_edit, wiki_query},
};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionResponse, CompletionTextEdit, TextEdit,
};

/// How many notes a menu offers.
const LIMIT: usize = 8;

/// Notes for the reference typed so far, for the live editor.
pub fn completer(index: Rc<RefCell<Index>>) -> katib_editor::Completer {
    Rc::new(move |query: &str| suggest(&index, query))
}

fn suggest(index: &RefCell<Index>, query: &str) -> Vec<Suggestion> {
    index.borrow().complete(query, LIMIT).unwrap_or_else(|e| {
        eprintln!("katib: {e}");
        Vec::new()
    })
}

/// The raw editor's provider: the same notes, as completion items whose edit
/// writes the reference and closes the link.
pub struct WikiCompletions(pub Rc<RefCell<Index>>);

impl CompletionProvider for WikiCompletions {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _: CompletionContext,
        _: &mut Window,
        _: &mut App,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        let source = text.to_string();
        let Some((start, query)) = wiki_query(&source, offset) else {
            return Task::ready(Ok(CompletionResponse::Array(Vec::new())));
        };
        let items = suggest(&self.0, query)
            .into_iter()
            .map(|s| {
                let (range, new_text) = completion_edit(&source, start, offset, &s.reference);
                CompletionItem {
                    label: s.title,
                    detail: Some(s.id),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                        range: lsp_types::Range {
                            start: text.offset_to_position(range.start),
                            end: text.offset_to_position(range.end),
                        },
                        new_text,
                    })),
                    ..Default::default()
                }
            })
            .collect();
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    /// Every edit asks; `completions` answers only inside an open `[[`.
    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut App) -> bool {
        !new_text.is_empty()
    }
}
