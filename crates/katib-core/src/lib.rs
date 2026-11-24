//! Vault discovery and note parsing. No GPUI dependency; see `docs/dev/design.md`.

pub mod attach;
pub mod complete;
pub mod config;
pub mod front;
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
pub use manage::{ManageError, Moved, move_folder, move_note, retarget, trash_note, untitled_id};
pub use note::{Item, Link, LinkKind, Note, heading_offset, parse, text_links};
pub use vault::{Entry, Vault, VaultError};
pub use write::{SaveError, save_guarded, write_atomic};
