//! Kururu Power Sentinel
//!
//! Autonomous power outage detection, safe shutdown coordinator,
//! and automatic Wake-on-LAN recovery daemon for Mnemocine Homelab.
//!
//! Target: Samsung Galaxy Tab 3 Lite (SM-T110) running Alpine Linux 3.20 (ARMv7)

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// Defaults
const DEFAULT_BLACKOUT_TIMEOUT_SECS: u64 = 180;
const DEFAULT_RECOVERY_QUARANTINE_SECS: u64 = 180;
const PROBE_INTERVAL_SECS: u64 = 5;
const CRITICAL_BATTERY_PERCENT: u8 = 20;

// SMB327 / SMB328 hardware charge termination (float) voltage cap (HB-105):
// Register 0x02 (SMB327_FLOAT_VOLTAGE) bits 6:1 = 0x21 (0x42) -> 4.14 V (~90% Li-ion ceiling).
// Prevents calendar ageing at 4.32 V while keeping full AC mains power path active.
const SMB327_FLOAT_VOLTAGE_REG: &str = "02\n";
const SMB327_FLOAT_VOLTAGE_4140MV_HEX: &str = "42\n";

// Accelerometer physical motion detection
// Calibration: Cat jump / desk shockwave settles at ~86 units max. Handheld pickup reaches 310-530 units.
const ORIENTATION_SHIFT_THRESHOLD: f64 = 180.0;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Vector3D {
    x: f64,
    y: f64,
    z: f64,
}

impl Vector3D {
    fn distance_to(&self, other: &Vector3D) -> f64 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        let dz = self.z - other.z;
        (dx * dx + dy * dy + dz * dz).sqrt()
    }
}

/// Reads current raw acceleration vector from sysfs
fn read_accelerometer() -> Option<Vector3D> {
    let content = fs::read_to_string("/sys/class/sensors/accelerometer_sensor/raw_data").ok()?;
    let parts: Vec<&str> = content.trim().split(',').collect();
    if parts.len() >= 3 {
        let x = parts[0].trim().parse::<f64>().ok()?;
        let y = parts[1].trim().parse::<f64>().ok()?;
        let z = parts[2].trim().parse::<f64>().ok()?;
        Some(Vector3D { x, y, z })
    } else {
        None
    }
}

// Network targets
const KAVURE_LAN_IP: &str = "192.168.3.41";
const PSICOPOMPO_LAN_IP: &str = "192.168.3.100";
const KAVURE_TS_IP: &str = "100.124.146.77";
const PSICOPOMPO_TS_IP: &str = "100.82.51.112";
const WOL_LOCAL_URL: &str = "http://127.0.0.1:9096";

// Dead-man switch: the guardian reports IN, instead of a monitor polling it from a dev machine.
// ntfy is self-hosted on ybytu and private to the tailnet (unauthenticated topics by design), and
// it is already the fleet's alert channel. Absence of these pushes is the alarm.
const NTFY_ALERTS_URL: &str = "http://100.115.253.109:8083/alerts";
const NTFY_PUSH_INTERVAL_SECS: u64 = 300;

// External WAN probe IPs
const WAN_PRIMARY_IP: &str = "1.1.1.1";
const WAN_BACKUP_IP: &str = "8.8.8.8";

// Smart Lamps (Tuya Local 10W Bulbs) — physical mains grid witnesses
// These bulbs are screwed into standard house sockets without battery or UPS backup.
// If any bulb responds on TCP port 6668, mains grid electricity is definitively online.
const SMART_LAMPS: [&str; 4] = [
    "192.168.3.36:6668", // Fridge lamp
    "192.168.3.37:6668", // Door lamp
    "192.168.3.38:6668", // Sink lamp
    "192.168.3.39:6668", // Desk lamp
];

/// Probes whether any smart lamp on the mains AC grid is reachable via TCP port 6668.
/// Returns true if at least one lamp responds, conclusively proving that mains power is active.
fn is_mains_grid_online() -> bool {
    for addr_str in &SMART_LAMPS {
        if let Ok(addr) = addr_str.parse::<SocketAddr>() {
            if TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok() {
                return true;
            }
        }
    }
    false
}

/// Probes a node across both local LAN and Tailscale mesh
fn is_node_online(lan_ip: &str, ts_ip: &str) -> bool {
    ping_check(lan_ip, 1) || ping_check(ts_ip, 1)
}

// Log file location: ONE canonical path, resolved once at startup and announced in the first
// line. The previous code walked two candidates and broke at the first that opened, silently —
// so `/var/log/kururu-sentinel.log` became a decoy frozen on 2026-10-05 while the live log sat in
// `/data` and nobody could tell which file to read (HB-108).
const LOG_CANDIDATES: [&str; 2] = [
    "/data/kururu-sentinel.log",
    "/var/log/kururu-sentinel.log",
];
/// Rotate at 2 MiB and keep three generations (`…log.1` oldest kept generation is `.3`).
/// 9.6 MiB in 30 days on an 8 GiB eMMC with a 5.1 GiB rootfs, with no rotation, was growing
/// without bound (HB-108).
const LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;
const LOG_GENERATIONS: u8 = 3;

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Resolves the log file once: the first candidate whose directory accepts a write wins.
/// An unwritable candidates list still yields a path, so logging never panics — it degrades
/// to stdout only.
fn log_path() -> &'static Path {
    LOG_PATH.get_or_init(|| {
        for candidate in LOG_CANDIDATES {
            if OpenOptions::new()
                .create(true)
                .append(true)
                .open(candidate)
                .is_ok()
            {
                return PathBuf::from(candidate);
            }
        }
        PathBuf::from(LOG_CANDIDATES[LOG_CANDIDATES.len() - 1])
    })
}

