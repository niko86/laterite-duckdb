//! WASM staticlib shim for the emscripten build path.
//!
//! extension-ci-tools' `rust.Makefile` builds the wasm artifact as
//! `cargo build --target wasm32-unknown-emscripten --example laterite_ags4`,
//! then `emcc`-links the resulting `liblaterite_ags4.a` as a SIDE_MODULE,
//! exporting `_laterite_ags4_init_c_api`. The `[[example]]` block in Cargo.toml
//! points here (`crate-type = ["staticlib"]`), so this file *is* that archive.
//!
//! `crate-type` can't be overridden per target (native needs `cdylib`; the wasm
//! link needs `staticlib`), so this example re-compiles `src/lib.rs` *as a
//! module* — the `#[unsafe(no_mangle)]` entry point comes along, and the native
//! cdylib and the wasm staticlib are separate artifacts that are never
//! co-linked. Do not add code here; keep it a pure re-map of `lib`.

#![allow(special_module_name)]

mod lib;
