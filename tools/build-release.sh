#!/usr/bin/env bash
# Builds the release images: target/glassos.img (USB) and target/glassos.iso (UEFI ISO).
#
#   tools/build-release.sh [COMMIT]
#
# COMMIT (optional) is the commit name stamped into the kernel (shown by `uname` and in crash
# reports); by default the local HEAD. Source paths of the build machine are remapped so the
# binaries do not carry local directory names.
set -euo pipefail
cd "$(dirname "$0")/.."

export GLASSOS_COMMIT="${1:-}"
export RUSTFLAGS="--remap-path-prefix=$HOME/.cargo/registry/src=/cargo --remap-path-prefix=$HOME/.rustup=/rustup --remap-path-prefix=$PWD=/glassos"
export CFLAGS="-ffile-prefix-map=$PWD=/glassos"

cargo build --release
(cd user && cargo build --release)

python3 tools/mkimage.py target/glassos.img
python3 tools/mkiso.py target/glassos.iso

if strings -n 6 target/x86_64-glassos/release/kernel.efi user/target/x86_64-unknown-none/release/lua | grep -q "$HOME"; then
    echo "build-release: local paths are still inside the binaries" >&2
    exit 1
fi
sha256sum target/glassos.img target/glassos.iso
