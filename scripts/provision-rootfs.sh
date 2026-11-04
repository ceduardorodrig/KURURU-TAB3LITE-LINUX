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

APK_PACKAGES="ripgrep fd bat eza sd dust procs bottom"

# Alpine 3.20 (community, checked 2026-11-04) carries all eight above.
#
# `duf` is Go, not Rust, and has no Alpine package either. It cross-compiles in one command from the
# host and is streamed in (5.6 MB, static):
#   GOOS=linux GOARCH=arm GOARM=7 go install github.com/muesli/duf@latest
#   ssh kururu 'cat > /usr/bin/duf.new' < "$(go env GOPATH)/bin/linux_arm/duf"
#   ssh kururu 'chmod 755 /usr/bin/duf.new && mv /usr/bin/duf.new /usr/bin/duf'
# Install it in /usr/bin, not /usr/local/bin: non-interactive sessions on this device get
# PATH=/usr/sbin:/usr/bin:/sbin:/bin, so anything in /usr/local/bin is invisible to `ssh <host> <cmd>`
# and reads as "not found" even with the file present.
#
# `git-delta` is Rust but pulls two C dependencies (`libz-sys` and `onig_sys`), so the musl target
# needs a C compiler. There is no armv7-musl gcc here; `zig cc` is the toolchain, and the three
# wrappers in this directory exist because cargo injects the Rust triple (`--target=armv7-…`) that zig
# rejects, and because AR_<target> cannot contain a space. Build recipe (host side):
#
#   sudo pacman -S zig
#   CC_armv7_unknown_linux_musleabihf=scripts/zig-armv7-musl-cc.sh \
#   CXX_armv7_unknown_linux_musleabihf=scripts/zig-armv7-musl-cxx.sh \
#   AR_armv7_unknown_linux_musleabihf=scripts/zig-armv7-musl-ar.sh \
#   CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=rust-lld \
#     cargo install git-delta --target armv7-unknown-linux-musleabihf --root /tmp/delta-arm --locked
#   ssh kururu 'cat > /usr/bin/delta.new' < /tmp/delta-arm/bin/delta
#   ssh kururu 'chmod 755 /usr/bin/delta.new && mv /usr/bin/delta.new /usr/bin/delta'
#
# Keep `LINKER=rust-lld`: letting zig link produces `duplicate symbol: _start`, because rust-lld
# already brings the musl crt for the target.
apk add --no-cache $APK_PACKAGES

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
