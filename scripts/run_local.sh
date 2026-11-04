#! /bin/bash
# Usage: run_local.sh [opts.json] [context.json]
# Paths are relative to the directory you call the script from.
# Without arguments, opts.json and context.json from test_setup/ are used.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RELEASE_DIR="$SCRIPT_DIR/../target/release"
TEST_SETUP_DIR="$SCRIPT_DIR/../test_setup"

OPTS_PATH="$(realpath -m "${1:-$TEST_SETUP_DIR/opts.json}")"
CONTEXT_PATH="$(realpath -m "${2:-$TEST_SETUP_DIR/context.json}")"

cd "$RELEASE_DIR"
# Add both paths depending on whether cross compile toolchain is installed or not
export LD_LIBRARY_PATH+=:./:$(rustc --print=sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/lib:$(rustc --print=sysroot)/lib
./weel_bin --opts "$OPTS_PATH" --context "$CONTEXT_PATH"
