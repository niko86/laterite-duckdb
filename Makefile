# laterite_ags4 — DuckDB Rust extension Makefile.
# Delegates to cargo for the build and to extension-ci-tools for metadata/CI.

PROJ_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))

# Extension configuration. The c_api_extensions makefiles read EXTENSION_NAME
# + TARGET_DUCKDB_VERSION — NOT the old C++-CMake-template EXT_NAME/EXT_CONFIG/
# DUCKDB_PLATFORM_VERSION, which these makefiles silently ignore. That mismatch
# is why `cargo build` saw an empty DUCKDB_EXTENSION_NAME and looked for the
# wrong dylib (`lib.dylib`).
EXTENSION_NAME=laterite_ags4

# Native-only. The path/remote readers use DuckDB's filesystem (the VFS) = the
# version-exact (unstable) C API, so the binary is pinned to one DuckDB version and
# rebuilt per release. DuckDB-WASM lags this ABI and is excluded (see
# description.yml's `excluded_platforms`); browser SQL-over-AGS is served by the
# dedicated `laterite-ags4-wasm` package instead.
USE_UNSTABLE_C_API=1
TARGET_DUCKDB_VERSION=v1.5.4

# Include extension-ci-tools build rules (the `extension-ci-tools` submodule).
include extension-ci-tools/makefiles/c_api_extensions/base.Makefile
include extension-ci-tools/makefiles/c_api_extensions/rust.Makefile

# --- Convenience aliases -------------------------------------------------
# extension-ci-tools ships only low-level targets (platform, venv,
# build_extension_with_metadata_*, test_extension_*). The duckdb template
# normally wires these friendly aliases on top; the staged scaffold omitted
# them, so `make configure` / `make release` / `make test` had nothing to run
# (`make test` even matched the test/ DIRECTORY). .PHONY so `test` isn't
# shadowed by that dir. Usage: make configure && make release && make test
.PHONY: configure release debug test test_release test_debug clean clean_all
configure: venv platform extension_version
release:   build_extension_with_metadata_release
debug:     build_extension_with_metadata_debug
test:         test_release
test_release: test_extension_release
test_debug:   test_extension_debug
clean:     clean_build clean_rust
clean_all: clean_configure clean_build clean_rust

# --- WASM link ordering ---------------------------------------------------
# extension-ci-tools' base.Makefile declares the metadata target as
#   build_extension_with_metadata_release: check_configure link_wasm_release build_extension_library_release
# i.e. it lists the emcc link (link_wasm_release) *before* the cargo build that
# stages the staticlib archive (build_extension_library_release). On a clean tree
# `make wasm_mvp` therefore runs emcc against a not-yet-copied
# `build/<plat>/release/liblaterite_ags4.a` and dies with "No such file".
# Add the missing dependency edge so the link waits for the archive. Gated to the
# wasm platforms (link_wasm_* is a no-op recipe off wasm) so the native build's
# ordering is untouched; make updates build_extension_library_* at most once per
# run, so this adds no redundant cargo invocation.
ifneq ($(DUCKDB_WASM_PLATFORM),)
link_wasm_release: build_extension_library_release
link_wasm_debug:   build_extension_library_debug
endif
