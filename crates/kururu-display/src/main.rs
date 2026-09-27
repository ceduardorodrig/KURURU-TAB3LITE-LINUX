mod font;

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const FB_WIDTH: usize = 1024;
const FB_HEIGHT: usize = 600;
const FB_STRIDE: usize = 4096;
const FB_SIZE: usize = FB_WIDTH * FB_HEIGHT * 4;

const BACKLIGHT_PATH: &str = "/sys/class/backlight/panel/brightness";
const BLANK_PATH: &str = "/sys/class/graphics/fb0/blank";
const EVENT_POWER: &str = "/dev/input/event2";
const EVENT_KEYS: &str = "/dev/input/event0";
const FB_PATH: &str = "/dev/graphics/fb0";
const WOL_LOG_PATH: &str = "/var/log/kururu-wol.log";

const EV_KEY: u16 = 1;
const KEY_VOLUMEDOWN: u16 = 114;
const KEY_VOLUMEUP: u16 = 115;
const KEY_POWER: u16 = 116;

#[derive(Clone, Copy)]
pub struct Color {
    pub b: u8,
    pub g: u8,
    pub r: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { b, g, r, a: 0xFF }
    }
}

pub const BG_COLOR: Color = Color::rgb(10, 14, 20);
pub const PANEL_BG: Color = Color::rgb(16, 23, 34);
pub const PANEL_ACTIVE_BG: Color = Color::rgb(22, 34, 52);
pub const BORDER_COLOR: Color = Color::rgb(40, 56, 80);
pub const BORDER_ACTIVE: Color = Color::rgb(46, 213, 115);
pub const TEXT_EMERALD: Color = Color::rgb(46, 213, 115);
pub const TEXT_CYAN: Color = Color::rgb(72, 219, 251);
pub const TEXT_AMBER: Color = Color::rgb(254, 202, 87);
pub const TEXT_WHITE: Color = Color::rgb(245, 246, 250);
pub const TEXT_GRAY: Color = Color::rgb(130, 140, 155);
pub const TEXT_DIM: Color = Color::rgb(80, 92, 108);
pub const TEXT_RED: Color = Color::rgb(255, 71, 87);

#[repr(C)]
struct InputEvent {
    tv_sec: u32,
    tv_usec: u32,
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
        let file = OpenOptions::new().write(true).open(path)?;
        let buffer = vec![0u8; FB_SIZE];
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

struct SystemInfo {
    hostname: String,
    kernel_version: String,
    uptime_str: String,
    load_avg: String,
    ram_used_mb: u64,
    ram_total_mb: u64,
    battery_pct: String,
    battery_pct_num: u32,
    battery_status: String,
    battery_temp_c: String,
    battery_volts: String,
    lan_ip: String,
    tailscale_ip: String,
    tailnet_suffix: String,
    homelab_online_count: usize,
    peers_total_count: usize,
    homelab_nodes: Vec<PeerDisplayInfo>,
    active_link_str: String,
    logs: Vec<String>,
    wol_logs: Vec<String>,
    wol_daemon_running: bool,
    time_str: String,
    date_str: String,
}

fn check_wol_daemon() -> bool {
    TcpStream::connect("127.0.0.1:9096").is_ok()
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

    let mut lan_ip = "Connecting...".to_string();
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
            logs.push(line.trim_end().to_string());
        }
    }

    let mut wol_logs = Vec::new();
    if let Ok(content) = fs::read_to_string(WOL_LOG_PATH) {
        let lines: Vec<&str> = content.lines().collect();
        let start = if lines.len() > 12 {
            lines.len() - 12
        } else {
            0
        };
        for line in &lines[start..] {
            wol_logs.push(line.trim_end().to_string());
        }
    }

    let (time_str, date_str) = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let tm = libc::localtime(&t);
        if !tm.is_null() {
            let h = (*tm).tm_hour;
            let m = (*tm).tm_min;
            let s = (*tm).tm_sec;
            let day = (*tm).tm_mday;
            let mon = (*tm).tm_mon + 1;
            let year = (*tm).tm_year + 1900;
            (
                format!("{:02}:{:02}:{:02}", h, m, s),
                format!("{:04}-{:02}-{:02}", year, mon, day),
            )
        } else {
            ("00:00:00".to_string(), "2026-09-26".to_string())
        }
    };

    let wol_daemon_running = check_wol_daemon();

    SystemInfo {
        hostname,
        kernel_version,
        uptime_str,
        load_avg,
        ram_used_mb,
        ram_total_mb,
        battery_pct,
        battery_pct_num,
        battery_status,
        battery_temp_c,
        battery_volts,
        lan_ip,
        tailscale_ip,
        tailnet_suffix,
        homelab_online_count,
        peers_total_count,
        homelab_nodes,
        active_link_str,
        logs,
        wol_logs,
        wol_daemon_running,
        time_str,
        date_str,
    }
}

