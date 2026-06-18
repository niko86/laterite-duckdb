# laterite_ags4 — DuckDB Rust extension Makefile.
# Delegates to cargo for the build and to extension-ci-tools for metadata/CI.

PROJ_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))

# Extension configuration
EXT_NAME=laterite_ags4
EXT_CONFIG=$(PROJ_DIR)extension_config.cmake

# C Extension API (NOT the DuckDB release version). v1.5.0 is the floor the
# `duckdb-1-5` feature (virtual filesystem) requires; the build is forward-
# compatible to the community target (v1.5.3).
USE_UNSTABLE_C_API=1
DUCKDB_PLATFORM_VERSION=v1.5.0

# Include extension-ci-tools build rules (the `extension-ci-tools` submodule).
include extension-ci-tools/makefiles/c_api_extensions/base.Makefile
include extension-ci-tools/makefiles/c_api_extensions/rust.Makefile
