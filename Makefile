# laterite_ags4 — DuckDB Rust extension Makefile.
# Delegates to cargo for the build and to extension-ci-tools for metadata/CI.

PROJ_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))

# Extension configuration. The c_api_extensions makefiles read EXTENSION_NAME
# + TARGET_DUCKDB_VERSION — NOT the old C++-CMake-template EXT_NAME/EXT_CONFIG/
# DUCKDB_PLATFORM_VERSION, which these makefiles silently ignore. That mismatch
# is why `cargo build` saw an empty DUCKDB_EXTENSION_NAME and looked for the
# wrong dylib (`lib.dylib`).
EXTENSION_NAME=laterite_ags4

# Unstable C Extension API — required by the `duckdb-1-5` VFS feature. NOTE: the
# unstable API pins the built binary to ONE EXACT DuckDB version (it is NOT
# forward-compatible — a v1.5.0 binary refuses to load in v1.5.4). So this MUST
# equal the DuckDB the extension runs against; the test runner installs latest
# stable (v1.5.4). community-extensions builds one binary per DuckDB version in
# its CI matrix and overrides this, so locally just match your test DuckDB.
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

# --- WASM build-order fix ------------------------------------------------
# This pinned extension-ci-tools lists `link_wasm_release` BEFORE
# `build_extension_library_release` in build_extension_with_metadata_release's
# prerequisites, and link_wasm_* carry no dependency on the build — so on a clean
# tree `emcc` runs before cargo and dies ("liblaterite_ags4.a: No such file").
# Add the missing edge so the staticlib is built before emcc links it. Native
# builds are unaffected (link_wasm_* are no-ops off a wasm platform).
link_wasm_release: build_extension_library_release
link_wasm_debug: build_extension_library_debug
