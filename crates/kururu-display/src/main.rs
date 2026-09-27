use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

mod font;

extern "C" {
    fn tzset();
}

const FB_PATH: &str = "/dev/graphics/fb0";
const EVENT_POWER: &str = "/dev/input/event2";
const EVENT_KEYS: &str = "/dev/input/event0";
const WOL_LOG_PATH: &str = "/var/log/kururu-wol.log";

const FB_WIDTH: usize = 1024;
const FB_HEIGHT: usize = 600;
const FB_STRIDE: usize = FB_WIDTH * 4;

const EV_KEY: u16 = 1;
const KEY_POWER: u16 = 116;
const KEY_VOLUMEUP: u16 = 115;
const KEY_VOLUMEDOWN: u16 = 114;
const KEY_HOMEPAGE: u16 = 102;

#[derive(Copy, Clone)]
struct Color {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

const BG_COLOR: Color = Color { r: 10, g: 14, b: 20, a: 255 };
const PANEL_BG: Color = Color { r: 16, g: 22, b: 32, a: 255 };
const PANEL_ACTIVE_BG: Color = Color { r: 24, g: 36, b: 54, a: 255 };
const BORDER_COLOR: Color = Color { r: 35, g: 48, b: 68, a: 255 };
const BORDER_ACTIVE: Color = Color { r: 0, g: 230, b: 118, a: 255 };

const TEXT_WHITE: Color = Color { r: 240, g: 244, b: 250, a: 255 };
const TEXT_GRAY: Color = Color { r: 140, g: 155, b: 175, a: 255 };
const TEXT_DIM: Color = Color { r: 80, g: 95, b: 115, a: 255 };
const TEXT_EMERALD: Color = Color { r: 0, g: 230, b: 118, a: 255 };
const TEXT_CYAN: Color = Color { r: 0, g: 210, b: 255, a: 255 };
const TEXT_AMBER: Color = Color { r: 255, g: 180, b: 0, a: 255 };
const TEXT_RED: Color = Color { r: 255, g: 82, b: 82, a: 255 };

#[repr(C)]
struct InputEvent {
    time_sec: usize,
    time_usec: usize,
    type_: u16,
    code: u16,
    value: i32,
}

struct Framebuffer {
    file: File,
    buffer: Vec<u8>,
}

impl Framebuffer {
    fn new(path: &str) -> std::io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let size = FB_STRIDE * FB_HEIGHT;
        let buffer = vec![0u8; size];
        Ok(Self { file, buffer })
    }

    fn clear(&mut self, color: Color) {
        for chunk in self.buffer.chunks_exact_mut(4) {
            chunk[0] = color.b;
            chunk[1] = color.g;
            chunk[2] = color.r;
            chunk[3] = color.a;
        }
    }

    fn draw_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: Color) {
        let max_x = (x + w).min(FB_WIDTH);
        let max_y = (y + h).min(FB_HEIGHT);

        for py in y..max_y {
            let row_offset = py * FB_STRIDE;
            for px in x..max_x {
                let offset = row_offset + px * 4;
                self.buffer[offset + 0] = color.b;
                self.buffer[offset + 1] = color.g;
                self.buffer[offset + 2] = color.r;
                self.buffer[offset + 3] = color.a;
            }
        }
    }

    fn draw_char(&mut self, x: usize, y: usize, ch: char, fg: Color, scale: usize) {
        let code = (ch as usize).min(127);
        let bitmap = &font::FONT_DATA[code];

        for row in 0..font::FONT_HEIGHT {
            let byte_val = bitmap[row];
            for col in 0..font::FONT_WIDTH {
                if (byte_val & (1 << (7 - col))) != 0 {
                    let px_base = x + col * scale;
                    let py_base = y + row * scale;
                    for sy in 0..scale {
                        let py = py_base + sy;
                        if py >= FB_HEIGHT {
                            continue;
                        }
                        let row_offset = py * FB_STRIDE;
                        for sx in 0..scale {
                            let px = px_base + sx;
                            if px >= FB_WIDTH {
                                continue;
                            }
                            let offset = row_offset + px * 4;
                            self.buffer[offset + 0] = fg.b;
                            self.buffer[offset + 1] = fg.g;
                            self.buffer[offset + 2] = fg.r;
                            self.buffer[offset + 3] = fg.a;
                        }
                    }
                }
            }
        }
    }

    fn draw_text(&mut self, x: usize, y: usize, text: &str, fg: Color, scale: usize) {
        let char_step = font::FONT_WIDTH * scale;
        let mut cur_x = x;
        let mut cur_y = y;

        for ch in text.chars() {
            if ch == '\n' {
                cur_x = x;
                cur_y += font::FONT_HEIGHT * scale;
                continue;
            }
            if cur_x + char_step > FB_WIDTH {
                cur_x = x;
                cur_y += font::FONT_HEIGHT * scale;
            }
            if cur_y + font::FONT_HEIGHT * scale > FB_HEIGHT {
                break;
            }
            self.draw_char(cur_x, cur_y, ch, fg, scale);
            cur_x += char_step;
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&self.buffer)?;
        self.file.flush()
    }
}

#[derive(serde::Deserialize, Default)]
struct TailscaleJson {
    #[serde(rename = "MagicDNSSuffix")]
    magic_dns_suffix: Option<String>,
    #[serde(rename = "CurrentTailnet")]
    current_tailnet: Option<CurrentTailnet>,
    #[serde(rename = "Self")]
    self_node: Option<SelfNode>,
    #[serde(rename = "Peer")]
    peer: Option<HashMap<String, PeerNode>>,
}

#[derive(serde::Deserialize, Default)]
struct CurrentTailnet {
    #[serde(rename = "MagicDNSSuffix")]
    magic_dns_suffix: Option<String>,
}

