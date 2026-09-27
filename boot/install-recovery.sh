#!/system/bin/sh
# -------------------------------------------------------------
# Mnemocine Homelab: Kururu (SM-T110) Headless Linux Node
# Native Headless Boot Hook
# -------------------------------------------------------------

export PATH=/system/xbin:/system/bin:/sbin:$PATH
LOGFILE="/data/kururu_boot.log"

chmod 777 /data 2>/dev/null
exec > "$LOGFILE" 2>&1
chmod 666 "$LOGFILE" 2>/dev/null

echo "=== [Kururu] Native Headless Boot Hook Triggered ==="

# Provisional timestamp to avoid TLS "not yet valid" errors before the network
# is up. This tablet's RTC is dead (reports 2014); NTP corrects the clock before
# the logging daemons start (bounded pre-sync below).
date -s "2026-01-01 00:00:00"

# Hardware display management is delegated to kururu-display daemon

# Reset firewall to accept incoming traffic (SSH/Tailscale)
echo "[Kururu] Flushing iptables firewall rules..."
iptables -F 2>/dev/null
iptables -X 2>/dev/null
iptables -t nat -F 2>/dev/null
iptables -P INPUT ACCEPT 2>/dev/null
iptables -P FORWARD ACCEPT 2>/dev/null
iptables -P OUTPUT ACCEPT 2>/dev/null

# Wait for /data/alpine rootfs
while [ ! -d /data/alpine/bin ]; do
    sleep 1
done

# Wait for /dev/tun
for i in $(seq 1 30); do
    if [ -e /dev/tun ]; then
        break
    fi
    sleep 1
done

mkdir -p /dev/net
if [ -e /dev/tun ]; then
    ln -sf /dev/tun /dev/net/tun
    chmod 666 /dev/tun
fi

# Prepare Alpine mountpoints
mkdir -p /data/alpine/dev/pts
mkdir -p /data/alpine/dev/net
mkdir -p /data/alpine/proc
mkdir -p /data/alpine/sys
mkdir -p /data/alpine/var/lib/tailscale
mkdir -p /data/alpine/run/wpa_supplicant
mkdir -p /data/alpine/etc/firmware

if [ -e /dev/tun ]; then
    ln -sf /dev/tun /data/alpine/dev/net/tun
fi

echo "nameserver 1.1.1.1" > /data/alpine/etc/resolv.conf
echo "nameserver 8.8.8.8" >> /data/alpine/etc/resolv.conf

# Mount host filesystems into Alpine
mount -o bind /dev /data/alpine/dev 2>/dev/null
mount -t devpts devpts /data/alpine/dev/pts 2>/dev/null
mount -o bind /proc /data/alpine/proc 2>/dev/null
mount -o bind /sys /data/alpine/sys 2>/dev/null

# Hardware Wi-Fi Power and Calibration
echo "[Kururu] Powering on Marvell Wi-Fi hardware via rfkill..."
if [ -e /sys/class/rfkill/rfkill0/state ]; then
    echo 1 > /sys/class/rfkill/rfkill0/state
fi

# Load Marvell WiFi modules
echo "[Kururu] Loading Marvell kernel modules..."
insmod /lib/modules/mlan.ko
insmod /lib/modules/sd8xxx.ko "drv_mode=1 cfg80211_wext=0xf fw_name=mrvl/sd8777_uapsta.bin max_vir_bss=1"

# Run Samsung macloader calibration if available
if [ -x /system/bin/macloader ]; then
    echo "[Kururu] Running macloader..."
    /system/bin/macloader
fi

sleep 2

# Detect interface name (wlan0 or mlan0)
WIFI_IF=""
if [ -d /sys/class/net/wlan0 ]; then
    WIFI_IF="wlan0"
elif [ -d /sys/class/net/mlan0 ]; then
    WIFI_IF="mlan0"
fi

echo "[Kururu] Detected Wi-Fi interface: $WIFI_IF"

