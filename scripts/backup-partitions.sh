#!/usr/bin/env bash
# ==============================================================================
# Kururu Project: Safety Backup of Critical Samsung SM-T110 Partitions
# ==============================================================================
set -euo pipefail

BACKUP_DIR="./backups/$(date +%Y%m%d_%H%M%S)"
mkdir -p "${BACKUP_DIR}"

echo "[+] Waiting for device in TWRP Recovery mode..."
adb wait-for-recovery

echo "[+] Backing up EFS radio calibration (mmcblk0p1)..."
adb shell "dd if=/dev/block/mmcblk0p1 of=/tmp/efs.img bs=4096"
adb pull /tmp/efs.img "${BACKUP_DIR}/efs.img"
adb shell "rm /tmp/efs.img"

echo "[+] Backing up Stock Boot partition (mmcblk0p10)..."
adb shell "dd if=/dev/block/mmcblk0p10 of=/tmp/boot.img bs=4096"
adb pull /tmp/boot.img "${BACKUP_DIR}/boot.img"
adb shell "rm /tmp/boot.img"

echo "[+] Backing up Recovery partition (mmcblk0p9)..."
adb shell "dd if=/dev/block/mmcblk0p9 of=/tmp/recovery.img bs=4096"
adb pull /tmp/recovery.img "${BACKUP_DIR}/recovery.img"
adb shell "rm /tmp/recovery.img"

echo "[✓] Critical backups saved to: ${BACKUP_DIR}"
