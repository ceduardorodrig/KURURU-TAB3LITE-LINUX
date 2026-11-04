#!/bin/sh
# Provision the Alpine chroot with the operator toolset this fleet's rules assume.
#
# The "Rust before GNU" rule applies to every agent that touches this device, and a bare Alpine rootfs
# satisfies none of it: on 2026-11-04 `eza`, `rg`, `fd`, `bat`, `sd`, `dust`, `procs` and `btm` were
# all missing, so `eza` failed mid-command (that is how it was noticed) and every other listing fell
# back to GNU — either breaking the rule or breaking the command. Run this after any rootfs bootstrap
# or reflash: the chroot lives in /data/alpine and the boot hook does not rebuild it.
#
# Usage (from the host, the script is streamed, never copied into the tree):
#   ssh kururu 'sh -s' < scripts/provision-rootfs.sh
#
# Idempotent: apk skips packages that are already installed.
set -eu

PACKAGES="ripgrep fd bat eza sd dust procs bottom"

# Alpine 3.20 (community, checked 2026-11-04) carries all eight above. `git-delta` and `duf` are NOT
# packaged there: they would have to be cross-compiled with `cargo build --target
# armv7-unknown-linux-musleabihf` and streamed in, and neither is required to operate this device —
# there is no git installed either, and `dust` covers the disk question `duf` answers elsewhere.
apk add --no-cache $PACKAGES

status=0
for tool in rg fd bat eza sd dust procs btm; do
    if path=$(command -v "$tool"); then
        printf '%-6s %s\n' "$tool" "$path"
    else
        printf '%-6s MISSING\n' "$tool"
        status=1
    fi
done

exit "$status"