if [ -n "$WIFI_IF" ]; then
    echo "[Kururu] Starting native wpa_supplicant on $WIFI_IF for SSID Cratos..."
    killall wpa_supplicant 2>/dev/null
    chroot /data/alpine /sbin/wpa_supplicant -B -i "$WIFI_IF" -c /etc/wpa_supplicant/wpa_supplicant.conf
    sleep 3
    # Configure IP statically for instant networking
    echo "[Kururu] Assigning static IP 192.168.3.55/24..."
    ip addr add 192.168.3.55/24 dev "$WIFI_IF" 2>/dev/null
    ip link set "$WIFI_IF" up
    ip route add default via 192.168.3.1 dev "$WIFI_IF" 2>/dev/null
else
    echo "[Kururu ERROR] No Wi-Fi interface detected!"
fi

# Start Dropbear SSH server
echo "[Kururu] Starting Dropbear SSH Server on port 22..."
killall dropbear 2>/dev/null
chroot /data/alpine /usr/sbin/dropbear -p 22 -R

# ── Bounded clock sync BEFORE starting the logging daemons ────────────
# The RTC on this tablet is dead (reports 2014); the provisional date above is
# only a placeholder. Sync via NTP so the display/wake logs carry the correct
# time from their first line. Bounded so a slow/absent network cannot stall boot.
sync_clock() {
    timeout 5 /system/xbin/busybox ntpd -q -n -p 200.160.7.186 2>/dev/null \
        || timeout 5 /system/xbin/busybox rdate -s 216.239.35.0 2>/dev/null || true
}

i=0
while [ "$i" -lt 8 ]; do
    if ping -c 1 -W 1 1.1.1.1 >/dev/null 2>&1 || ping -c 1 -W 1 192.168.3.1 >/dev/null 2>&1; then
        break
    fi
    i=$((i + 1))
    sleep 1
done
echo "[Kururu NTP] Syncing clock before daemons..." >> "$LOGFILE"
sync_clock
echo "[Kururu NTP] Clock now: $(date)" >> "$LOGFILE"

# Background service: Tailscale connection (clock already synced above)
(
    # Self-heal if the pre-boot sync failed because the network was still down.
    for _ in $(seq 1 30); do
        [ "$(date +%Y)" -ge 2026 ] && break
        ping -c 1 -W 1 1.1.1.1 >/dev/null 2>&1 && sync_clock
        sleep 2
    done

    echo "[Kururu] Starting Tailscaled daemon..." >> "$LOGFILE"
    killall tailscaled 2>/dev/null
    nohup chroot /data/alpine /usr/local/bin/tailscaled \
        --state=/var/lib/tailscale/tailscaled.state \
        --tun=tailscale0 > /data/tailscaled.log 2>&1 &

    chmod 666 /data/tailscaled.log 2>/dev/null
    sleep 3

    echo "[Kururu] Running Tailscale up..." >> "$LOGFILE"
    chroot /data/alpine /usr/local/bin/tailscale up --hostname=kururu --ssh --accept-routes=false > /data/tailscale_auth.txt 2>&1
    chmod 666 /data/tailscale_auth.txt 2>/dev/null
    echo "[Kururu Watcher] Tailscale command executed." >> "$LOGFILE"
) &

# Start Kururu Display Daemon
echo "[Kururu] Starting Kururu Display & Power Daemon..."
killall kururu-display 2>/dev/null
nohup chroot /data/alpine /usr/local/bin/kururu-display > /data/kururu-display.log 2>&1 &

# Start Kururu Wake-on-LAN (WOL) Relay Daemon on port 9096
echo "[Kururu] Starting Wake-on-LAN HTTP Relay Daemon (port 9096)..."
killall kururu-wake 2>/dev/null
# NOTE: redirect target must exist in the Android init context (/var does not
# exist there — that silently aborted the daemon at boot). Use /data instead.
nohup chroot /data/alpine /usr/local/bin/kururu-wake --daemon 9096 > /data/kururu-wol-daemon.log 2>&1 &

echo "=== [Kururu] Native Headless Boot Sequence Complete ==="
