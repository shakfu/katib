//! The live preview editor. Spike for `docs/dev/design.md` section 4.3, approach B.

mod doc;
mod project;
mod view;

pub use doc::Document;
pub use project::{LeafMap, Projection, Segment, project};
pub use view::{Completer, EditorEvent, LiveEditor, init};
