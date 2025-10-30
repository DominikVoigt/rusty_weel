#! /bin/bash

RUSTFLAGS='-C prefer-dynamic' cross build --target aarch64-unknown-linux-gnu --release