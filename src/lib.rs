//! The page: a cce-ui app run in the phone's browser (`src/web`). Built for
//! wasm32 by `build.rs` and embedded in the server; on any other target this
//! library holds only the page's pure logic, for its tests — the server is
//! the binary, `src/main.rs`.

#[cfg(target_arch = "wasm32")]
mod web;

/// The echo strip's keystroke prediction: pure, so tested natively.
pub mod predict;
/// The Agent view's text wrapping: pure, so tested natively.
pub mod wrap;
