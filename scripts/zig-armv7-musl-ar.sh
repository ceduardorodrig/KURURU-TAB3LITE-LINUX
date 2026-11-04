#!/usr/bin/env bash
# zig ar wrapper: the cc crate treats AR_<target> as a program name, so spaces are not allowed.
set -euo pipefail
exec zig ar "$@"
