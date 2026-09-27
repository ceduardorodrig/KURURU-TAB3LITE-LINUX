mod font;

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const FB_WIDTH: usize = 1024;
const FB_HEIGHT: usize = 600;
const FB_STRIDE: usize = 4096;
const FB_SIZE: usize = FB_WIDTH * FB_HEIGHT * 4;

const BACKLIGHT_PATH: &str = "/sys/class/backlight/panel/brightness";
const BLANK_PATH: &str = "/sys/class/graphics/fb0/blank";
const EVENT_PATH: &str = "/dev/input/event2";
const FB_PATH: &str = "/dev/graphics/fb0";

const EV_KEY: u16 = 1;
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
pub const BORDER_COLOR: Color = Color::rgb(40, 56, 80);
pub const TEXT_EMERALD: Color = Color::rgb(46, 213, 115);
pub const TEXT_CYAN: Color = Color::rgb(72, 219, 251);
pub const TEXT_AMBER: Color = Color::rgb(254, 202, 87);
pub const TEXT_WHITE: Color = Color::rgb(245, 246, 250);
pub const TEXT_GRAY: Color = Color::rgb(130, 140, 155);
pub const TEXT_DIM: Color = Color::rgb(80, 92, 108);

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

struct SystemInfo {
    hostname: String,
    uptime_str: String,
    load_avg: String,
    ram_used_mb: u64,
    ram_total_mb: u64,
    battery_pct: String,
    battery_status: String,
    lan_ip: String,
    tailscale_ip: String,
    logs: Vec<String>,
}

fn gather_system_info() -> SystemInfo {
    let hostname = fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap_or_else(|_| "kururu".to_string())
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

    let battery_pct = fs::read_to_string("/sys/class/power_supply/battery/capacity")
        .unwrap_or_else(|_| "--".to_string())
        .trim()
        .to_string();

    let battery_status = fs::read_to_string("/sys/class/power_supply/battery/status")
        .unwrap_or_else(|_| "Unknown".to_string())
        .trim()
        .to_string();

    let mut lan_ip = "Connecting...".to_string();
    let mut tailscale_ip = "Connecting...".to_string();

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
                    } else if cur_iface.starts_with("tailscale") {
                        tailscale_ip = ip.to_string();
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

    SystemInfo {
        hostname,
        uptime_str,
        load_avg,
        ram_used_mb,
        ram_total_mb,
        battery_pct,
        battery_status,
        lan_ip,
        tailscale_ip,
        logs,
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

fn render_dashboard(fb: &mut Framebuffer, info: &SystemInfo) {
    fb.clear(BG_COLOR);

    // Top Header Banner
    fb.draw_rect(0, 0, FB_WIDTH, 48, PANEL_BG);
    fb.draw_rect(0, 48, FB_WIDTH, 2, BORDER_COLOR);

    fb.draw_text(16, 12, "KURURU LINUX", TEXT_EMERALD, 2);
    fb.draw_text(
        220,
        18,
        "Samsung Galaxy Tab 3 Lite (SM-T110) • Headless Node",
        TEXT_CYAN,
        1,
    );
    fb.draw_text(
        660,
        18,
        "StenioSentinel Governed • Mnemocine Homelab",
        TEXT_GRAY,
        1,
    );

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
        format!("Tailscale IP: {} [Connected]", info.tailscale_ip),
        format!("LAN IP:       {} (wlan0/mlan0)", info.lan_ip),
        format!("Load Average: {}", info.load_avg),
        format!("Uptime:       {}", info.uptime_str),
        format!(
            "RAM Usage:    {} MB / {} MB ({}% free)",
            info.ram_used_mb, info.ram_total_mb, free_pct
        ),
        format!(
            "Battery:      {}% ({})",
            info.battery_pct, info.battery_status
        ),
        format!("SSH Server:   Dropbear (port 22, root pubkey)"),
    ];

    let mut ty = 96;
    for line in &telemetry_lines {
        let (label, val) = if let Some(idx) = line.find(':') {
            (&line[..=idx], &line[idx + 1..])
        } else {
            (line.as_str(), "")
        };
        fb.draw_text(28, ty, label, TEXT_GRAY, 1);
        fb.draw_text(150, ty, val.trim_start(), TEXT_WHITE, 1);
        ty += 21;
    }

    // Quick Stats & Homelab Card (Right)
    fb.draw_rect(512, 60, 496, 240, PANEL_BG);
    fb.draw_rect(512, 60, 496, 2, BORDER_COLOR);
    fb.draw_text(524, 72, "CLUSTER CONNECTIVITY", TEXT_AMBER, 1);

    let cluster_lines = [
        format!("Tailnet:      mnemocine.ts.net"),
        format!("DNS:          100.100.100.100 (MagicDNS)"),
        format!("Peers:        psicopompo, ybytu, ybyra, kuaray"),
        format!("Kernel:       3.4.5-kururu-headless-armv7l"),
        format!("Architecture: ARMv7-a (Marvell PXA986 Dual Core)"),
        format!("Display:      1024x600 60Hz (Panel 88PM822 Backlight)"),
        format!("Power Key:    /dev/input/event2 (Hardware Wake/Sleep)"),
        format!("Touch / GUI:  Disabled (Zero JVM / TouchWiz Overhead)"),
    ];

    let mut cy = 96;
    for line in &cluster_lines {
        let (label, val) = if let Some(idx) = line.find(':') {
            (&line[..=idx], &line[idx + 1..])
        } else {
            (line.as_str(), "")
        };
        fb.draw_text(524, cy, label, TEXT_GRAY, 1);
        fb.draw_text(644, cy, val.trim_start(), TEXT_WHITE, 1);
        cy += 21;
    }

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

    // Bottom Navigation Bar
    fb.draw_rect(0, 568, FB_WIDTH, 32, PANEL_BG);
    fb.draw_rect(0, 568, FB_WIDTH, 1, BORDER_COLOR);
    fb.draw_text(
        16,
        576,
        "[POWER BUTTON] Press to Sleep / Wake Display",
        TEXT_CYAN,
        1,
    );
    fb.draw_text(
        420,
        576,
        "Auto-sleep: 120s inactivity timer",
        TEXT_DIM,
        1,
    );
    fb.draw_text(
        780,
        576,
        "Zero-Waste Tech Recycling",
        TEXT_EMERALD,
        1,
    );
}

fn main() {
    println!("[Kururu Display Daemon] Starting v1.0...");

    let screen_active = Arc::new(AtomicBool::new(true));
    let screen_active_clone = screen_active.clone();

    // Start with display ON initially so user sees it right away
    set_display_hardware(true);

    // Thread: Listen to hardware Power button
    thread::spawn(move || {
        let mut file = match File::open(EVENT_PATH) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Failed to open {}: {}", EVENT_PATH, e);
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
                let current = screen_active_clone.load(Ordering::SeqCst);
                let new_state = !current;
                println!(
                    "[Kururu Display] Power button toggled -> State: {}",
                    if new_state { "AWAKE" } else { "SLEEP" }
                );
                set_display_hardware(new_state);
                screen_active_clone.store(new_state, Ordering::SeqCst);
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
            if !was_active {
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

            let info = gather_system_info();
            render_dashboard(&mut fb, &info);
            let _ = fb.flush();

            thread::sleep(Duration::from_secs(2));
        } else {
            was_active = false;
            // Sleep quietly while screen is off, checking state every 250ms
            thread::sleep(Duration::from_millis(250));
        }
    }
}
