# laterite_ags4 — DuckDB Rust extension Makefile.
# Delegates to cargo for the build and to extension-ci-tools for metadata/CI.

PROJ_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))

# Extension configuration. The c_api_extensions makefiles read EXTENSION_NAME
# + TARGET_DUCKDB_VERSION — NOT the old C++-CMake-template EXT_NAME/EXT_CONFIG/
# DUCKDB_PLATFORM_VERSION, which these makefiles silently ignore. That mismatch
# is why `cargo build` saw an empty DUCKDB_EXTENSION_NAME and looked for the
# wrong dylib (`lib.dylib`).
EXTENSION_NAME=laterite_ags4

# The path/remote readers use DuckDB's filesystem (the VFS) = the version-exact
# (unstable) C API, so the binary is pinned to one DuckDB version and rebuilt per
# release. That pin is what makes the wasm gate's npm version check load-bearing
# (test/wasm/README.md): the @duckdb/duckdb-wasm release it loads into must embed
# exactly TARGET_DUCKDB_VERSION.
#
# This is NOT a wasm exclusion — that note predated the wasm build. description.yml
# excludes only linux_amd64_musl, all three wasm variants ship, and `make test_wasm`
# measures the VFS reader working in wasm against a host-registered file.
USE_UNSTABLE_C_API=1
# The HOST DuckDB version the metadata footer is stamped for — must match the
# DuckDB the extension is LOADED into (the community build matrix + the
# sqllogictest venv are 1.5.6), NOT the C-API line the crate is built against.
# duckdb-rs 1.10506.0 compiles against the 1.5.6 C-API and the host is 1.5.6, so
# the cdylib is stamped v1.5.6. The stamped version must EQUAL the host's or the
# loader rejects it: the venv shipped v1.5.5 while this stayed v1.5.4, and the
# 1.5.5 sqllogictest host rejected the v1.5.4-stamped load with a version mismatch.
TARGET_DUCKDB_VERSION=v1.5.6
# The sqllogictest host's DuckDB, derived from the stamp above so the two cannot
# disagree. Left blank, base.Makefile's `venv` pip-installs whatever PyPI calls
# latest, so the host moved the day PyPI did rather than the day this file did:
# PyPI shipped 1.5.6 while the cdylib was still stamped v1.5.5, and every PR went
# red on the version-mismatch load with nothing in the PR to blame. `?=` so a
# caller that sets it on purpose (an env override, the community matrix) still wins.
DUCKDB_TEST_VERSION?=$(patsubst v%,%,$(TARGET_DUCKDB_VERSION))

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
	$(PYTHON_VENV_BIN) -m pip install "laterite==$(LATERITE_VERSION)" "duckdb==$(DUCKDB_TEST_VERSION)"
	$(PYTHON_VENV_BIN) scripts/laterite_parity.py --extension build/debug/$(EXTENSION_NAME).duckdb_extension

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
# The npm pin in test/wasm/package.json is load-bearing: under
# USE_UNSTABLE_C_API=1 the artifact is stamped for ONE DuckDB version and the
# loader rejects any other, so the @duckdb/duckdb-wasm release must embed
# exactly TARGET_DUCKDB_VERSION. The gate asserts that first and says so.
#
# Note both variants leave configure/platform.txt holding a wasm platform (the
# extension-ci-tools `wasm_*` targets rewrite it) — run `make configure` again
# before a native build.
.PHONY: test_wasm test_wasm_mvp test_wasm_eh wasm_gate_install
wasm_gate_install: test/wasm/node_modules

test/wasm/node_modules: test/wasm/package-lock.json
	cd test/wasm && npm ci
	@touch test/wasm/node_modules

test_wasm_mvp: wasm_mvp wasm_gate_install
	node test/wasm/gate.mjs wasm_mvp

test_wasm_eh: wasm_eh wasm_gate_install
	node test/wasm/gate.mjs wasm_eh

test_wasm: test_wasm_mvp test_wasm_eh
