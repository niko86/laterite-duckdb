// WASM shim. The [[example]] target in Cargo.toml points here
// (crate-type = ["staticlib"]); extension-ci-tools builds it as
// `cargo build --target wasm32-unknown-emscripten --example laterite_ags4`, then
// emcc links it into the .wasm side module.
//
// `mod lib;` re-compiles lib.rs into this staticlib (so the entry symbol is
// defined here, no separate cdylib link → no duplicate symbol). It works only
// because lib.rs's submodules use `super::`-relative internal paths, not
// `crate::` — otherwise they'd resolve to `crate::lib::*` here and fail to build.
mod lib;