#[allow(dead_code)]
#[derive(serde::Deserialize, Default)]
struct SelfNode {
    #[serde(rename = "HostName")]
    hostname: Option<String>,
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Option<Vec<String>>,
}

#[allow(dead_code)]
#[derive(serde::Deserialize, Default, Clone)]
struct PeerNode {
    #[serde(rename = "HostName")]
    hostname: Option<String>,
    #[serde(rename = "Online")]
    online: Option<bool>,
    #[serde(rename = "Active")]
    active: Option<bool>,
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Option<Vec<String>>,
    #[serde(rename = "CurAddr")]
    cur_addr: Option<String>,
}

#[allow(dead_code)]
struct PeerDisplayInfo {
    name: String,
    ip: String,
    online: bool,
    active: bool,
    cur_addr: String,
}

#[allow(dead_code)]
struct SystemInfo {
    hostname: String,
    kernel_version: String,
    uptime_str: String,
    load_avg: String,
    cpu_freq_str: String,
    ram_used_mb: u64,
    ram_total_mb: u64,
    disk_used_mb: u64,
    disk_total_mb: u64,
    disk_free_mb: u64,
    battery_pct: String,
    battery_pct_num: u32,
    battery_status: String,
    battery_temp_c: String,
    battery_volts: String,
    lan_ip: String,
    wifi_ssid: String,
    wifi_signal_dbm: String,
    wifi_mac: String,
    tailscale_ip: String,
    tailnet_suffix: String,
    homelab_online_count: usize,
    peers_total_count: usize,
    homelab_nodes: Vec<PeerDisplayInfo>,
    active_link_str: String,
    logs: Vec<String>,
    wol_logs: Vec<String>,
    wol_daemon_running: bool,
    wol_target1_mac: String,
    wol_target2_mac: String,
    time_str: String,
    date_str: String,
    day_name: String,
    date_full_str: String,
}

fn check_wol_daemon() -> bool {
    TcpStream::connect("127.0.0.1:9096").is_ok()
}

/// Returns current local time as "[HH:MM:SS]" using the same TZ-aware libc
/// path as the retro clock, ensuring all log timestamps stay consistent.
fn local_time_str() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let tm = libc::localtime(&t);
        if !tm.is_null() {
            format!("[{:02}:{:02}:{:02}]", (*tm).tm_hour, (*tm).tm_min, (*tm).tm_sec)
        } else {
            "[--:--:--]".to_string()
        }
    }
}

