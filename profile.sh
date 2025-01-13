#! /bin/bash
cd ./target/release
# Add both paths depending on whether cross compile toolchain is installed or not
export LD_LIBRARY_PATH+=:./:$(rustc --print=sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/lib:$(rustc --print=sysroot)/lib
valgrind --tool=massif ./weel_bin