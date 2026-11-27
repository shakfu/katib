//! Vault discovery and note parsing. No GPUI dependency; see `docs/dev/design.md`.

pub mod attach;
pub mod complete;
pub mod config;
pub mod export;
pub mod front;
pub mod github;
mod index;
mod manage;
pub mod md;
mod note;
pub mod paths;
pub mod tasks;
#[cfg(test)]
mod test_dir;
mod vault;
mod write;

pub use index::{
    Backlink, Broken, Hit, Index, IndexError, NoteRow, Problem, Recent, Stats, Suggestion,
    TagCount, Target, TaskRow,
};
pub use manage::{
    ManageError, Moved, move_folder, move_note, move_note_edits, preview_move_folder,
    preview_move_note, retarget, trash_note, untitled_id,
};
pub use note::{Item, Link, LinkKind, Note, heading_offset, parse, text_links};
pub use vault::{Entry, Exclusive, Vault, VaultError};
pub use write::{SaveError, autosave_due, save_guarded, write_atomic};
