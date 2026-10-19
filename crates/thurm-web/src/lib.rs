//! Thurm in the browser (`specs/web.md`): a sans-I/O client of the daemon protocol with a
//! local libghostty terminal per subscribed pane, compiled to wasm for the page in `web/`.

pub mod grid;
pub mod keys;
pub mod session;

#[cfg(target_arch = "wasm32")]
mod wasm;

pub use session::{KeyResult, Session};