/// Shifts the log by size: `log.2`→`log.3`, `log.1`→`log.2`, `log`→`log.1`.
fn rotate_if_needed(path: &Path) {
    let too_big = fs::metadata(path)
        .map(|meta| meta.len() > LOG_MAX_BYTES)
        .unwrap_or(false);
    if !too_big {
        return;
    }
    let base = path.to_string_lossy().to_string();
    for generation in (1..LOG_GENERATIONS).rev() {
        let _ = fs::rename(
            format!("{}.{}", base, generation),
            format!("{}.{}", base, generation + 1),
        );
    }
    let _ = fs::rename(&base, format!("{}.1", base));
}

/// Returns formatted timestamp "[HH:MM:SS]" in Brasilia Time (UTC-3)
fn timestamp_str() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // UTC-3 = subtract 3 hours (10800s)
    let local = secs.saturating_sub(3 * 3600);
    let h = (local / 3600) % 24;
    let m = (local / 60) % 60;
    let s = local % 60;
    format!("[{:02}:{:02}:{:02}]", h, m, s)
}

fn log_event(msg: &str) {
    let ts = timestamp_str();
    let line = format!("{} {}", ts, msg);

    // One writer owns the log. Printing unconditionally duplicated every line as soon as a launcher
    // redirected stdout into the same storage — the boot hook sends it to
    // `/data/kururu-sentinel-stdout.log`, a second unbounded file with no rotation. The console copy
    // is therefore opt-in: KURURU_CONSOLE=1 for a foreground/debug run.
    if std::env::var_os("KURURU_CONSOLE").is_some() {
        println!("{}", line);
    }

    let path = log_path();
    rotate_if_needed(path);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{}", line);
    }
}

/// Reads tablet battery charge capacity percentage (0-100%) from sysfs
fn read_battery_capacity() -> Option<u8> {
    let ps_dir = Path::new("/sys/class/power_supply");
    for bat_name in &["battery", "sec-battery", "sec-fuelgauge"] {
        let cap_file = ps_dir.join(bat_name).join("capacity");
        if let Ok(val) = fs::read_to_string(&cap_file) {
            if let Ok(num) = val.trim().parse::<u8>() {
                return Some(num);
            }
        }
    }
    None
}

/// Enforces the SMB327/SMB328 hardware float voltage cap at 4.14 V (`0x42` in reg `0x02`, HB-105).
/// Idempotent: reads register `0x02` first and only writes if the kernel driver reset it to default (`0x56`).
fn enforce_charger_float_voltage_cap() {
    let chg_dir = Path::new("/sys/class/power_supply/sec-charger");
    let reg_path = chg_dir.join("reg");
    let data_path = chg_dir.join("data");
    if !reg_path.exists() || !data_path.exists() {
        return;
    }
    if fs::write(&reg_path, SMB327_FLOAT_VOLTAGE_REG).is_ok() {
        if let Ok(curr) = fs::read_to_string(&data_path) {
            if !curr.trim().eq_ignore_ascii_case("42") {
                if fs::write(&data_path, SMB327_FLOAT_VOLTAGE_4140MV_HEX).is_ok() {
                    log_event(&format!(
                        "[BATTERY-CAP] Applied SMB327 float voltage cap 4.14V (reg 0x02: 0x{} -> 0x42).",
                        curr.trim()
                    ));
                }
            }
        }
    }
}

/// Reads the SMB327 float-voltage register (`0x02`) as the driver reports it — hex, the same
/// representation `enforce_charger_float_voltage_cap` writes.
fn read_float_voltage_reg() -> Option<String> {
    let chg_dir = Path::new("/sys/class/power_supply/sec-charger");
    let reg_path = chg_dir.join("reg");
    let data_path = chg_dir.join("data");
    if fs::write(&reg_path, SMB327_FLOAT_VOLTAGE_REG).is_err() {
        return None;
    }
    fs::read_to_string(&data_path)
        .ok()
        .map(|value| value.trim().to_string())
}

/// Short label for the finite-state machine, used in the pushed heartbeat.
fn state_label(state: &SentinelState) -> &'static str {
    match state {
        SentinelState::AcNormal => "AcNormal",
        SentinelState::PortableUsage => "PortableUsage",
        SentinelState::CableFault => "CableFault",
        SentinelState::BlackoutPending(_) => "BlackoutPending",
        SentinelState::ExecutingShutdown => "ExecutingShutdown",
        SentinelState::WaitingForPower => "WaitingForPower",
        SentinelState::StabilizingRecovery(_) => "StabilizingRecovery",
        SentinelState::ClosedLoopWake => "ClosedLoopWake",
    }
}

