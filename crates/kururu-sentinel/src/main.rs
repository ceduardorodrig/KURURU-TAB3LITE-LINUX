//! Kururu Power Sentinel
//!
//! Autonomous power outage detection, safe shutdown coordinator,
//! and automatic Wake-on-LAN recovery daemon for Mnemocine Homelab.
//!
//! Target: Samsung Galaxy Tab 3 Lite (SM-T110) running Alpine Linux 3.20 (ARMv7)

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// Defaults
const DEFAULT_BLACKOUT_TIMEOUT_SECS: u64 = 180;
const DEFAULT_RECOVERY_QUARANTINE_SECS: u64 = 180;
const PROBE_INTERVAL_SECS: u64 = 5;

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
const KAVURE_TS_IP: &str = "100.124.146.77";
const PSICOPOMPO_TS_IP: &str = "100.82.51.112";
const WOL_LOCAL_URL: &str = "http://127.0.0.1:9096";

// External WAN probe IPs
const WAN_PRIMARY_IP: &str = "1.1.1.1";
const WAN_BACKUP_IP: &str = "8.8.8.8";

// Log file locations (persistent on /data if available, fallback to /var/log)
const LOG_PATHS: [&str; 2] = [
    "/data/kururu-sentinel.log",
    "/var/log/kururu-sentinel.log",
];

/// Returns formatted timestamp "[HH:MM:SS]" in Brasília Time (UTC-3)
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
    println!("{}", line);

    for path in &LOG_PATHS {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{}", line);
            break;
        }
    }
}

/// Checks physical AC mains power status on Kururu hardware
/// Inspects Linux sysfs power_supply classes
fn is_ac_online() -> bool {
    let ps_dir = Path::new("/sys/class/power_supply");
    if !ps_dir.exists() {
        return true; // Default fallback if running outside hardware
    }

    // Check dedicated AC / mains supplies first
    if let Ok(entries) = fs::read_dir(ps_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            
            // Check online attribute if present
            let online_file = path.join("online");
            if online_file.exists() {
                if let Ok(val) = fs::read_to_string(&online_file) {
                    let v = val.trim();
                    if name.contains("ac") || name.contains("mains") || name.contains("sec-charger") || name.contains("usb") {
                        if v == "1" {
                            return true;
                        }
                    }
                }
            }

            // Check battery status
            let status_file = path.join("status");
            if status_file.exists() {
                if let Ok(val) = fs::read_to_string(&status_file) {
                    let v = val.trim();
                    if v == "Charging" || v == "Full" {
                        return true;
                    }
                }
            }
        }
    }

    false
}

/// Probes an IP address using ICMP ping with a small timeout
fn ping_check(ip: &str, timeout_secs: u64) -> bool {
    Command::new("ping")
        .args(["-c", "1", "-W", &timeout_secs.to_string(), ip])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
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

    let request = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        path, host_port
    );

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

