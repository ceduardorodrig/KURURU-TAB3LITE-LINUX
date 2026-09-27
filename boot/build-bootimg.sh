#!/usr/bin/env bash
# ==============================================================================
# Kururu Project: Build Headless Boot Image for Samsung SM-T110 (PXA986)
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK_DIR="${SCRIPT_DIR}/build"
RAMDISK_DIR="${WORK_DIR}/ramdisk"
OUTPUT_IMG="${SCRIPT_DIR}/kururu_headless_boot.img"

if [ ! -f "${SCRIPT_DIR}/zImage" ]; then
    echo "[-] Error: ${SCRIPT_DIR}/zImage not found!"
    echo "    Extract zImage from your stock boot.img or TWRP recovery before building."
    exit 1
fi

echo "[+] Preparing ramdisk..."
rm -rf "${WORK_DIR}"
mkdir -p "${RAMDISK_DIR}"

# Extract base ramdisk or use supplied template
if [ -f "${SCRIPT_DIR}/stock_ramdisk.cpio.gz" ]; then
    cd "${RAMDISK_DIR}"
    gzip -dc "${SCRIPT_DIR}/stock_ramdisk.cpio.gz" | cpio -idm 2>/dev/null
    cd "${SCRIPT_DIR}"
fi

# Overlay headless configurations
cp -f "${SCRIPT_DIR}/init.rc" "${RAMDISK_DIR}/init.rc"
cp -f "${SCRIPT_DIR}/default.prop" "${RAMDISK_DIR}/default.prop"

# Pack ramdisk
echo "[+] Packing cpio.gz ramdisk..."
cd "${RAMDISK_DIR}"
find . | cpio -H newc -o | gzip -9 > "${WORK_DIR}/ramdisk.cpio.gz"
cd "${SCRIPT_DIR}"

# Build Android Boot Image
echo "[+] Building boot image with mkbootimg..."
mkbootimg \
    --kernel "${SCRIPT_DIR}/zImage" \
    --ramdisk "${WORK_DIR}/ramdisk.cpio.gz" \
    --base 0x10000000 \
    --pagesize 2048 \
    --ramdisk_offset 0x01000000 \
    --tags_offset 0x00000100 \
    -o "${OUTPUT_IMG}"

echo "[✓] Boot image generated successfully: ${OUTPUT_IMG}"