/// Pushes one state line to the fleet's ntfy topic. The guardian reports *in*, so silence is the
/// alarm — no monitor has to poll a host that may be the one that died. Every failure is logged
/// and swallowed: a broken network must never take the guardian down with it.
fn push_state(state: &SentinelState, debit: u64) {
    let battery = read_battery_capacity()
        .map(|percent| format!("{}%", percent))
        .unwrap_or_else(|| "N/A".to_string());
    let float_reg = read_float_voltage_reg().unwrap_or_else(|| "?".to_string());
    let ac = if is_ac_online() { 1 } else { 0 };
    let body = format!(
        "kururu guardian: state={} ac={} bat={} float_reg_0x02={} debit={}s",
        state_label(state),
        ac,
        battery,
        float_reg,
        debit
    );
    let headers = [("Title", "kururu power guardian"), ("Tags", "battery,guardian")];
    if let Err(err) = send_http_post_with(NTFY_ALERTS_URL, &body, &headers, 3000) {
        log_event(&format!("[NTFY] heartbeat push failed: {}", err));
    }
}

/// Checks physical AC mains power status on Kururu hardware
/// Inspects Linux sysfs power_supply classes
fn is_ac_online() -> bool {
    let ps_dir = Path::new("/sys/class/power_supply");
    if !ps_dir.exists() {
        return true; // Default fallback if running outside hardware
    }

    // Direct check of known charger nodes
    for name in &["ac", "usb", "sec-charger"] {
        let online_file = ps_dir.join(name).join("online");
        if let Ok(val) = fs::read_to_string(&online_file) {
            if val.trim() == "1" {
                return true;
            }
        }
    }

    // Secondary check: battery actively charging
    let battery_status = ps_dir.join("battery").join("status");
    if let Ok(val) = fs::read_to_string(&battery_status) {
        if val.trim() == "Charging" {
            return true;
        }
    }

    false
}

/// Probes an IP address using ICMP ping with a small timeout
///
/// Absolute paths first. This daemon is started by the Android boot hook, whose PATH is
/// `/system/xbin:/system/bin:…` and contains neither `/bin` nor `/usr/bin`, so a bare
/// `Command::new("ping")` failed there while the network was perfectly healthy — on 2026-11-04 the
/// FSM read *both* hosts as DOWN right after boot. During a cable loss that is a false blackout,
/// which ends in a homelab shutdown. A missing binary falls through to the next candidate; a
/// reachable binary that fails the probe returns false immediately.
const PING_BINARIES: [&str; 3] = ["/bin/ping", "/usr/bin/ping", "ping"];

fn ping_check(ip: &str, timeout_secs: u64) -> bool {
    for candidate in PING_BINARIES {
        let result = Command::new(candidate)
            .args(["-c", "1", "-W", &timeout_secs.to_string(), ip])
            .output();
        if let Ok(out) = result {
            return out.status.success();
        }
    }
    false
}

/// Checks external WAN connectivity
fn is_wan_online() -> bool {
    if ping_check(WAN_PRIMARY_IP, 2) {
        return true;
    }
    ping_check(WAN_BACKUP_IP, 2)
}

/// Sends an HTTP POST request to trigger remote shutdown
fn send_http_post(url_str: &str, timeout_ms: u64) -> Result<String, String> {
    send_http_post_with(url_str, "", &[], timeout_ms)
}

/// POSTs a body plus extra headers (ntfy reads the message from the body and the metadata from
/// headers such as `Title` and `Tags`). One implementation for every POST this daemon makes.
fn send_http_post_with(
    url_str: &str,
    body: &str,
    headers: &[(&str, &str)],
    timeout_ms: u64,
) -> Result<String, String> {
    // Parse URL (format: http://host:port/path)
    let url_no_proto = url_str.strip_prefix("http://").unwrap_or(url_str);
    let (host_port, path) = match url_no_proto.split_once('/') {
        Some((hp, p)) => (hp, format!("/{}", p)),
        None => (url_no_proto, "/".to_string()),
    };

    let addrs: Vec<SocketAddr> = host_port
        .to_socket_addrs()
        .map_err(|e| format!("Address resolution error for {}: {}", host_port, e))?
        .collect();

    if addrs.is_empty() {
        return Err(format!("No address resolved for {}", host_port));
    }

    let mut stream = TcpStream::connect_timeout(&addrs[0], Duration::from_millis(timeout_ms))
        .map_err(|e| format!("Connect error to {}: {}", host_port, e))?;

    let _ = stream.set_read_timeout(Some(Duration::from_millis(timeout_ms)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(timeout_ms)));

    let mut request = format!("POST {} HTTP/1.1\r\nHost: {}\r\n", path, host_port);
    for (name, value) in headers {
        request.push_str(&format!("{}: {}\r\n", name, value));
    }
    request.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    ));

    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("Write failed: {}", e))?;

    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let resp_str = String::from_utf8_lossy(&response).to_string();

    if resp_str.contains("200 OK") {
        Ok(resp_str)
    } else {
        Err(format!("Non-200 response: {}", resp_str.lines().next().unwrap_or("empty")))
    }
}

