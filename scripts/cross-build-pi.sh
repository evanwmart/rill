#!/usr/bin/env bash
# Cross-compile the Pi bench binaries in a Debian 13 container (the Pi's own
# distro), then leave them in target/aarch64-unknown-linux-gnu/release/.
#
#   scripts/cross-build-pi.sh                 # build the four bench binaries
#   scripts/cross-build-pi.sh -p rill         # or whatever cargo args you like
#
# Why a container and not a host toolchain: this box is openSUSE with glibc
# 2.43, the Pi is Debian 13 with 2.41, and a binary linked against the newer
# one does not run on the older. Building inside the target's own distro makes
# that mismatch impossible rather than merely unlikely.
#
# See docs/pi-bring-up.md. Deploy with:
#   rsync -a target/aarch64-unknown-linux-gnu/release/{rill,files-app,rill-compositor,rill-vector} \
#         rill-pi:~/rill/target/release/
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# The container's rustc follows rust-toolchain.toml — the pin governs every
# build, cross builds included. Baking the version into the image tag means a
# pin bump automatically builds a fresh image instead of silently reusing the
# old toolchain (which is exactly what happened when the pin first landed:
# a 1.94 image met a 1.98 pin and every crate failed).
toolchain=$(sed -n 's/^channel = "\(.*\)"/\1/p' "$repo/rust-toolchain.toml")
toolchain=${toolchain:-1.98.0}
image=rill-cross-aarch64-$toolchain
args=("$@")
if [[ ${#args[@]} -eq 0 ]]; then
    args=(-p rill -p files-app -p rill-compositor -p rill-vector)
fi

if ! docker image inspect "$image" >/dev/null 2>&1; then
    echo "==> building $image"
    docker build -f "$repo/scripts/cross/Dockerfile.aarch64" \
        --build-arg RUST_VERSION="$toolchain" -t "$image" "$repo"
fi

# A host-owned cargo home keeps rebuilds warm and keeps every file the
# container writes owned by you — a docker *volume* here is created root-owned
# and the --user build then cannot write its own registry cache.
cargo_home=${RILL_CROSS_CARGO_HOME:-$HOME/.cache/rill-cross-cargo}
mkdir -p "$cargo_home"

# `label=disable` rather than a `:z` relabel: SELinux is enforcing here, and
# the alternative recursively rewrites the security label of every file in the
# source tree to make one build container happy. Turning confinement off for
# this container touches nothing outside it.
docker run --rm -t \
    --security-opt label=disable \
    -v "$repo":/work -w /work \
    -v "$cargo_home":/cargo-home \
    -e CARGO_HOME=/cargo-home \
    -e CARGO_TARGET_DIR=/work/target \
    --user "$(id -u):$(id -g)" \
    "$image" \
    cargo build --release --locked --target aarch64-unknown-linux-gnu "${args[@]}"

echo
echo "==> built:"
ls -la "$repo/target/aarch64-unknown-linux-gnu/release/" | grep -E "rill$|rill-compositor$|rill-vector$|files-app$" || true
file "$repo"/target/aarch64-unknown-linux-gnu/release/rill 2>/dev/null || true
