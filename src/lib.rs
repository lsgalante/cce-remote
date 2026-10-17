//! The page: a cce-ui app run in the phone's browser (`src/web`). Built for
//! wasm32 by `build.rs` and embedded in the server; on any other target this
//! library is empty — the server is the binary, `src/main.rs`.

#[cfg(target_arch = "wasm32")]
mod web;