/// Dispatches shutdown request to a node using concurrent race (LAN ⚡ Tailscale)
fn trigger_node_shutdown(node_name: &str, lan_ip: &str, ts_ip: &str) {
    let (tx, rx) = std::sync::mpsc::sync_channel(2);

    let lan_url = format!("http://{}:9096/power/shutdown", lan_ip);
    let tx_lan = tx.clone();
    let name_lan = node_name.to_string();
    thread::spawn(move || {
        let res = send_http_post(&lan_url, 3000);
        let _ = tx_lan.send((name_lan, "LAN", res));
    });

    let ts_url = format!("http://{}:9096/power/shutdown", ts_ip);
    let tx_ts = tx;
    let name_ts = node_name.to_string();
    thread::spawn(move || {
        let res = send_http_post(&ts_url, 3000);
        let _ = tx_ts.send((name_ts, "Tailscale", res));
    });

    let mut accepted = false;
    for _ in 0..2 {
        if let Ok((name, iface, res)) = rx.recv_timeout(Duration::from_millis(3200)) {
            match res {
                Ok(_) => {
                    if !accepted {
                        log_event(&format!(
                            "[SHUTDOWN] {} accepted shutdown request via {} (HTTP 200).",
                            name, iface
                        ));
                        accepted = true;
                    }
                }
                Err(e) => {
                    log_event(&format!(
                        "[SHUTDOWN] Probe to {} via {} failed: {}",
                        name, iface, e
                    ));
                }
            }
        }
    }

    if !accepted {
        log_event(&format!(
            "[SHUTDOWN] CRITICAL: Failed to dispatch shutdown to {} across both LAN and Tailscale!",
            node_name
        ));
    }
}

/// Dispatches simultaneous shutdown commands to Kavure and Psicopompo
fn trigger_parallel_shutdown() {
    log_event("[ACTION: SHUTDOWN] Emitting parallel shutdown signals via LAN ⚡ Tailscale...");

    let h1 = thread::spawn(|| {
        trigger_node_shutdown("Kavure", KAVURE_LAN_IP, KAVURE_TS_IP);
    });

    let h2 = thread::spawn(|| {
        trigger_node_shutdown("Psicopompo", PSICOPOMPO_LAN_IP, PSICOPOMPO_TS_IP);
    });

    let _ = h1.join();
    let _ = h2.join();
}

/// Dispatches simultaneous WoL magic packets to Kavure and Psicopompo
fn trigger_parallel_wake() {
    log_event("[ACTION: WAKE] Emitting simultaneous WoL magic packets via kururu-wake daemon...");

    let h1 = thread::spawn(|| {
        let url = format!("{}/wake/kavure", WOL_LOCAL_URL);
        match send_http_post(&url, 2000) {
            Ok(_) => log_event("[WAKE] WoL packet dispatched to Kavure."),
            Err(e) => log_event(&format!("[WAKE] Error dispatching WoL to Kavure: {}", e)),
        }
    });

    let h2 = thread::spawn(|| {
        let url = format!("{}/wake/psicopompo", WOL_LOCAL_URL);
        match send_http_post(&url, 2000) {
            Ok(_) => log_event("[WAKE] WoL packet dispatched to Psicopompo."),
            Err(e) => log_event(&format!("[WAKE] Error dispatching WoL to Psicopompo: {}", e)),
        }
    });

    let _ = h1.join();
    let _ = h2.join();
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum BlackoutReason {
    StaticDockLostAc,
    WanLostInPortable,
    CriticalBattery,
}

#[derive(Debug, PartialEq, Eq)]
enum SentinelState {
    AcNormal,
    PortableUsage,
    CableFault,
    BlackoutPending(BlackoutReason),
    ExecutingShutdown,
    WaitingForPower,
    StabilizingRecovery(u64),
    ClosedLoopWake,
}

/// Single-instance guard. Two starters exist in this image — the Android boot hook (which
/// `killall`s and starts the binary) and `kururu-wake`, whose supervisor loop spawns it when a
/// `/proc` scan finds nothing. At boot they race: on 2026-11-04 both won and two daemons ran,
/// doubling the ntfy pushes, the shutdown requests and the log lines. The guard makes the daemon
/// own its uniqueness, so no caller can create a second instance.
///
/// The pid file is a hint, not a lock: a crashed daemon leaves it behind, so a stale pid is
/// detected (`/proc/<pid>` absent) and taken over instead of blocking the next start forever.
const PID_FILE: &str = "/data/kururu-sentinel.pid";

fn claim_single_instance(path: &Path) -> bool {
    let pid = std::process::id();

    for _ in 0..2 {
        // O_EXCL: the create IS the lock. A read-then-write let both boot starters through on
        // 2026-11-04 (pids 548 and 553 both ran), because neither had written when the other read.
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut file) => {
                let _ = writeln!(file, "{}", pid);
                return true;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = fs::read_to_string(path)
                    .ok()
                    .and_then(|content| content.trim().parse::<u32>().ok());
                match holder {
                    // A live holder means this start loses, by design.
                    Some(other) if Path::new(&format!("/proc/{}", other)).exists() => return false,
                    // Stale pid (crashed daemon): take the file over instead of blocking forever.
                    _ => {
                        let _ = fs::remove_file(path);
                    }
                }
            }
            // Unwritable pid file must not stop a guardian: run, accepting a theoretical duplicate.
            Err(_) => return true,
        }
    }

    false
}