fn gather_system_info() -> SystemInfo {
    let hostname = fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap_or_else(|_| "kururu".to_string())
        .trim()
        .to_string();

    let kernel_version = fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_else(|_| "3.4.5".to_string())
        .trim()
        .to_string();

    let uptime_str = if let Ok(u) = fs::read_to_string("/proc/uptime") {
        if let Some(first) = u.split_whitespace().next() {
            if let Ok(sec) = first.parse::<f64>() {
                let s = sec as u64;
                let days = s / 86400;
                let hours = (s % 86400) / 3600;
                let mins = (s % 3600) / 60;
                let secs = s % 60;
                if days > 0 {
                    format!("{}d {}h {}m {}s", days, hours, mins, secs)
                } else {
                    format!("{}h {}m {}s", hours, mins, secs)
                }
            } else {
                "Unknown".to_string()
            }
        } else {
            "Unknown".to_string()
        }
    } else {
        "Unknown".to_string()
    };

    let load_avg = if let Ok(l) = fs::read_to_string("/proc/loadavg") {
        let parts: Vec<&str> = l.split_whitespace().take(3).collect();
        parts.join(", ")
    } else {
        "Unknown".to_string()
    };

    let cpu_freq_str = if let Ok(f) = fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq") {
        if let Ok(khz) = f.trim().parse::<u64>() {
            format!("{:.2} GHz", khz as f64 / 1_000_000.0)
        } else {
            "1.20 GHz".to_string()
        }
    } else {
        "1.20 GHz".to_string()
    };

    let mut total_kb = 0u64;
    let mut free_kb = 0u64;
    let mut buffers_kb = 0u64;
    let mut cached_kb = 0u64;

    if let Ok(mem) = fs::read_to_string("/proc/meminfo") {
        for line in mem.lines() {
            let mut parts = line.split_whitespace();
            if let Some(key) = parts.next() {
                if let Some(val_str) = parts.next() {
                    let val = val_str.parse::<u64>().unwrap_or(0);
                    match key {
                        "MemTotal:" => total_kb = val,
                        "MemFree:" => free_kb = val,
                        "Buffers:" => buffers_kb = val,
                        "Cached:" => cached_kb = val,
                        _ => {}
                    }
                }
            }
        }
    }

    let ram_total_mb = total_kb / 1024;
    let available_kb = free_kb + buffers_kb + cached_kb;
    let used_kb = if total_kb > available_kb {
        total_kb - available_kb
    } else {
        0
    };
    let ram_used_mb = used_kb / 1024;

    let (disk_total_mb, disk_free_mb) = unsafe {
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        let path = std::ffi::CString::new("/").unwrap_or_default();
        if libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) == 0 {
            let s = stat.assume_init();
            let total = (s.f_blocks as u64 * s.f_frsize as u64) / (1024 * 1024);
            let free = (s.f_bavail as u64 * s.f_frsize as u64) / (1024 * 1024);
            (total, free)
        } else {
            (5120, 4400)
        }
    };
    let disk_used_mb = if disk_total_mb > disk_free_mb {
        disk_total_mb - disk_free_mb
    } else {
        0
    };

    let battery_pct_raw = fs::read_to_string("/sys/class/power_supply/battery/capacity")
        .unwrap_or_else(|_| "50".to_string());
    let battery_pct_num = battery_pct_raw.trim().parse::<u32>().unwrap_or(50);
    let battery_pct = battery_pct_num.to_string();

    let battery_status = fs::read_to_string("/sys/class/power_supply/battery/status")
        .unwrap_or_else(|_| "Unknown".to_string())
        .trim()
        .to_string();

    let battery_temp_c = if let Ok(t_raw) = fs::read_to_string("/sys/class/power_supply/battery/temp") {
        if let Ok(val) = t_raw.trim().parse::<f64>() {
            format!("{:.1}°C", val / 10.0)
        } else {
            "--°C".to_string()
        }
    } else {
        "--°C".to_string()
    };

    let battery_volts = if let Ok(v_raw) = fs::read_to_string("/sys/class/power_supply/battery/voltage_now") {
        if let Ok(val) = v_raw.trim().parse::<f64>() {
            format!("{:.2}V", val / 1_000_000.0)
        } else {
            "--V".to_string()
        }
    } else {
        "--V".to_string()
    };

    let mut lan_ip = "192.168.3.55".to_string();
    if let Ok(output) = Command::new("ip").args(["-4", "addr", "show"]).output() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut cur_iface = "";
        for line in stdout.lines() {
            let trimmed = line.trim();
            if line.starts_with(|c: char| c.is_ascii_digit()) {
                if let Some(name) = line.split(':').nth(1) {
                    cur_iface = name.trim();
                }
            } else if trimmed.starts_with("inet ") {
                if let Some(ip_cidr) = trimmed.split_whitespace().nth(1) {
                    let ip = ip_cidr.split('/').next().unwrap_or(ip_cidr);
                    if cur_iface.starts_with("mlan") || cur_iface.starts_with("wlan") {
                        lan_ip = ip.to_string();
                    }
                }
            }
        }
    }

    let mut wifi_ssid = "Cratos".to_string();
    let mut wifi_mac = "00:50:43:XX:XX:XX".to_string();
    if let Ok(output) = Command::new("wpa_cli").arg("status").output() {
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("ssid=") {
                wifi_ssid = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("address=") {
                wifi_mac = rest.trim().to_string();
            }
        }
    }

    let mut wifi_signal_dbm = "-35 dBm".to_string();
    if let Ok(content) = fs::read_to_string("/proc/net/wireless") {
        for line in content.lines() {
            if line.contains("mlan0:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 4 {
                    let lvl = parts[3].trim_end_matches('.');
                    wifi_signal_dbm = format!("{} dBm", lvl);
                }
            }
        }
    }

    let mut tailscale_ip = "100.127.188.45".to_string();
    let mut tailnet_suffix = "chimaera-heptatonic.ts.net".to_string();
    let mut homelab_nodes = Vec::new();
    let mut homelab_online_count = 0;
    let mut peers_total_count = 0;
    let mut active_link_str = "None (Idle)".to_string();

    if let Ok(output) = Command::new("tailscale").args(["status", "--json"]).output() {
        if let Ok(ts) = serde_json::from_slice::<TailscaleJson>(&output.stdout) {
            if let Some(suffix) = ts.magic_dns_suffix {
                if !suffix.is_empty() {
                    tailnet_suffix = suffix;
                }
            } else if let Some(ct) = ts.current_tailnet {
                if let Some(suffix) = ct.magic_dns_suffix {
                    if !suffix.is_empty() {
                        tailnet_suffix = suffix;
                    }
                }
            }

            if let Some(self_node) = ts.self_node {
                if let Some(ips) = self_node.tailscale_ips {
                    if let Some(first_ip) = ips.first() {
                        tailscale_ip = first_ip.clone();
                    }
                }
            }

            if let Some(peer_map) = ts.peer {
                peers_total_count = peer_map.len();
                let mut all_peers = Vec::new();

                for (_k, v) in peer_map {
                    let name = v.hostname.unwrap_or_else(|| "unknown".to_string());
                    let online = v.online.unwrap_or(false);
                    let active = v.active.unwrap_or(false);
                    let ip = v.tailscale_ips.and_then(|ips| ips.first().cloned()).unwrap_or_default();
                    let cur_addr = v.cur_addr.unwrap_or_default();

                    if active && !name.is_empty() {
                        active_link_str = if !cur_addr.is_empty() {
                            format!("{} ({})", name, cur_addr)
                        } else {
                            format!("{} (active)", name)
                        };
                    }

                    all_peers.push(PeerDisplayInfo {
                        name,
                        ip,
                        online,
                        active,
                        cur_addr,
                    });
                }

                const HOMELAB_NODES: [&str; 5] = [
                    "Psicopompo",
                    "Kuaray",
                    "Kavure",
                    "Ybytu",
                    "Ybyra",
                ];

                for &canonical in &HOMELAB_NODES {
                    let needle = canonical.to_lowercase();
                    let match_peer = all_peers.iter().find(|p| p.name.to_lowercase().contains(&needle));
                    if let Some(p) = match_peer {
                        if p.online {
                            homelab_online_count += 1;
                        }
                        homelab_nodes.push(PeerDisplayInfo {
                            name: canonical.to_string(),
                            ip: p.ip.clone(),
                            online: p.online,
                            active: p.active,
                            cur_addr: p.cur_addr.clone(),
                        });
                    } else {
                        homelab_nodes.push(PeerDisplayInfo {
                            name: canonical.to_string(),
                            ip: "---".to_string(),
                            online: false,
                            active: false,
                            cur_addr: "".to_string(),
                        });
                    }
                }
            }
        }
    }

    let ts = local_time_str();
    let mut logs = Vec::new();
    if let Ok(output) = Command::new("dmesg").output() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = stdout.lines().collect();
        let start = if lines.len() > 14 {
            lines.len() - 14
        } else {
            0
        };
        for line in &lines[start..] {
            // Prepend wall-clock timestamp so dmesg lines are clock-aligned
            logs.push(format!("{} {}", ts, line.trim_end()));
        }
    }

    let mut wol_logs = Vec::new();
    if let Ok(content) = fs::read_to_string(WOL_LOG_PATH) {
        let lines: Vec<&str> = content.lines().collect();
        let start = if lines.len() > 14 {
            lines.len() - 14
        } else {
            0
        };
        for line in &lines[start..] {
            wol_logs.push(line.trim_end().to_string());
        }
    }

    let day_names = [
        "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday",
    ];
    let month_names = [
        "January", "February", "March", "April", "May", "June", "July", "August",
        "September", "October", "November", "December",
    ];

    // Force Brazil / America/Sao_Paulo timezone
    std::env::set_var("TZ", "America/Sao_Paulo");
    unsafe {
        tzset();
    }

    let (time_str, date_str, day_name, date_full_str) = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let tm = libc::localtime(&t);
        if !tm.is_null() {
            let h = (*tm).tm_hour;
            let m = (*tm).tm_min;
            let s = (*tm).tm_sec;
            let day = (*tm).tm_mday;
            let mon = (*tm).tm_mon;
            let year = (*tm).tm_year + 1900;
            let wday = ((*tm).tm_wday as usize) % 7;
            let mon_idx = (mon as usize) % 12;

            let d_name = day_names[wday].to_string();
            let m_name = month_names[mon_idx];
            let full = format!("{} • {} {} {}", d_name.to_uppercase(), day, m_name.to_uppercase(), year);

            (
                format!("{:02}:{:02}:{:02}", h, m, s),
                format!("{:02}/{:02}/{}", day, mon + 1, year),
                d_name,
                full,
            )
        } else {
            (
                "00:00:00".to_string(),
                "01/01/1970".to_string(),
                "Monday".to_string(),
                "MONDAY • 1 JANUARY 1970".to_string(),
            )
        }
    };

    let (wol_target1_mac, wol_target2_mac) = {
        let mut psi = "d0:94:66:••:••:58".to_string();
        let mut kav = "d0:94:66:••:••:c4".to_string();
        if let Ok(content) = fs::read_to_string("/etc/kururu-wake.conf") {
            for line in content.lines() {
                let trimmed = line.trim();
                if let Some((k, v)) = trimmed.split_once('=') {
                    let k_clean = k.trim().to_lowercase();
                    let v_clean = v.trim().trim_matches('"').trim_matches('\'');
                    if k_clean == "psicopompo" {
                        psi = v_clean.to_string();
                    } else if k_clean == "kavure" {
                        kav = v_clean.to_string();
                    }
                }
            }
        }
        (psi, kav)
    };

    SystemInfo {
        hostname,
        kernel_version,
        uptime_str,
        load_avg,
        cpu_freq_str,
        ram_used_mb,
        ram_total_mb,
        disk_used_mb,
        disk_total_mb,
        disk_free_mb,
        battery_pct,
        battery_pct_num,
        battery_status,
        battery_temp_c,
        battery_volts,
        lan_ip,
        wifi_ssid,
        wifi_signal_dbm,
        wifi_mac,
        tailscale_ip,
        tailnet_suffix,
        homelab_online_count,
        peers_total_count,
        homelab_nodes,
        active_link_str,
        logs,
        wol_logs,
        wol_daemon_running: check_wol_daemon(),
        wol_target1_mac,
        wol_target2_mac,
        time_str,
        date_str,
        day_name,
        date_full_str,
    }
}

