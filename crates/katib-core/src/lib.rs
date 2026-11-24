//! Vault discovery and note parsing. No GPUI dependency; see `docs/dev/design.md`.

mod index;
pub mod md;
mod note;
#[cfg(test)]
mod test_dir;
mod vault;
mod write;

pub use index::{Backlink, Hit, Index, IndexError, NoteRow, Target};
pub use note::{Link, LinkKind, Note, heading_offset, parse, text_links};
pub use vault::{Entry, Vault, VaultError};
pub use write::{SaveError, save_guarded, write_atomic};
