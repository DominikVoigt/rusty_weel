To run the weel binary (libweellib.so has to be in the folder):
LD_LIBRARY_PATH+=:./:$(rustc --print=sysroot)/lib ./weel_bin