fn set_display_hardware(enable: bool) {
    let power_path = "/sys/class/graphics/fb0/blank";
    let val = if enable { "0\n" } else { "4\n" };
    let _ = fs::write(power_path, val);

    let bl_val = if enable { "180\n" } else { "0\n" };
    let backlight_paths = [
        "/sys/class/backlight/panel/brightness",
        "/sys/class/backlight/pwm-backlight/brightness",
    ];
    for path in &backlight_paths {
        let _ = fs::write(path, bl_val);
    }
}

fn draw_header(fb: &mut Framebuffer, active_tab: usize) {
    fb.draw_rect(0, 0, FB_WIDTH, 48, PANEL_BG);
    fb.draw_rect(0, 48, FB_WIDTH, 2, BORDER_COLOR);

    // Left Node Brand
    fb.draw_text(16, 14, "KURURU", TEXT_EMERALD, 2);

    // 3 Distinct Tabs
    let tabs = [
        "1. KURURU",
        "2. HOMELAB",
        "3. RETRO CLOCK",
    ];

    let mut tx = 160;
    for (i, &name) in tabs.iter().enumerate() {
        let is_current = i == active_tab;
        let tab_w = 150;
        let bg = if is_current { PANEL_ACTIVE_BG } else { PANEL_BG };
        let border = if is_current { BORDER_ACTIVE } else { BORDER_COLOR };
        let text_color = if is_current { TEXT_EMERALD } else { TEXT_GRAY };

        fb.draw_rect(tx, 8, tab_w, 32, bg);
        fb.draw_rect(tx, 8, tab_w, 2, border);
        fb.draw_rect(tx, 38, tab_w, 2, border);

        let pad_x = tx + 14;
        fb.draw_text(pad_x, 16, name, text_color, 1);

        tx += tab_w + 12;
    }

    fb.draw_text(
        660,
        18,
        "StenioSentinel • Mnemocine Homelab",
        TEXT_GRAY,
        1,
    );
}