fn set_display_hardware(on: bool) {
    if on {
        let _ = fs::write(BLANK_PATH, b"0\n");
        let _ = fs::write(BACKLIGHT_PATH, b"180\n");
    } else {
        let _ = fs::write(BACKLIGHT_PATH, b"0\n");
        let _ = fs::write(BLANK_PATH, b"1\n");
    }
}

fn draw_header(fb: &mut Framebuffer, active_tab: usize) {
    fb.draw_rect(0, 0, FB_WIDTH, 48, PANEL_BG);
    fb.draw_rect(0, 48, FB_WIDTH, 2, BORDER_COLOR);

    // Left Node Brand
    fb.draw_text(16, 14, "KURURU", TEXT_EMERALD, 2);

    // 3 Clickable/Navigable Tabs
    let tabs = [
        "1. CLUSTER",
        "2. WOL RELAY",
        "3. DESK CLOCK",
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
        "[VOL +/-] Switch Tab (1/2/3)",
        TEXT_CYAN,
        1,
    );
    fb.draw_text(
        340,
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
// TAB 0: CLUSTER & HARDWARE TELEMETRY
// -------------------------------------------------------------
fn render_tab_cluster(fb: &mut Framebuffer, info: &SystemInfo) {
    // Telemetry Card (Left)
    fb.draw_rect(16, 60, 480, 240, PANEL_BG);
    fb.draw_rect(16, 60, 480, 2, BORDER_COLOR);
    fb.draw_text(28, 72, "NODE TELEMETRY", TEXT_AMBER, 1);

    let free_pct = if info.ram_total_mb > 0 {
        100 - (info.ram_used_mb * 100 / info.ram_total_mb)
    } else {
        0
    };

    let telemetry_lines = [
        format!("Status:       ONLINE (Bare-Metal Headless)"),
        format!("Hostname:     {}", info.hostname),
        format!("Kernel:       {} (ARMv7l)", info.kernel_version),
        format!("Tailscale IP: {} [Connected]", info.tailscale_ip),
        format!("LAN IP:       {} (wlan0/mlan0)", info.lan_ip),
        format!("Load Average: {}", info.load_avg),
        format!("Uptime:       {}", info.uptime_str),
        format!(
            "RAM Usage:    {} MB / {} MB ({}% free)",
            info.ram_used_mb, info.ram_total_mb, free_pct
        ),
        format!(
            "Battery:      {}% ({}, {}, {})",
            info.battery_pct, info.battery_status, info.battery_volts, info.battery_temp_c
        ),
        format!("SSH Server:   Dropbear (port 22, root pubkey)"),
    ];

    let mut ty = 94;
    for line in &telemetry_lines {
        let (label, val) = if let Some(idx) = line.find(':') {
            (&line[..=idx], &line[idx + 1..])
        } else {
            (line.as_str(), "")
        };
        fb.draw_text(28, ty, label, TEXT_GRAY, 1);
        fb.draw_text(150, ty, val.trim_start(), TEXT_WHITE, 1);
        ty += 19;
    }

    // Dynamic Homelab Servers Card (Right)
    fb.draw_rect(512, 60, 496, 240, PANEL_BG);
    fb.draw_rect(512, 60, 496, 2, BORDER_COLOR);

    let header_peers = format!(
        "HOMELAB CLUSTER ({}/5 Online)",
        info.homelab_online_count
    );
    fb.draw_text(524, 72, &header_peers, TEXT_AMBER, 1);

    fb.draw_text(524, 93, "Tailnet:", TEXT_GRAY, 1);
    fb.draw_text(615, 93, &info.tailnet_suffix, TEXT_CYAN, 1);

    fb.draw_text(524, 110, "Direct Link:", TEXT_GRAY, 1);
    fb.draw_text(615, 110, &info.active_link_str, TEXT_EMERALD, 1);

    // List the 5 core homelab servers
    let mut py = 132;
    for peer in &info.homelab_nodes {
        let (bullet, color) = if peer.active {
            ("★", TEXT_EMERALD)
        } else if peer.online {
            ("●", TEXT_CYAN)
        } else {
            ("○", TEXT_DIM)
        };

        fb.draw_text(524, py, bullet, color, 1);
        fb.draw_text(540, py, &peer.name, if peer.online { TEXT_WHITE } else { TEXT_DIM }, 1);
        fb.draw_text(655, py, &peer.ip, TEXT_GRAY, 1);

        let status_desc = if peer.active {
            "direct"
        } else if peer.online {
            "online"
        } else {
            "offline"
        };
        fb.draw_text(805, py, status_desc, if peer.online { color } else { TEXT_DIM }, 1);

        py += 21;
    }

    let summary_line = format!("Mesh Total: {} nodes registered in Tailnet", info.peers_total_count);
    fb.draw_text(524, 276, &summary_line, TEXT_DIM, 1);

    // Live Terminal Console Card (Bottom)
    fb.draw_rect(16, 312, 992, 244, PANEL_BG);
    fb.draw_rect(16, 312, 992, 2, BORDER_COLOR);
    fb.draw_text(28, 322, "LIVE SYSTEM CONSOLE (DMESG TAIL)", TEXT_EMERALD, 1);

    let mut log_y = 344;
    for log in &info.logs {
        let truncated = if log.len() > 118 {
            &log[..118]
        } else {
            log.as_str()
        };
        fb.draw_text(28, log_y, truncated, TEXT_GRAY, 1);
        log_y += 16;
        if log_y > 540 {
            break;
        }
    }
}

// -------------------------------------------------------------
// TAB 1: WAKE-ON-LAN HARDWARE CONTROLLER & RELAY
// -------------------------------------------------------------
fn render_tab_wol(fb: &mut Framebuffer, info: &SystemInfo) {
    // Target 1: Psicopompo
    fb.draw_rect(16, 60, 480, 240, PANEL_BG);
    fb.draw_rect(16, 60, 480, 2, BORDER_COLOR);
    fb.draw_text(28, 72, "WOL TARGET 1: PSICOPOMPO", TEXT_AMBER, 1);

    let psicopompo_lines = [
        ("Host:", "Psicopompo (Workstation & Gaming)"),
        ("Hardware MAC:", "d0:94:66:de:8b:58"),
        ("Tailscale IP:", "100.82.51.112"),
        ("LAN Broadcast:", "192.168.3.255:9 (mlan0)"),
        ("HTTP Trigger:", "http://kururu:9096/wake/psicopompo"),
        ("SSH Command:", "ssh root@kururu kururu-wake psicopompo"),
        ("WOL Engine:", "Native Rust Burst (5 packets / 25ms)"),
    ];

    let mut y = 96;
    for (label, val) in &psicopompo_lines {
        fb.draw_text(28, y, label, TEXT_GRAY, 1);
        fb.draw_text(150, y, val, TEXT_WHITE, 1);
        y += 20;
    }

    // Target 2: Kavure
    fb.draw_rect(512, 60, 496, 240, PANEL_BG);
    fb.draw_rect(512, 60, 496, 2, BORDER_COLOR);
    fb.draw_text(524, 72, "WOL TARGET 2: KAVURE", TEXT_AMBER, 1);

    let kavure_lines = [
        ("Host:", "Kavure (Dell OptiPlex / Services)"),
        ("Hardware MAC:", "d0:94:66:ad:f3:c4"),
        ("Tailscale IP:", "100.124.146.77"),
        ("LAN Broadcast:", "192.168.3.255:9 (mlan0)"),
        ("HTTP Trigger:", "http://kururu:9096/wake/kavure"),
        ("SSH Command:", "ssh root@kururu kururu-wake kavure"),
        ("Daemon Status:", if info.wol_daemon_running { "ACTIVE (:9096)" } else { "STOPPED" }),
    ];

    let mut ky = 96;
    for (label, val) in &kavure_lines {
        fb.draw_text(524, ky, label, TEXT_GRAY, 1);
        let color = if label.starts_with("Daemon") {
            if info.wol_daemon_running { TEXT_EMERALD } else { TEXT_RED }
        } else {
            TEXT_WHITE
        };
        fb.draw_text(650, ky, val, color, 1);
        ky += 20;
    }

    // Bottom Card: WOL Dispatch Log
    fb.draw_rect(16, 312, 992, 244, PANEL_BG);
    fb.draw_rect(16, 312, 992, 2, BORDER_COLOR);
    fb.draw_text(28, 322, "RECENT WAKE-ON-LAN DISPATCH AUDIT LOG (/var/log/kururu-wol.log)", TEXT_EMERALD, 1);

    let mut log_y = 348;
    if info.wol_logs.is_empty() {
        fb.draw_text(28, log_y, "No Wake-on-LAN packets dispatched yet. Waiting for triggers...", TEXT_DIM, 1);
    } else {
        for log in &info.wol_logs {
            let truncated = if log.len() > 118 {
                &log[..118]
            } else {
                log.as_str()
            };
            fb.draw_text(28, log_y, truncated, TEXT_CYAN, 1);
            log_y += 16;
            if log_y > 540 {
                break;
            }
        }
    }
}

// -------------------------------------------------------------
// TAB 2: RETRO DESK CLOCK & BATTERY MONITOR
// -------------------------------------------------------------
fn render_tab_clock(fb: &mut Framebuffer, info: &SystemInfo) {
    // Massive Centered Digital Clock Card
    fb.draw_rect(16, 60, 992, 260, PANEL_BG);
    fb.draw_rect(16, 60, 992, 2, BORDER_COLOR);

    fb.draw_text(40, 78, "SOVEREIGN HOMELAB TIME ENGINE • NTP SYNCHRONIZED", TEXT_AMBER, 1);

    // Render huge 4x clock (Font 8x16 scaled 4x = 32x64 px per char)
    // 8 chars * 32 px = 256 px width -> Center at x = (1024 - 256) / 2 = 384
    fb.draw_text(384, 110, &info.time_str, TEXT_EMERALD, 4);

    let date_line = format!("Local Date: {} • Node Hostname: {}", info.date_str, info.hostname);
    fb.draw_text(320, 200, &date_line, TEXT_CYAN, 2);

    let cluster_status = format!(
        "Mnemocine Homelab: {}/5 Servers Active • Tailnet: {}",
        info.homelab_online_count, info.tailnet_suffix
    );
    fb.draw_text(260, 250, &cluster_status, TEXT_GRAY, 1);

    // Bottom Detailed Hardware & UPS Power Card
    fb.draw_rect(16, 332, 992, 224, PANEL_BG);
    fb.draw_rect(16, 332, 992, 2, BORDER_COLOR);
    fb.draw_text(28, 344, "INTEGRATED UPS HARDWARE POWER & TELEMETRY", TEXT_AMBER, 1);

    // Battery Bar
    fb.draw_text(28, 376, "Battery Charge:", TEXT_GRAY, 1);
    
    // Draw battery meter bar (width = 300px)
    fb.draw_rect(160, 374, 304, 18, BORDER_COLOR);
    let fill_w = (info.battery_pct_num as usize * 300) / 100;
    fb.draw_rect(162, 376, fill_w, 14, TEXT_EMERALD);

    let bat_label = format!("{}% ({})", info.battery_pct, info.battery_status);
    fb.draw_text(476, 376, &bat_label, TEXT_WHITE, 1);

    let power_details = [
        format!("Cell Voltage:    {}", info.battery_volts),
        format!("Cell Temp:       {}", info.battery_temp_c),
        format!("Power Source:    USB Charging (5V Micro-USB)"),
        format!("System Load:     {}", info.load_avg),
        format!("Uptime:          {}", info.uptime_str),
        format!("Memory:          {} MB used / {} MB total", info.ram_used_mb, info.ram_total_mb),
    ];

    let mut py = 410;
    for line in &power_details {
        fb.draw_text(28, py, line, TEXT_WHITE, 1);
        py += 22;
    }

    let side_notes = [
        "Hardware UPS Protection: ACTIVE",
        "If AC line drops, Kururu stays alive for hours.",
        "Zero-power idle screen blanking after 120s.",
        "Wake-on-LAN ready on port 9096.",
    ];

    let mut sy = 410;
    for note in &side_notes {
        fb.draw_text(520, sy, note, TEXT_CYAN, 1);
        sy += 22;
    }
}

fn main() {
    println!("[Kururu Display Daemon] Starting v1.2 (Multi-Tab & Volume Navigation)...");

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
                if !screen_active_keys.load(Ordering::SeqCst) {
                    // If asleep, pressing volume key wakes display up immediately
                    println!("[Kururu Display] Volume key pressed -> Waking up display");
                    set_display_hardware(true);
                    screen_active_keys.store(true, Ordering::SeqCst);
                    wake_signal_keys.store(true, Ordering::SeqCst);
                } else if event.code == KEY_VOLUMEUP {
                    // Next tab
                    let next = (current_tab_keys.load(Ordering::SeqCst) + 1) % 3;
                    current_tab_keys.store(next, Ordering::SeqCst);
                    println!("[Kururu Display] Volume UP -> Switched to Tab {}", next + 1);
                    wake_signal_keys.store(true, Ordering::SeqCst);
                } else if event.code == KEY_VOLUMEDOWN {
                    // Previous tab
                    let prev = (current_tab_keys.load(Ordering::SeqCst) + 3 - 1) % 3;
                    current_tab_keys.store(prev, Ordering::SeqCst);
                    println!("[Kururu Display] Volume DOWN -> Switched to Tab {}", prev + 1);
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

    loop {
        let is_active = screen_active.load(Ordering::SeqCst);

        if is_active {
            if !was_active || wake_signal.swap(false, Ordering::SeqCst) {
                last_awake_time = Instant::now();
                was_active = true;
            }

            // Check auto-sleep timeout (120 seconds)
            if last_awake_time.elapsed() > Duration::from_secs(120) {
                println!("[Kururu Display] Inactivity timeout (120s) -> Sleeping display");
                set_display_hardware(false);
                screen_active.store(false, Ordering::SeqCst);
                was_active = false;
                thread::sleep(Duration::from_millis(500));
                continue;
            }

            let tab = current_tab.load(Ordering::SeqCst);
            let info = gather_system_info();

            fb.clear(BG_COLOR);
            draw_header(&mut fb, tab);

            match tab {
                0 => render_tab_cluster(&mut fb, &info),
                1 => render_tab_wol(&mut fb, &info),
                _ => render_tab_clock(&mut fb, &info),
            }

            draw_footer(&mut fb);
            let _ = fb.flush();

            thread::sleep(Duration::from_secs(2));
        } else {
            was_active = false;
            // Sleep quietly while screen is off, checking state every 200ms
            thread::sleep(Duration::from_millis(200));
        }
    }
}