fn main() {
    if !claim_single_instance(Path::new(PID_FILE)) {
        eprintln!("kururu-sentinel: another instance already holds {}", PID_FILE);
        return;
    }

    // The Android boot hook exports its own PATH (`/system/bin:/system/xbin`). Normalise it once so
    // every helper this daemon shells out to resolves like it does in an interactive Alpine shell.
    std::env::set_var("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");

    log_event("=================================================================");
    log_event("Kururu Power Sentinel v1.3 (Autonomous Homelab Power Guardian)");
    log_event(&format!(
        "Config: Blackout threshold: {}s | Quarantine: {}s | Probes: {}s",
        DEFAULT_BLACKOUT_TIMEOUT_SECS, DEFAULT_RECOVERY_QUARANTINE_SECS, PROBE_INTERVAL_SECS
    ));
    log_event(&format!(
        "Physics: Dynamic Dock Baseline | Motion Threshold: {:.0} units",
        ORIENTATION_SHIFT_THRESHOLD
    ));
    log_event(&format!(
        "Targets: Kavure ({}/{}) | Psicopompo ({}/{})",
        KAVURE_LAN_IP, KAVURE_TS_IP, PSICOPOMPO_LAN_IP, PSICOPOMPO_TS_IP
    ));
    log_event("=================================================================");

    let mut state = SentinelState::AcNormal;
    let mut blackout_debit_secs: u64 = 0;
    let mut last_heartbeat = Instant::now();
    let mut last_push = Instant::now();
    let mut dock_baseline: Option<Vector3D> = None;

    log_event(&format!(
        "Logging to {} (rotation at {} KiB, {} generations)",
        log_path().display(),
        LOG_MAX_BYTES / 1024,
        LOG_GENERATIONS
    ));

    enforce_charger_float_voltage_cap();

    // One push at startup so the fleet knows the guardian (re)started; failure is non-fatal.
    push_state(&state, blackout_debit_secs);

    loop {
        let ac = is_ac_online();
        if ac {
            enforce_charger_float_voltage_cap();
        }

        match state {
            SentinelState::AcNormal => {
                // Drain any residual blackout debit gradually (10s per 5s cycle)
                if blackout_debit_secs > 0 {
                    blackout_debit_secs = blackout_debit_secs.saturating_sub(PROBE_INTERVAL_SECS * 2);
                }

                // Continuously calibrate dock baseline orientation while resting on AC
                if let Some(curr_vec) = read_accelerometer() {
                    match dock_baseline {
                        None => dock_baseline = Some(curr_vec),
                        Some(ref mut b) => {
                            // Low-pass filter (90% existing baseline, 10% new sample)
                            b.x = b.x * 0.9 + curr_vec.x * 0.1;
                            b.y = b.y * 0.9 + curr_vec.y * 0.1;
                            b.z = b.z * 0.9 + curr_vec.z * 0.1;
                        }
                    }
                }

                if !ac {
                    let curr_vec = read_accelerometer();
                    let delta = match (curr_vec, dock_baseline) {
                        (Some(c), Some(b)) => c.distance_to(&b),
                        _ => 0.0,
                    };
                    let wan = is_wan_online();
                    let mains = is_mains_grid_online();

                    if delta >= ORIENTATION_SHIFT_THRESHOLD {
                        if wan || mains {
                            log_event(&format!(
                                "[ASSESSMENT] Confidence: 10% | Physical pickup (delta={:.1} >= {:.1}) with active network -> PortableUsage.",
                                delta, ORIENTATION_SHIFT_THRESHOLD
                            ));
                            state = SentinelState::PortableUsage;
                        } else {
                            log_event(&format!(
                                "[ASSESSMENT] Confidence: 85% | Physical pickup (delta={:.1}) BUT network offline -> BlackoutPending(WanLostInPortable).",
                                delta
                            ));
                            state = SentinelState::BlackoutPending(BlackoutReason::WanLostInPortable);
                        }
                    } else if mains {
                        // Static on dock without AC, BUT smart lamps prove mains grid electricity is healthy!
                        log_event(&format!(
                            "[ASSESSMENT] Confidence: 0% | Static on dock (delta={:.1} < {:.1}), AC lost BUT smart lamps are ONLINE! Mains grid healthy; cable fault or unplugged charger -> CableFault.",
                            delta, ORIENTATION_SHIFT_THRESHOLD
                        ));
                        state = SentinelState::CableFault;
                        blackout_debit_secs = 0;
                    } else {
                        // Static on dock without AC, AND smart lamps are offline!
                        if wan {
                            log_event(&format!(
                                "[ASSESSMENT] Confidence: 90% | Static on dock without AC and smart lamps offline (WAN alive via UPS, delta={:.1}) -> BlackoutPending(StaticDockLostAc).",
                                delta
                            ));
                            state = SentinelState::BlackoutPending(BlackoutReason::StaticDockLostAc);
                        } else {
                            log_event(&format!(
                                "[ASSESSMENT] Confidence: 100% | Static on dock without AC, smart lamps offline AND WAN offline (delta={:.1}) -> BlackoutPending(StaticDockLostAc).",
                                delta
                            ));
                            state = SentinelState::BlackoutPending(BlackoutReason::StaticDockLostAc);
                        }
                    }
                } else if last_heartbeat.elapsed() >= Duration::from_secs(60) {
                    let kav_up = is_node_online(KAVURE_LAN_IP, KAVURE_TS_IP);
                    let psi_up = is_node_online(PSICOPOMPO_LAN_IP, PSICOPOMPO_TS_IP);
                    let bat_str = match read_battery_capacity() {
                        Some(c) => format!("bat={}%", c),
                        None => "bat=N/A".to_string(),
                    };
                    let base_str = match dock_baseline {
                        Some(b) => format!("dock=({:.0},{:.0},{:.0})", b.x, b.y, b.z),
                        None => "dock=calibrating".to_string(),
                    };
                    log_event(&format!(
                        "[HEARTBEAT] state=AcNormal ac_online=1 {} kavure_ts={} psicopompo_ts={} {} debit={}s",
                        bat_str,
                        if kav_up { "UP" } else { "DOWN" },
                        if psi_up { "UP" } else { "DOWN" },
                        base_str,
                        blackout_debit_secs
                    ));
                    last_heartbeat = Instant::now();
                }
            }

            SentinelState::PortableUsage => {
                if ac {
                    log_event("[INFO] AC power reconnected. Resuming AcNormal state.");
                    state = SentinelState::AcNormal;
                    dock_baseline = None; // Trigger immediate recalibration to dock
                } else {
                    // Check 1: Critical battery level (Safety Watchdog)
                    if let Some(cap) = read_battery_capacity() {
                        if cap <= CRITICAL_BATTERY_PERCENT {
                            log_event(&format!(
                                "[CRITICAL] Tablet battery critically low ({}% <= {}%) in portable mode! Escalating to BlackoutPending.",
                                cap, CRITICAL_BATTERY_PERCENT
                            ));
                            state = SentinelState::BlackoutPending(BlackoutReason::CriticalBattery);
                        }
                    }

                    // Check 2: Physical return to resting dock position without AC
                    if state == SentinelState::PortableUsage {
                        let curr_vec = read_accelerometer();
                        let delta = match (curr_vec, dock_baseline) {
                            (Some(c), Some(b)) => c.distance_to(&b),
                            _ => 999.0,
                        };

                        if delta < ORIENTATION_SHIFT_THRESHOLD {
                            let mains = is_mains_grid_online();
                            if mains {
                                log_event(&format!(
                                    "[ASSESSMENT] Confidence: 0% | Tablet returned to resting dock position without AC, but smart lamps are ONLINE (delta={:.1} < {:.1}) -> CableFault.",
                                    delta, ORIENTATION_SHIFT_THRESHOLD
                                ));
                                state = SentinelState::CableFault;
                            } else {
                                log_event(&format!(
                                    "[ASSESSMENT] Confidence: 90% | Tablet returned to resting dock position without AC and smart lamps offline (delta={:.1} < {:.1}) -> BlackoutPending(StaticDockLostAc).",
                                    delta, ORIENTATION_SHIFT_THRESHOLD
                                ));
                                state = SentinelState::BlackoutPending(BlackoutReason::StaticDockLostAc);
                            }
                        }
                    }

                    // Check 3: External WAN probe lost
                    if state == SentinelState::PortableUsage {
                        let wan = is_wan_online();
                        if !wan {
                            log_event("[ASSESSMENT] Confidence: 85% | WAN connectivity lost during battery operation -> BlackoutPending(WanLostInPortable).");
                            state = SentinelState::BlackoutPending(BlackoutReason::WanLostInPortable);
                        }
                    }
                }
            }

            SentinelState::CableFault => {
                if ac {
                    log_event("[INFO] AC power reconnected. Resuming AcNormal state.");
                    state = SentinelState::AcNormal;
                    dock_baseline = None; // Trigger immediate recalibration to dock
                } else {
                    // Check 1: Critical battery level (Safety Watchdog)
                    if let Some(cap) = read_battery_capacity() {
                        if cap <= CRITICAL_BATTERY_PERCENT {
                            log_event(&format!(
                                "[CRITICAL] Tablet battery critically low ({}% <= {}%) during CableFault! Escalating to BlackoutPending.",
                                cap, CRITICAL_BATTERY_PERCENT
                            ));
                            state = SentinelState::BlackoutPending(BlackoutReason::CriticalBattery);
                        }
                    }

                    // Check 2: Physical movement
                    if state == SentinelState::CableFault {
                        let curr_vec = read_accelerometer();
                        let delta = match (curr_vec, dock_baseline) {
                            (Some(c), Some(b)) => c.distance_to(&b),
                            _ => 999.0,
                        };

                        if delta >= ORIENTATION_SHIFT_THRESHOLD {
                            log_event(&format!(
                                "[TRANSITION] Tablet picked up from dock during CableFault (delta={:.1} >= {:.1}) -> PortableUsage.",
                                delta, ORIENTATION_SHIFT_THRESHOLD
                            ));
                            state = SentinelState::PortableUsage;
                        }
                    }

                    // Check 3: Real power grid outage occurred while in CableFault
                    if state == SentinelState::CableFault {
                        let mains = is_mains_grid_online();
                        if !mains {
                            log_event("[ALERT] Smart lamps went offline while in CableFault! Power grid failure detected -> BlackoutPending(StaticDockLostAc).");
                            state = SentinelState::BlackoutPending(BlackoutReason::StaticDockLostAc);
                        }
                    }

                    // Periodic heartbeat during CableFault
                    if state == SentinelState::CableFault && last_heartbeat.elapsed() >= Duration::from_secs(60) {
                        let bat_str = match read_battery_capacity() {
                            Some(c) => format!("bat={}%", c),
                            None => "bat=N/A".to_string(),
                        };
                        log_event(&format!(
                            "[HEARTBEAT] state=CableFault ac_online=0 mains=UP {} standing by (no shutdown)",
                            bat_str
                        ));
                        last_heartbeat = Instant::now();
                    }
                }
            }

            SentinelState::BlackoutPending(reason) => {
                if ac {
                    // Leaky bucket drain: AC present drains 2x probe interval per cycle
                    let drain_rate = PROBE_INTERVAL_SECS * 2;
                    blackout_debit_secs = blackout_debit_secs.saturating_sub(drain_rate);
                    log_event(&format!(
                        "[RECOVERY] AC detected in BlackoutPending. Draining debit: {}/{}s remaining.",
                        blackout_debit_secs, DEFAULT_BLACKOUT_TIMEOUT_SECS
                    ));
                    if blackout_debit_secs == 0 {
                        log_event("[EVENT] AC power fully stabilized. Returning to AcNormal state.");
                        state = SentinelState::AcNormal;
                    }
                } else {
                    let curr_vec = read_accelerometer();
                    let delta = match (curr_vec, dock_baseline) {
                        (Some(c), Some(b)) => c.distance_to(&b),
                        _ => 0.0,
                    };
                    let wan = is_wan_online();

                    // If smart lamps come back online while in StaticDockLostAc, mains power was restored!
                    if matches!(reason, BlackoutReason::StaticDockLostAc) && is_mains_grid_online() {
                        log_event("[TRANSITION] Smart lamps became reachable during blackout countdown! Mains power restored -> CableFault.");
                        state = SentinelState::CableFault;
                        blackout_debit_secs = 0;
                    }

                    // Check if conditions warrant de-escalation back to PortableUsage
                    let deescalate = match reason {
                        BlackoutReason::StaticDockLostAc => {
                            // User physically approached dock and picked up tablet while WAN is alive
                            if delta >= ORIENTATION_SHIFT_THRESHOLD && wan {
                                log_event(&format!(
                                    "[TRANSITION] Human picked up tablet from dock during countdown (delta={:.1} >= {:.1}, wan=OK). Transitioning to PortableUsage.",
                                    delta, ORIENTATION_SHIFT_THRESHOLD
                                ));
                                true
                            } else {
                                false
                            }
                        }
                        BlackoutReason::WanLostInPortable => {
                            // Temporary Wi-Fi/WAN glitch recovered while in handheld mode
                            if wan {
                                log_event("[TRANSITION] WAN connectivity recovered during portable operation. Transitioning back to PortableUsage.");
                                true
                            } else {
                                false
                            }
                        }
                        BlackoutReason::CriticalBattery => false,
                    };

                    if deescalate {
                        state = SentinelState::PortableUsage;
                        blackout_debit_secs = 0;
                    } else {
                        blackout_debit_secs += PROBE_INTERVAL_SECS;
                        log_event(&format!(
                            "[DEBIT] ac_online=0 reason={:?} delta={:.1} wan={} debit={}/{}s",
                            reason, delta, if wan { "OK" } else { "FAIL" }, blackout_debit_secs, DEFAULT_BLACKOUT_TIMEOUT_SECS
                        ));

                        if blackout_debit_secs >= DEFAULT_BLACKOUT_TIMEOUT_SECS {
                            log_event(&format!(
                                "[CRITICAL] Blackout debit threshold reached (3 minutes) under reason {:?}. Triggering safe shutdown!",
                                reason
                            ));
                            state = SentinelState::ExecutingShutdown;
                        }
                    }
                }
            }

            SentinelState::ExecutingShutdown => {
                trigger_parallel_shutdown();

                // Wait for hosts to enter S5
                log_event("[SHUTDOWN] Waiting for hosts to enter S5 soft-off state...");
                let mut kav_down = false;
                let mut psi_down = false;

                for _ in 0..12 {
                    thread::sleep(Duration::from_secs(5));
                    if !kav_down && !is_node_online(KAVURE_LAN_IP, KAVURE_TS_IP) {
                        kav_down = true;
                        log_event("[CONFIRM] Kavure confirmed offline (S5).");
                    }
                    if !psi_down && !is_node_online(PSICOPOMPO_LAN_IP, PSICOPOMPO_TS_IP) {
                        psi_down = true;
                        log_event("[CONFIRM] Psicopompo confirmed offline (S5).");
                    }
                    if kav_down && psi_down {
                        break;
                    }
                }

                log_event("[STATE] Both hosts offline. No-break battery preserved. Transitioning to WaitingForPower.");
                state = SentinelState::WaitingForPower;
            }

            SentinelState::WaitingForPower => {
                if ac {
                    log_event("[EVENT] AC power detected! Entering stabilization quarantine (target: 180s continuous).");
                    state = SentinelState::StabilizingRecovery(0);
                }
            }

            SentinelState::StabilizingRecovery(elapsed) => {
                if !ac {
                    log_event("[WARN] AC power flapped/lost during stabilization! Resetting quarantine to WaitingForPower.");
                    state = SentinelState::WaitingForPower;
                } else {
                    let next_elapsed = elapsed + PROBE_INTERVAL_SECS;
                    if next_elapsed >= DEFAULT_RECOVERY_QUARANTINE_SECS {
                        log_event("[STABLE] 3-minute stabilization quarantine completed without fluctuations! Initiating recovery.");
                        state = SentinelState::ClosedLoopWake;
                    } else {
                        log_event(&format!(
                            "[QUARANTINE] ac_online=1 stable={}/{}s",
                            next_elapsed, DEFAULT_RECOVERY_QUARANTINE_SECS
                        ));
                        state = SentinelState::StabilizingRecovery(next_elapsed);
                    }
                }
            }

            SentinelState::ClosedLoopWake => {
                trigger_parallel_wake();

                log_event("[WAKE] Closed-loop verification loop started. Polling nodes...");
                let mut kav_online = false;
                let mut psi_online = false;

                for attempt in 1..=12 {
                    thread::sleep(Duration::from_secs(10));

                    if !kav_online && is_node_online(KAVURE_LAN_IP, KAVURE_TS_IP) {
                        kav_online = true;
                        log_event("[SUCCESS] Kavure confirmed online!");
                    }
                    if !psi_online && is_node_online(PSICOPOMPO_LAN_IP, PSICOPOMPO_TS_IP) {
                        psi_online = true;
                        log_event("[SUCCESS] Psicopompo confirmed online!");
                    }

                    if kav_online && psi_online {
                        log_event("[RECOVERY] All homelab servers recovered and healthy. Returning to AcNormal state.");
                        state = SentinelState::AcNormal;
                        break;
                    }

                    // Re-fire WoL if one host is lagging after 40 seconds
                    if attempt % 4 == 0 {
                        log_event(&format!(
                            "[RE-WAKE] Attempt {}: Re-firing WoL for pending nodes (kavure={}, psicopompo={})...",
                            attempt, !kav_online, !psi_online
                        ));
                        if !kav_online {
                            let _ = send_http_post(&format!("{}/wake/kavure", WOL_LOCAL_URL), 2000);
                        }
                        if !psi_online {
                            let _ = send_http_post(&format!("{}/wake/psicopompo", WOL_LOCAL_URL), 2000);
                        }
                    }
                }

                if state != SentinelState::AcNormal {
                    log_event("[WARN] Closed-loop recovery timed out after 120s. Resuming AcNormal monitoring.");
                    state = SentinelState::AcNormal;
                }
            }
        }

        thread::sleep(Duration::from_secs(PROBE_INTERVAL_SECS));

        if last_push.elapsed() >= Duration::from_secs(NTFY_PUSH_INTERVAL_SECS) {
            push_state(&state, blackout_debit_secs);
            last_push = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_labels_are_stable() {
        assert_eq!(state_label(&SentinelState::AcNormal), "AcNormal");
        assert_eq!(
            state_label(&SentinelState::BlackoutPending(
                BlackoutReason::CriticalBattery
            )),
            "BlackoutPending"
        );
        assert_eq!(
            state_label(&SentinelState::StabilizingRecovery(30)),
            "StabilizingRecovery"
        );
    }

    #[test]
    fn live_holder_blocks_a_second_start() {
        let dir = std::env::temp_dir().join(format!("kururu-pid-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let pid_file = dir.join("sentinel.pid");
        let _ = fs::write(&pid_file, format!("{}\n", std::process::id()));

        assert!(
            !claim_single_instance(&pid_file),
            "a live pid already in the file must refuse the second start"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_pid_is_taken_over() {
        let dir = std::env::temp_dir().join(format!("kururu-pid-stale-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let pid_file = dir.join("sentinel.pid");
        // A pid that cannot exist: a crashed daemon must not block the next start forever.
        let _ = fs::write(&pid_file, "2147483647\n");

        assert!(
            claim_single_instance(&pid_file),
            "a stale pid file must be taken over"
        );
        let content = fs::read_to_string(&pid_file).unwrap_or_default();
        assert_eq!(
            content.trim(),
            std::process::id().to_string(),
            "the winner rewrites the pid file with its own pid"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_shifts_generations_and_keeps_the_history() {
        let dir = std::env::temp_dir().join(format!("kururu-log-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let log = dir.join("sentinel.log");
        let oversized = vec![b'x'; (LOG_MAX_BYTES + 1) as usize];
        let _ = fs::write(&log, &oversized);
        let _ = fs::write(dir.join("sentinel.log.1"), b"older generation");

        rotate_if_needed(&log);

        assert!(
            dir.join("sentinel.log.1").exists(),
            "the oversized file becomes generation .1"
        );
        assert!(
            dir.join("sentinel.log.2").exists(),
            "the previous .1 shifts to .2 — history is kept, not overwritten"
        );
        assert!(
            !log.exists(),
            "the file is renamed, never truncated in place"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