fn draw_footer(fb: &mut Framebuffer) {
    fb.draw_rect(0, 568, FB_WIDTH, 32, PANEL_BG);
    fb.draw_rect(0, 568, FB_WIDTH, 1, BORDER_COLOR);
    fb.draw_text(
        16,
        576,
        "[VOL -] Next Tab (->)",
        TEXT_CYAN,
        1,
    );
    fb.draw_text(
        180,
        576,
        "[VOL +] Prev Tab (<-)",
        TEXT_AMBER,
        1,
    );
    fb.draw_text(
        360,
        576,
        "[POWER] Sleep / Wake Display",
        TEXT_WHITE,
        1,
    );
    fb.draw_text(
        640,
        576,
        "Auto-sleep: 120s timer",
        TEXT_DIM,
        1,
    );
    fb.draw_text(
        840,
        576,
        "UPS Battery: OK",
        TEXT_EMERALD,
        1,
    );
}

// -------------------------------------------------------------
// TAB 0: KURURU NODE (Exclusive Local Device Telemetry)
// -------------------------------------------------------------
fn render_tab_kururu(fb: &mut Framebuffer, info: &SystemInfo) {
    // Card 1: Top Left - System, APU & Memory
    fb.draw_rect(16, 56, 480, 244, PANEL_BG);
    fb.draw_rect(16, 56, 480, 2, BORDER_COLOR);
    fb.draw_text(28, 68, "KURURU ARCHITECTURE & COMPUTE", TEXT_AMBER, 1);

    let free_pct = if info.ram_total_mb > 0 {
        100 - (info.ram_used_mb * 100 / info.ram_total_mb)
    } else {
        0
    };

    let telemetry_lines = [
        ("Device Model:", "Samsung Tab 3 Lite (SM-T110)"),
        ("Linux Kernel:", info.kernel_version.as_str()),
        ("Distribution:", "Alpine Linux v3.20.3 (musl)"),
        ("APU Processor:", "Marvell PXA988 Dual Cortex-A9"),
        ("CPU Frequency:", info.cpu_freq_str.as_str()),
        ("System Load:", info.load_avg.as_str()),
        ("Node Uptime:", info.uptime_str.as_str()),
        (
            "RAM Memory:",
            &format!("{} MB used / {} MB ({}% free)", info.ram_used_mb, info.ram_total_mb, free_pct),
        ),
        (
            "Storage Rootfs:",
            &format!("{} MB free / {} MB total", info.disk_free_mb, info.disk_total_mb),
        ),
        ("Init Mode:", "Native Headless Bare-Metal"),
    ];

    let mut ty = 92;
    for (label, val) in &telemetry_lines {
        fb.draw_text(28, ty, label, TEXT_GRAY, 1);
        fb.draw_text(165, ty, val, TEXT_WHITE, 1);
        ty += 19;
    }

    // Card 2: Top Right - PMIC Battery & Local Wi-Fi Radio
    fb.draw_rect(512, 56, 496, 244, PANEL_BG);
    fb.draw_rect(512, 56, 496, 2, BORDER_COLOR);
    fb.draw_text(524, 68, "PMIC BATTERY (UPS) & LOCAL WI-FI RADIO", TEXT_AMBER, 1);

    // Battery bar inside Card 2
    fb.draw_text(524, 92, "Battery Level:", TEXT_GRAY, 1);
    fb.draw_rect(656, 90, 200, 16, BORDER_COLOR);
    let fill_w = (info.battery_pct_num as usize * 196) / 100;
    fb.draw_rect(658, 92, fill_w, 12, TEXT_EMERALD);
    let bat_label = format!("{}% ({})", info.battery_pct, info.battery_status);
    fb.draw_text(866, 92, &bat_label, TEXT_WHITE, 1);

    let power_radio_lines = [
        ("Fuelgauge:", format!("{} • Temp: {}", info.battery_volts, info.battery_temp_c)),
        ("Hardware PMIC:", "Marvell 88PM822 / AXP228 Driver".to_string()),
        ("UPS Protection:", "Active (3600 mAh Built-in Buffer)".to_string()),
        ("Wi-Fi Chipset:", "Marvell SD8777 (Interface: mlan0)".to_string()),
        ("Connected AP:", format!("{} (2.4 GHz BSSID)", info.wifi_ssid)),
        ("Signal Level:", format!("{} (Link Quality: 5/5)", info.wifi_signal_dbm)),
        ("Local LAN IP:", format!("{} (Port 22 Open)", info.lan_ip)),
        ("Hardware MAC:", info.wifi_mac.clone()),
        ("SSH Service:", "Dropbear (authorized keys root)".to_string()),
    ];

    let mut ry = 114;
    for (label, val) in &power_radio_lines {
        fb.draw_text(524, ry, label, TEXT_GRAY, 1);
        fb.draw_text(668, ry, val, TEXT_WHITE, 1);
        ry += 19;
    }

    // Card 3: Bottom - Live Kernel Log Console (dmesg tail)
    fb.draw_rect(16, 308, 992, 252, PANEL_BG);
    fb.draw_rect(16, 308, 992, 2, BORDER_COLOR);
    fb.draw_text(28, 318, "KURURU KERNEL LOG CONSOLE (DMESG TAIL)", TEXT_EMERALD, 1);

    let mut log_y = 338;
    for log in &info.logs {
        let truncated = if log.len() > 118 {
            &log[..118]
        } else {
            log.as_str()
        };
        fb.draw_text(28, log_y, truncated, TEXT_GRAY, 1);
        log_y += 17;
        if log_y > 546 {
            break;
        }
    }
}

