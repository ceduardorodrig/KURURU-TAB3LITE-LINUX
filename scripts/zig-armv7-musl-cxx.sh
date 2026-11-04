#!/usr/bin/env bash
# zig c++ wrapper — same triple rewrite as the cc wrapper, see zig-armv7-musl-cc.sh for why.
set -euo pipefail
ZIG_MODE=c++ exec "$(dirname "$0")/zig-armv7-musl-cc.sh" "$@"
