#! /bin/bash
cd ./target/release
export LD_LIBRARY_PATH+=:./:$(rustc --print=sysroot)/lib
./weel_bin