// -------------------------------------------------------------
// TAB 1: HOMELAB & WOL (Cluster Telemetry & Wake-on-LAN)
// -------------------------------------------------------------
fn render_tab_homelab(fb: &mut Framebuffer, info: &SystemInfo) {
    // Card 1: Top Left - Tailnet Cluster & Core Servers
    fb.draw_rect(16, 56, 480, 244, PANEL_BG);
    fb.draw_rect(16, 56, 480, 2, BORDER_COLOR);

    let cluster_header = format!(
        "MNEMOCINE TAILNET CLUSTER ({}/5 Online)",
        info.homelab_online_count
    );
    fb.draw_text(28, 68, &cluster_header, TEXT_AMBER, 1);

    fb.draw_text(28, 90, "Tailnet:", TEXT_GRAY, 1);
    fb.draw_text(130, 90, &info.tailnet_suffix, TEXT_CYAN, 1);

    fb.draw_text(28, 108, "Kururu IP:", TEXT_GRAY, 1);
    fb.draw_text(130, 108, &info.tailscale_ip, TEXT_WHITE, 1);

    fb.draw_text(28, 126, "Direct Link:", TEXT_GRAY, 1);
    fb.draw_text(130, 126, &info.active_link_str, TEXT_EMERALD, 1);

    // Dynamic Server List (5 core servers)
    let mut py = 148;
    for peer in &info.homelab_nodes {
        let (bullet, color) = if peer.active {
            ("★", TEXT_EMERALD)
        } else if peer.online {
            ("●", TEXT_CYAN)
        } else {
            ("○", TEXT_DIM)
        };

        fb.draw_text(28, py, bullet, color, 1);
        fb.draw_text(44, py, &peer.name, if peer.online { TEXT_WHITE } else { TEXT_DIM }, 1);
        fb.draw_text(145, py, &peer.ip, TEXT_GRAY, 1);

        let status_desc = if peer.active {
            "direct"
        } else if peer.online {
            "online"
        } else {
            "offline"
        };
        fb.draw_text(295, py, status_desc, if peer.online { color } else { TEXT_DIM }, 1);

        py += 19;
    }

    let summary_line = format!("Mesh Total: {} nodes registered in Tailnet", info.peers_total_count);
    fb.draw_text(28, 276, &summary_line, TEXT_DIM, 1);

    // Card 2: Top Right - Wake-on-LAN Controller & Relay Targets
    fb.draw_rect(512, 56, 496, 244, PANEL_BG);
    fb.draw_rect(512, 56, 496, 2, BORDER_COLOR);
    fb.draw_text(524, 68, "WAKE-ON-LAN CONTROLLER & RELAY", TEXT_AMBER, 1);

    let daemon_label = if info.wol_daemon_running { "ACTIVE (:9096) — Kururu Native Rust" } else { "STOPPED" };
    let daemon_color = if info.wol_daemon_running { TEXT_EMERALD } else { TEXT_RED };

    let wol_lines = [
        ("Daemon Status:", daemon_label, daemon_color),
        ("Broadcast Target:", "192.168.3.255:9 (mlan0 direct AP)", TEXT_WHITE),
        ("Target 1 (Host):", "Psicopompo (Workstation & Gaming)", TEXT_WHITE),
        ("Target 1 (MAC):", info.wol_target1_mac.as_str(), TEXT_AMBER),
        ("Target 1 (URI):", "http://kururu:9096/wake/psicopompo", TEXT_CYAN),
        ("Target 2 (Host):", "Kavure (Services & Microserver)", TEXT_WHITE),
        ("Target 2 (MAC):", info.wol_target2_mac.as_str(), TEXT_AMBER),
        ("Target 2 (URI):", "http://kururu:9096/wake/kavure", TEXT_CYAN),
        ("Transmission:", "Layer 2 Magic Packet Burst (5x / 25ms)", TEXT_GRAY),
    ];

    let mut wy = 90;
    for (label, val, col) in &wol_lines {
        fb.draw_text(524, wy, label, TEXT_GRAY, 1);
        fb.draw_text(678, wy, val, *col, 1);
        wy += 19;
    }

    // Card 3: Bottom - Wake-on-LAN Dispatch Audit Log
    fb.draw_rect(16, 308, 992, 252, PANEL_BG);
    fb.draw_rect(16, 308, 992, 2, BORDER_COLOR);
    fb.draw_text(28, 318, "RECENT WAKE-ON-LAN DISPATCH AUDIT LOG (/var/log/kururu-wol.log)", TEXT_EMERALD, 1);

    let mut log_y = 338;
    if info.wol_logs.is_empty() {
        fb.draw_text(28, log_y, "No Wake-on-LAN packets dispatched yet. Waiting for triggers on port 9096...", TEXT_DIM, 1);
    } else {
        for log in &info.wol_logs {
            let truncated = if log.len() > 118 {
                &log[..118]
            } else {
                log.as_str()
            };
            fb.draw_text(28, log_y, truncated, TEXT_CYAN, 1);
            log_y += 17;
            if log_y > 546 {
                break;
            }
        }
    }
}

