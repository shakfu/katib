//! katib's built-in agent: the vault as a toolset for `minima-core`'s agent loop, run on its own
//! thread for the window.

pub mod acp;
pub mod machine;
pub mod mcp;
mod session;
mod tools;

pub use minima_core::Cancel;
pub use session::{Choice, Event, Finish, PermissionOption, Session};
pub use tools::{Target, VaultTools, describe};
