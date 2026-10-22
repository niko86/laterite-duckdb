# laterite_ags4 — DuckDB Rust extension Makefile.
# Delegates to cargo for the build and to extension-ci-tools for metadata/CI.

PROJ_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))

# Extension configuration. The c_api_extensions makefiles read EXTENSION_NAME
# + TARGET_DUCKDB_VERSION — NOT the old C++-CMake-template EXT_NAME/EXT_CONFIG/
# DUCKDB_PLATFORM_VERSION, which these makefiles silently ignore. That mismatch
# is why `cargo build` saw an empty DUCKDB_EXTENSION_NAME and looked for the
# wrong dylib (`lib.dylib`).
EXTENSION_NAME=laterite_ags4

# NO USE_UNSTABLE_C_API, so the metadata footer's ABI type is `C_STRUCT`
# (stable). The unstable stamp (`C_STRUCT_UNSTABLE`) tied the binary to one exact
# DuckDB build — a v1.5.5-stamped binary was refused by the 1.5.6 sqllogictest
# host — and on DuckDB 2.0 it selects the V2 C API: the loader looks for
# `laterite_ags4_init_c_api_v2`, which duckdb-rs (V1 header only) never emits,
# so LOAD fails. Nothing here needs the unstable API any more: every function
# that was unstable in the V1 API, the VFS calls the path/remote readers use
# included, was stabilised at v1.5.6 (DuckDB 2.0's extension_load.cpp says so;
# the 1.5.6 and 2.0 duckdb_extension.h both gate them on
# DUCKDB_API_VERSION_AT_LEAST(1, 5, 6), not on UNSTABLE).
#
# Under `C_STRUCT` this is the C-API version the binary DECLARES, which the
# loader reads as a FLOOR: any host whose V1 C API is at least this loads it. It
# is not the host's DuckDB version and need not equal it — the one binary loads
# into 1.5.6 and into 2.0, whose V1 C API is also v1.5.6. It is v1.5.6 because
# that is where the VFS functions became stable; src/lib.rs requests the same
# version at init. test/wasm/checks.mjs reads this line with a regex, so keep it
# a literal `=` assignment.
TARGET_DUCKDB_VERSION=v1.5.6
# The sqllogictest host: the DuckDB 2.0 release, pinned so the host cannot move
# under a PR. `?=` so a caller that sets it on purpose (an env override, the
# community matrix) still wins.
DUCKDB_TEST_VERSION?=2.0.0

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

# --- laterite parity gate (#57) --------------------------------------------
# Release X of this extension pins the same engine crates as laterite X, so the
# SQL functions the two share must give the same answers. LATERITE_VERSION is
# the one place that names X: scripts/release.sh refuses to cut any other
# version, and this target installs exactly that laterite beside the
# sqllogictest host's DuckDB (pinned again so pip cannot move the host) and
# compares the debug build against it. See scripts/laterite_parity.py.
LATERITE_VERSION:=$(shell cat $(PROJ_DIR)LATERITE_VERSION)
.PHONY: test_laterite_parity
test_laterite_parity: check_configure
	$(PYTHON_VENV_BIN) -m pip install "laterite==$(LATERITE_VERSION)" "duckdb==$$($(PYTHON_VENV_BIN) -c 'import duckdb;print(duckdb.__version__)')"
	$(PYTHON_VENV_BIN) scripts/laterite_parity.py --extension build/debug/$(EXTENSION_NAME).duckdb_extension

# --- function docs gate ----------------------------------------------------
# functions.json and docs/function_descriptions.csv (DuckDB's community page is
# built from the csv) state each function's output columns; this DESCRIBEs the
# debug build and fails on any list that differs. See scripts/check_function_docs.py.
.PHONY: test_function_docs
test_function_docs: check_configure
	$(PYTHON_VENV_BIN) scripts/check_function_docs.py --extension build/debug/$(EXTENSION_NAME).duckdb_extension

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

# --- WASM functional gate --------------------------------------------------
# The wasm counterpart of `make test_debug` (#29). `make wasm_mvp` only proves
# the artifact COMPILES; these targets LOAD it into @duckdb/duckdb-wasm in
# headless node and run real SQL against it — read_ags_text born-typing, the
# `_parent_id`/`_id` join, `_content_hash` parity with the native/wheel output,
# `read_ags` through the host VFS, and the metadata surface. See test/wasm/.
#
# The npm pin in test/wasm/package.json is load-bearing: the
# @duckdb/duckdb-wasm release must embed a DuckDB at or above the C-API floor
# (TARGET_DUCKDB_VERSION). The gate asserts the artifact's stamp and the host's
# version against that floor first, and says so.
#
# Note both variants leave configure/platform.txt holding a wasm platform (the
# extension-ci-tools `wasm_*` targets rewrite it) — run `make configure` again
# before a native build.
.PHONY: test_wasm test_wasm_mvp test_wasm_eh test_wasm_browser test_wasm_threads wasm_gate_install
wasm_gate_install: test/wasm/node_modules

test/wasm/node_modules: test/wasm/package-lock.json
	cd test/wasm && npm ci
	@touch test/wasm/node_modules

test_wasm_mvp: wasm_mvp wasm_gate_install
	node test/wasm/gate.mjs wasm_mvp

test_wasm_eh: wasm_eh wasm_gate_install
	node test/wasm/gate.mjs wasm_eh

test_wasm: test_wasm_mvp test_wasm_eh

# The browser gate (#50): Chromium via Playwright — once: `cd test/wasm && npx
# playwright install chromium-headless-shell`. CI runs it on wasm_eh. The
# wasm_threads mode is kept for when a loadable threads build exists: today's
# artifact is refused by the coi runtime, which is why it is not published.
test_wasm_browser: wasm_eh wasm_gate_install
	node test/wasm/browser-gate.mjs wasm_eh

test_wasm_threads: wasm_threads wasm_gate_install
	node test/wasm/browser-gate.mjs wasm_threads