// -------------------------------------------------------------
// TAB 2: RETRO DESK CLOCK & AMBIENT STATION
// -------------------------------------------------------------
fn render_tab_clock(fb: &mut Framebuffer, info: &SystemInfo) {
    // Massive Centered Digital Clock Card (Clean & Uncluttered)
    fb.draw_rect(16, 56, 992, 294, PANEL_BG);
    fb.draw_rect(16, 56, 992, 2, BORDER_COLOR);

    fb.draw_text(32, 70, "MNEMOCINE TIME STATION • SOVEREIGN NTP CLOCK", TEXT_AMBER, 1);

    // Render huge 5x clock (Font 8x16 scaled 5x = 40x80 px per char)
    // 8 chars * 40 px = 320 px width -> Center at x = (1024 - 320) / 2 = 352
    fb.draw_text(352, 96, &info.time_str, TEXT_EMERALD, 5);

    // Date banner (Scale 2, centered)
    let date_char_w = 8 * 2;
    let date_width = info.date_full_str.len() * date_char_w;
    let date_x = (FB_WIDTH.saturating_sub(date_width)) / 2;
    fb.draw_text(date_x, 196, &info.date_full_str, TEXT_CYAN, 2);

    // Minimal Sub-banner
    let sub = format!("Timezone: America/Sao_Paulo (UTC-3) • Host: {} • Uptime: {}", info.hostname, info.uptime_str);
    let sub_x = (FB_WIDTH.saturating_sub(sub.len() * 8)) / 2;
    fb.draw_text(sub_x, 244, &sub, TEXT_GRAY, 1);

    // Status Dots
    let cluster_pill = format!(
        "Homelab: {}/5 Servers Online   •   Wi-Fi: {} ({})   •   WOL Relay: Port 9096 Ready",
        info.homelab_online_count, info.wifi_ssid, info.wifi_signal_dbm
    );
    let dots_x = (FB_WIDTH.saturating_sub(cluster_pill.len() * 8)) / 2;
    fb.draw_text(dots_x, 272, &cluster_pill, TEXT_DIM, 1);

    // Bottom Ambient Ribbon: 3 Clean Symmetrical Vitals Cards (Each 316px wide)
    // Card 1: Hardware UPS & Power (Left)
    fb.draw_rect(16, 358, 316, 202, PANEL_BG);
    fb.draw_rect(16, 358, 316, 2, BORDER_COLOR);
    fb.draw_text(28, 370, "HARDWARE UPS / NO-BREAK", TEXT_AMBER, 1);

    fb.draw_rect(28, 396, 292, 16, BORDER_COLOR);
    let fill_w = (info.battery_pct_num as usize * 288) / 100;
    fb.draw_rect(30, 398, fill_w, 12, TEXT_EMERALD);

    let ups_lines = [
        ("Charge:", format!("{}% ({})", info.battery_pct, info.battery_status)),
        ("Voltage:", info.battery_volts.clone()),
        ("Thermal:", info.battery_temp_c.clone()),
        ("AC Supply:", "5V Micro-USB Continuous".to_string()),
        ("Buffer:", "3600 mAh Li-ion Cell".to_string()),
    ];

    let mut uy = 422;
    for (label, val) in &ups_lines {
        fb.draw_text(28, uy, label, TEXT_GRAY, 1);
        fb.draw_text(115, uy, val, TEXT_WHITE, 1);
        uy += 22;
    }

    // Card 2: Kururu Node Vitals (Center)
    fb.draw_rect(348, 358, 328, 202, PANEL_BG);
    fb.draw_rect(348, 358, 328, 2, BORDER_COLOR);
    fb.draw_text(360, 370, "KURURU NODE VITALS", TEXT_AMBER, 1);

    let vitals_lines = [
        ("Device:", "Samsung SM-T110 (Goyawifi)".to_string()),
        ("APU:", format!("Marvell PXA988 @ {}", info.cpu_freq_str)),
        ("Load:", format!("{} (1m, 5m, 15m)", info.load_avg)),
        ("RAM:", format!("{} MB used / {} MB", info.ram_used_mb, info.ram_total_mb)),
        ("Storage:", format!("{} MB free in rootfs", info.disk_free_mb)),
        ("Cooling:", "Passive Thermal (Silent 0 dB)".to_string()),
    ];

    let mut vy = 398;
    for (label, val) in &vitals_lines {
        fb.draw_text(360, vy, label, TEXT_GRAY, 1);
        fb.draw_text(440, vy, val, TEXT_WHITE, 1);
        vy += 22;
    }

    // Card 3: Homelab Network (Right)
    fb.draw_rect(692, 358, 316, 202, PANEL_BG);
    fb.draw_rect(692, 358, 316, 2, BORDER_COLOR);
    fb.draw_text(704, 370, "HOMELAB NETWORK", TEXT_AMBER, 1);

    let net_lines = [
        ("Tailnet:", info.tailnet_suffix.clone()),
        ("Node IP:", info.tailscale_ip.clone()),
        ("Servers:", format!("{}/5 Nodes Active", info.homelab_online_count)),
        ("Wi-Fi AP:", info.wifi_ssid.clone()),
        ("Signal:", info.wifi_signal_dbm.clone()),
        ("WOL Engine:", "Port 9096 Listener Ready".to_string()),
    ];

    let mut ny = 398;
    for (label, val) in &net_lines {
        fb.draw_text(704, ny, label, TEXT_GRAY, 1);
        fb.draw_text(800, ny, val, TEXT_WHITE, 1);
        ny += 22;
    }
}

