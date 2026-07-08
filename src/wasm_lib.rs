//! WASM staticlib shim for the emscripten build path.
//!
//! extension-ci-tools' `rust.Makefile` builds the wasm artifact as
//! `cargo build --target wasm32-unknown-emscripten --example laterite_ags4`,
//! then `emcc`-links the resulting `liblaterite_ags4.a`, exporting
//! `_laterite_ags4_init_c_api`. The `[[example]]` block in Cargo.toml points
//! here (`crate-type = ["staticlib"]`), so this file *is* that archive.
//!
//! The C entry point is emitted here, in the staticlib, by re-invoking
//! `entry_point_v2!` against the library's `register`. `src/lib.rs` gates its
//! own copy off `wasm32`, so the archive has exactly one definition of the
//! symbol. Gated to `wasm32` so a native `cargo build --examples` / `cargo
//! test` (which compiles examples) sees an empty, inert staticlib rather than a
//! second `laterite_ags4_init_c_api` that would clash with the cdylib's.

#[cfg(target_arch = "wasm32")]
quack_rs::entry_point_v2!(laterite_ags4_init_c_api, laterite_ags4::register);
