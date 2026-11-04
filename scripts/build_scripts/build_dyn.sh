#! /bin/bash
# Dynamic linking against the prebuilt libstd.so requires unwinding and is incompatible with LTO:
# override the size/memory oriented release profile (see Cargo.toml) with the fast-to-compile settings.
export CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_PANIC=unwind CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 CARGO_PROFILE_RELEASE_INCREMENTAL=true
RUSTFLAGS='-C prefer-dynamic' cargo build --release