fn main() {
    println!("[Kururu Display Daemon] Starting v1.3 (Inverted Volume Navigation & Modular Dashboards)...");

    // Force Brazil / America/Sao_Paulo timezone across entire process
    std::env::set_var("TZ", "America/Sao_Paulo");
    unsafe {
        tzset();
    }

    let screen_active = Arc::new(AtomicBool::new(true));
    let screen_active_power = screen_active.clone();
    let screen_active_keys = screen_active.clone();

    let current_tab = Arc::new(AtomicUsize::new(0));
    let current_tab_keys = current_tab.clone();

    let wake_signal = Arc::new(AtomicBool::new(true));
    let wake_signal_power = wake_signal.clone();
    let wake_signal_keys = wake_signal.clone();

    // Start with display ON initially so user sees it right away
    set_display_hardware(true);

    // Thread 1: Listen to hardware Power button (/dev/input/event2)
    thread::spawn(move || {
        let mut file = match File::open(EVENT_POWER) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Failed to open {}: {}", EVENT_POWER, e);
                return;
            }
        };

        let event_size = std::mem::size_of::<InputEvent>();
        let mut buf = vec![0u8; event_size];

        loop {
            if file.read_exact(&mut buf).is_err() {
                thread::sleep(Duration::from_millis(200));
                continue;
            }

            let event = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const InputEvent) };

            // KEY_POWER press (value == 1)
            if event.type_ == EV_KEY && event.code == KEY_POWER && event.value == 1 {
                let current = screen_active_power.load(Ordering::SeqCst);
                let new_state = !current;
                println!(
                    "[Kururu Display] Power button toggled -> State: {}",
                    if new_state { "AWAKE" } else { "SLEEP" }
                );
                set_display_hardware(new_state);
                screen_active_power.store(new_state, Ordering::SeqCst);
                if new_state {
                    wake_signal_power.store(true, Ordering::SeqCst);
                }
            }
        }
    });

    // Thread 2: Listen to hardware Volume Up / Volume Down keys (/dev/input/event0)
    // INVERTED: Volume Up goes right-to-left (prev tab); Volume Down goes left-to-right (next tab)
    thread::spawn(move || {
        let mut file = match File::open(EVENT_KEYS) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Failed to open {}: {}", EVENT_KEYS, e);
                return;
            }
        };

        let event_size = std::mem::size_of::<InputEvent>();
        let mut buf = vec![0u8; event_size];

        loop {
            if file.read_exact(&mut buf).is_err() {
                thread::sleep(Duration::from_millis(200));
                continue;
            }

            let event = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const InputEvent) };

            // Only respond on key down (value == 1)
            if event.type_ == EV_KEY && event.value == 1 {
                let was_asleep = !screen_active_keys.load(Ordering::SeqCst);
                if was_asleep {
                    println!("[Kururu Display] Key pressed while asleep -> Waking display up");
                    set_display_hardware(true);
                    screen_active_keys.store(true, Ordering::SeqCst);
                    wake_signal_keys.store(true, Ordering::SeqCst);
                }

                if event.code == KEY_VOLUMEUP {
                    // Volume Up: right-to-left (previous tab: 2 -> 1 -> 0 -> 2)
                    let prev = (current_tab_keys.load(Ordering::SeqCst) + 3 - 1) % 3;
                    current_tab_keys.store(prev, Ordering::SeqCst);
                    println!("[Kururu Display] Volume UP -> Switched to Tab {} (<-)", prev + 1);
                    wake_signal_keys.store(true, Ordering::SeqCst);
                } else if event.code == KEY_VOLUMEDOWN {
                    // Volume Down: left-to-right (next tab: 0 -> 1 -> 2 -> 0)
                    let next = (current_tab_keys.load(Ordering::SeqCst) + 1) % 3;
                    current_tab_keys.store(next, Ordering::SeqCst);
                    println!("[Kururu Display] Volume DOWN -> Switched to Tab {} (->)", next + 1);
                    wake_signal_keys.store(true, Ordering::SeqCst);
                } else if event.code == KEY_HOMEPAGE {
                    println!("[Kururu Display] Home key pressed -> Wake/Refreshed");
                    wake_signal_keys.store(true, Ordering::SeqCst);
                }
            }
        }
    });

    let mut fb = match Framebuffer::new(FB_PATH) {
        Ok(fb) => fb,
        Err(e) => {
            eprintln!("Failed to open framebuffer {}: {}", FB_PATH, e);
            std::process::exit(1);
        }
    };

    let mut last_awake_time = Instant::now();
    let mut was_active = false;

    // Cached system info — refreshed every REFRESH_INTERVAL, rendered on every wake
    let mut cached_info: Option<SystemInfo> = None;
    let mut last_refresh = Instant::now()
        .checked_sub(Duration::from_secs(10))
        .unwrap_or_else(Instant::now);

    const TICK_MS: u64 = 50;          // Poll interval: 50ms max button latency
    const REFRESH_SECS: u64 = 2;      // Full telemetry refresh cadence

    loop {
        let is_active = screen_active.load(Ordering::SeqCst);

        if is_active {
            let got_wake = wake_signal.swap(false, Ordering::SeqCst);

            if !was_active {
                // Just woke up — reset activity timer and force immediate refresh
                last_awake_time = Instant::now();
                last_refresh = last_awake_time
                    .checked_sub(Duration::from_secs(10))
                    .unwrap_or(last_awake_time);
                was_active = true;
            }

            if got_wake {
                // Button pressed — reset inactivity timer
                last_awake_time = Instant::now();
            }

            // Check auto-sleep timeout (120 seconds of inactivity)
            if last_awake_time.elapsed() > Duration::from_secs(120) {
                println!("[Kururu Display] Inactivity timeout (120s) -> Sleeping display");
                set_display_hardware(false);
                screen_active.store(false, Ordering::SeqCst);
                was_active = false;
                cached_info = None;
                thread::sleep(Duration::from_millis(200));
                continue;
            }

            // Refresh telemetry every REFRESH_SECS, or immediately on first paint
            let needs_refresh = cached_info.is_none()
                || last_refresh.elapsed() >= Duration::from_secs(REFRESH_SECS);

            // Render immediately on button press even without a full refresh,
            // so the tab switch feels instant (<50ms).
            let should_render = got_wake || needs_refresh;

            if needs_refresh {
                cached_info = Some(gather_system_info());
                last_refresh = Instant::now();
            }

            if should_render {
                if let Some(ref info) = cached_info {
                    let tab = current_tab.load(Ordering::SeqCst);

                    fb.clear(BG_COLOR);
                    draw_header(&mut fb, tab);

                    match tab {
                        0 => render_tab_kururu(&mut fb, info),
                        1 => render_tab_homelab(&mut fb, info),
                        _ => render_tab_clock(&mut fb, info),
                    }

                    draw_footer(&mut fb);
                    let _ = fb.flush();
                }
            }

            thread::sleep(Duration::from_millis(TICK_MS));
        } else {
            was_active = false;
            cached_info = None;
            // Sleep quietly while screen is off, checking state every 200ms
            thread::sleep(Duration::from_millis(200));
        }
    }
}