/// Dispatches simultaneous shutdown commands to Kavure and Psicopompo
fn trigger_parallel_shutdown() {
    log_event("[ACTION: SHUTDOWN] Emitting parallel shutdown signals via Tailscale...");

    let h1 = thread::spawn(|| {
        let url = format!("http://{}:9096/power/shutdown", KAVURE_TS_IP);
        match send_http_post(&url, 3000) {
            Ok(_) => log_event("[SHUTDOWN] Kavure accepted shutdown request (HTTP 200)."),
            Err(e) => log_event(&format!("[SHUTDOWN] Failed to send shutdown to Kavure: {}", e)),
        }
    });

    let h2 = thread::spawn(|| {
        let url = format!("http://{}:9096/power/shutdown", PSICOPOMPO_TS_IP);
        match send_http_post(&url, 3000) {
            Ok(_) => log_event("[SHUTDOWN] Psicopompo accepted shutdown request (HTTP 200)."),
            Err(e) => log_event(&format!("[SHUTDOWN] Failed to send shutdown to Psicopompo: {}", e)),
        }
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
}

#[derive(Debug, PartialEq, Eq)]
enum SentinelState {
    AcNormal,
    PortableUsage,
    BlackoutPending(BlackoutReason),
    ExecutingShutdown,
    WaitingForPower,
    StabilizingRecovery(u64),
    ClosedLoopWake,
}

fn main() {
    log_event("=================================================================");
    log_event("Kururu Power Sentinel v1.1 (Autonomous Homelab Power Guardian)");
    log_event(&format!(
        "Config: Blackout threshold: {}s | Quarantine: {}s | Probes: {}s",
        DEFAULT_BLACKOUT_TIMEOUT_SECS, DEFAULT_RECOVERY_QUARANTINE_SECS, PROBE_INTERVAL_SECS
    ));
    log_event(&format!(
        "Physics: Dynamic Dock Baseline | Motion Threshold: {:.0} units",
        ORIENTATION_SHIFT_THRESHOLD
    ));
    log_event(&format!(
        "Targets: Kavure ({}) | Psicopompo ({})",
        KAVURE_TS_IP, PSICOPOMPO_TS_IP
    ));
    log_event("=================================================================");

    let mut state = SentinelState::AcNormal;
    let mut blackout_debit_secs: u64 = 0;
    let mut last_heartbeat = Instant::now();
    let mut dock_baseline: Option<Vector3D> = None;

    loop {
        let ac = is_ac_online();

        match state {
            SentinelState::AcNormal => {
                blackout_debit_secs = 0;

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

                    if delta >= ORIENTATION_SHIFT_THRESHOLD {
                        if wan {
                            log_event(&format!(
                                "[ASSESSMENT] Confidence: 10% | Physical pickup (delta={:.1} >= {:.1}) with active WAN -> PortableUsage.",
                                delta, ORIENTATION_SHIFT_THRESHOLD
                            ));
                            state = SentinelState::PortableUsage;
                        } else {
                            log_event(&format!(
                                "[ASSESSMENT] Confidence: 85% | Physical pickup (delta={:.1}) BUT WAN offline -> BlackoutPending(WanLostInPortable).",
                                delta
                            ));
                            state = SentinelState::BlackoutPending(BlackoutReason::WanLostInPortable);
                        }
                    } else {
                        // Static on dock without AC
                        if wan {
                            log_event(&format!(
                                "[ASSESSMENT] Confidence: 75% | Static on dock without AC (delta={:.1} < {:.1}, WAN active via UPS) -> BlackoutPending(StaticDockLostAc).",
                                delta, ORIENTATION_SHIFT_THRESHOLD
                            ));
                            state = SentinelState::BlackoutPending(BlackoutReason::StaticDockLostAc);
                        } else {
                            log_event(&format!(
                                "[ASSESSMENT] Confidence: 100% | Static on dock without AC AND WAN offline (delta={:.1}) -> BlackoutPending(StaticDockLostAc).",
                                delta
                            ));
                            state = SentinelState::BlackoutPending(BlackoutReason::StaticDockLostAc);
                        }
                    }
                } else if last_heartbeat.elapsed() >= Duration::from_secs(60) {
                    let kav_up = ping_check(KAVURE_TS_IP, 1);
                    let psi_up = ping_check(PSICOPOMPO_TS_IP, 1);
                    let base_str = match dock_baseline {
                        Some(b) => format!("dock=({:.0},{:.0},{:.0})", b.x, b.y, b.z),
                        None => "dock=calibrating".to_string(),
                    };
                    log_event(&format!(
                        "[HEARTBEAT] state=AcNormal ac_online=1 kavure_ts={} psicopompo_ts={} {} debit=0s",
                        if kav_up { "UP" } else { "DOWN" },
                        if psi_up { "UP" } else { "DOWN" },
                        base_str
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
                    let wan = is_wan_online();
                    if !wan {
                        log_event("[ASSESSMENT] Confidence: 85% | WAN connectivity lost during battery operation -> BlackoutPending(WanLostInPortable).");
                        state = SentinelState::BlackoutPending(BlackoutReason::WanLostInPortable);
                    }
                }
            }

            SentinelState::BlackoutPending(reason) => {
                if ac {
                    log_event("[EVENT] AC restored while in BlackoutPending. Draining debit.");
                    state = SentinelState::AcNormal;
                    blackout_debit_secs = 0;
                } else {
                    let curr_vec = read_accelerometer();
                    let delta = match (curr_vec, dock_baseline) {
                        (Some(c), Some(b)) => c.distance_to(&b),
                        _ => 0.0,
                    };
                    let wan = is_wan_online();

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
                    if !kav_down && !ping_check(KAVURE_TS_IP, 1) {
                        kav_down = true;
                        log_event("[CONFIRM] Kavure confirmed offline (S5).");
                    }
                    if !psi_down && !ping_check(PSICOPOMPO_TS_IP, 1) {
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

                    if !kav_online && ping_check(KAVURE_TS_IP, 1) {
                        kav_online = true;
                        log_event("[SUCCESS] Kavure confirmed online on Tailnet!");
                    }
                    if !psi_online && ping_check(PSICOPOMPO_TS_IP, 1) {
                        psi_online = true;
                        log_event("[SUCCESS] Psicopompo confirmed online on Tailnet!");
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
    }
}
