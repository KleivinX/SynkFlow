# Source this before building anything you will share:   source packaging/clean-paths.sh
#
# Rust (and C code built by it) embeds the build machine's source paths for panic messages. Without this, a shared
# binary carries the user name and folder layout of whoever built it, e.g. /Users/<name>/.cargo/registry/....
# This rewrites those prefixes to neutral ones. Check a result with:  strings <binary> | grep "$HOME"   (must print nothing)
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=/home/user --remap-path-prefix=$PWD=/src/synkflow"
export CFLAGS="${CFLAGS:-} -ffile-prefix-map=$HOME=/home/user"
export CXXFLAGS="${CXXFLAGS:-} -ffile-prefix-map=$HOME=/home/user"
