#!/usr/bin/env bash
# zig cc wrapper for the armv7-musl Rust target.
#
# The `cc` crate appends `--target=armv7-unknown-linux-musleabihf` (the Rust triple) whenever it
# detects a clang-like compiler, and zig rejects that triple ("unknown architecture: armv7" — zig
# wants `arm-linux-musleabihf`). Drop the injected flag and hand zig its own. Everything else is
# passed through untouched, which is what the build scripts expect.
set -euo pipefail

args=()
for arg in "$@"; do
    [[ "$arg" == "--target=armv7-unknown-linux-musleabihf" ]] && continue
    args+=("$arg")
done

exec zig "${ZIG_MODE:-cc}" -target arm-linux-musleabihf "${args[@]}"
