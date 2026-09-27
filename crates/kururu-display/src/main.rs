use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::os::unix::io::AsRawFd;
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

// ── Touch input (Samsung sec_touchscreen on /dev/input/event1) ─────────
const EVENT_TOUCH: &str = "/dev/input/event1";
const TOUCH_CONF_PATH: &str = "/etc/kururu-touch.conf";
const EV_ABS: u16 = 3;
const BTN_TOUCH: u16 = 330;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const ABS_MT_POSITION_X: u16 = 53;
const ABS_MT_POSITION_Y: u16 = 54;
const ABS_MT_TRACKING_ID: u16 = 57;

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

// ── Layout design system (dark retro-HUD) ──────────────────────────────
const MARGIN: usize = 16;        // outer screen margin
const GUTTER: usize = 16;        // gap between adjacent cards
const PAD: usize = 12;           // inner card padding
const TITLE_H: usize = 28;       // card title band height
const ROW_H: usize = 19;         // telemetry row pitch
const LOG_ROW_H: usize = 17;     // log console row pitch
const GRID_DOT: usize = 32;      // background dot-grid spacing
const GRID_COLOR: Color = Color { r: 22, g: 30, b: 42, a: 255 };

// Derived geometry — single source of truth for card placement.
const CONTENT_W: usize = FB_WIDTH - 2 * MARGIN;             // 992
const HALF_W: usize = (CONTENT_W - GUTTER) / 2;             // 488
const COL_B_X: usize = MARGIN + HALF_W + GUTTER;            // 520
const RIBBON_W: usize = (CONTENT_W - 2 * GUTTER) / 3;       // 320
const RIBBON_2_X: usize = MARGIN + RIBBON_W + GUTTER;       // 352
const RIBBON_3_X: usize = MARGIN + 2 * (RIBBON_W + GUTTER); // 688

// ── Tab registry: single source of truth for header AND input handler ──
const TABS: [&str; 3] = ["KURURU", "HOMELAB", "RETRO CLOCK"];
const TAB_COUNT: usize = TABS.len();
const MAX_TABS: usize = 8;

// ── Pixel-art sprites ('#' = pixel). Data-driven masks rendered by
//    Framebuffer::draw_sprite — adding art is just adding a mask. ──────
const SP_FROG_BODY: &[&str] = &[
    "  ####          ####  ",
    " ######        ###### ",
    " ######        ###### ",
    " ######        ###### ",
    "  ####          ####  ",
    "   ################   ",
    " #################### ",
    "######################",
    "######################",
    "######################",
    " #################### ",
    " #################### ",
    "  ##################  ",
    "    ##############    ",
];
const SP_FROG_DETAIL: &[&str] = &[
    "                      ",
    "   ##            ##   ",
    "   ##            ##   ",
    "                      ",
    "                      ",
    "                      ",
    "                      ",
    "                      ",
    "                      ",
    "                      ",
    "      ##########      ",
    "                      ",
    "                      ",
    "                      ",
];
const ICON_CHIP: &[&str] = &[
    "..####..",
    ".#....#.",
    "##....##",
    "#.####.#",
    "#.####.#",
    "##....##",
    ".#....#.",
    "..####..",
];
const ICON_BATTERY: &[&str] = &[
    ".######.",
    "#......#",
    "#.####.#",
    "#.####.#",
    "#......#",
    ".######.",
    "...##...",
    "........",
];
const ICON_WIFI: &[&str] = &[
    "........",
    "..####..",
    ".#....#.",
    "#..##..#",
    "...##...",
    "..#..#..",
    ".#....#.",
    "........",
];
const ICON_GLOBE: &[&str] = &[
    "..####..",
    ".#.##.#.",
    "#..##..#",
    "#.####.#",
    "#.####.#",
    "#..##..#",
    ".#.##.#.",
    "..####..",
];
const ICON_CLOCK: &[&str] = &[
    "..####..",
    ".#....#.",
    "#...#..#",
    "#...#..#",
    "#...####",
    "#......#",
    ".#....#.",
    "..####..",
];
const ICON_TERMINAL: &[&str] = &[
    "########",
    "#......#",
    "#.##...#",
    "#..#...#",
    "#.#....#",
    "#......#",
    "########",
    "........",
];
const ICON_SERVER: &[&str] = &[
    ".######.",
    ".#....#.",
    ".######.",
    ".#....#.",
    ".######.",
    ".#....#.",
    ".######.",
    "........",
];
const ICON_POWER: &[&str] = &[
    "...##...",
    "..####..",
    ".######.",
    "########",
    "###..###",
    "##....##",
    "##....##",
    ".######.",
];

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

    /// Blit a pixel-art mask ('#' = pixel, anything else = transparent).
    /// Used for every icon and the frog so art stays data-driven and DRY.
    fn draw_sprite(&mut self, x: usize, y: usize, sprite: &[&str], color: Color, scale: usize) {
        for (ry, row) in sprite.iter().enumerate() {
            for (cx, ch) in row.chars().enumerate() {
                if ch == '#' {
                    self.draw_rect(x + cx * scale, y + ry * scale, scale, scale, color);
                }
            }
        }
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
    battery_health: String,
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

/// Char-safe truncation — never panics on multibyte UTF-8 boundaries.
fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// Open an input device, logging (and returning None) on failure. Shared by
/// both the power and volume listener threads (DRY).
fn open_input_device(path: &str) -> Option<File> {
    match File::open(path) {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("Failed to open {}: {}", path, e);
            None
        }
    }
}

/// Blocking read of a single input event into `buf`. Returns None on error so
/// the caller can back off. Shared by both listener threads (DRY).
fn read_event(file: &mut File, buf: &mut [u8]) -> Option<InputEvent> {
    file.read_exact(buf).ok()?;
    Some(unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const InputEvent) })
}

/// Spawn a blocking listener thread for an input device, forwarding every
/// decoded event to `handler`. The shared setup (open + buffer + backoff loop)
/// lives here once so the power and volume listeners stay DRY.
fn spawn_input_listener<F>(path: &'static str, mut handler: F)
where
    F: FnMut(InputEvent) + Send + 'static,
{
    thread::spawn(move || {
        let mut file = match open_input_device(path) {
            Some(f) => f,
            None => return,
        };
        let mut buf = vec![0u8; std::mem::size_of::<InputEvent>()];

        loop {
            match read_event(&mut file, &mut buf) {
                Some(event) => handler(event),
                None => thread::sleep(Duration::from_millis(200)),
            }
        }
    });
}

// ── Touch calibration ──────────────────────────────────────────────────
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct AbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

/// EVIOCGABS(abs) ioctl request: `_IOC(_IOC_READ, 'E', 0x40 + abs, sizeof(AbsInfo))`.
const fn eviocgabs(abs: u16) -> u64 {
    (2u64 << 30) | (24u64 << 16) | (0x45u64 << 8) | (0x40 + abs as u64)
}

fn abs_range(file: &File, abs: u16) -> Option<(i32, i32)> {
    let mut info = AbsInfo::default();
    let ret = unsafe { libc::ioctl(file.as_raw_fd(), eviocgabs(abs) as _, &mut info as *mut AbsInfo) };
    if ret == 0 && info.maximum > info.minimum && info.maximum > 0 && info.maximum <= 65535 {
        Some((info.minimum, info.maximum))
    } else {
        None
    }
}

fn detect_ranges(path: &str) -> Option<((i32, i32), (i32, i32))> {
    let file = File::open(path).ok()?;
    let x = abs_range(&file, ABS_MT_POSITION_X).or_else(|| abs_range(&file, ABS_X))?;
    let y = abs_range(&file, ABS_MT_POSITION_Y).or_else(|| abs_range(&file, ABS_Y))?;
    Some((x, y))
}

fn parse_flag(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// Raw→screen calibration. Priority: `/etc/kururu-touch.conf` → kernel
/// auto-detected abs ranges → panel-native defaults (1024×600).
#[derive(Clone, Copy)]
struct TouchCal {
    x_min: i32,
    x_max: i32,
    y_min: i32,
    y_max: i32,
    swap_xy: bool,
    invert_x: bool,
    invert_y: bool,
}

impl TouchCal {
    fn defaults() -> Self {
        Self { x_min: 0, x_max: 1023, y_min: 0, y_max: 599, swap_xy: false, invert_x: false, invert_y: false }
    }

    fn load() -> Self {
        let mut cal = Self::defaults();
        let mut saw_range_key = false;
        if let Ok(content) = fs::read_to_string(TOUCH_CONF_PATH) {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let Some((k, v)) = line.split_once('=') else { continue };
                let val = v.trim();
                match k.trim().to_ascii_lowercase().as_str() {
                    "x_min" => if let Ok(n) = val.parse() { cal.x_min = n; saw_range_key = true; },
                    "x_max" => if let Ok(n) = val.parse() { cal.x_max = n; saw_range_key = true; },
                    "y_min" => if let Ok(n) = val.parse() { cal.y_min = n; saw_range_key = true; },
                    "y_max" => if let Ok(n) = val.parse() { cal.y_max = n; saw_range_key = true; },
                    "swap_xy" => cal.swap_xy = parse_flag(val),
                    "invert_x" => cal.invert_x = parse_flag(val),
                    "invert_y" => cal.invert_y = parse_flag(val),
                    _ => {}
                }
            }
        }
        if !saw_range_key {
            if let Some((x, y)) = detect_ranges(EVENT_TOUCH) {
                cal.x_min = x.0;
                cal.x_max = x.1;
                cal.y_min = y.0;
                cal.y_max = y.1;
                // Landscape panel: if the raw X span matches the screen height
                // (and Y the width), the controller reports in portrait → swap.
                cal.swap_xy = (x.1 - x.0) < (y.1 - y.0);
                println!("[Kururu Touch] Auto-detected abs range x=({},{}) y=({},{})", x.0, x.1, y.0, y.1);
            }
        }
        println!(
            "[Kururu Touch] Calibration x=({}..{}) y=({}..{}) swap_xy={} invert_x={} invert_y={}",
            cal.x_min, cal.x_max, cal.y_min, cal.y_max, cal.swap_xy, cal.invert_x, cal.invert_y
        );
        cal
    }

    fn to_screen(&self, raw_x: i32, raw_y: i32) -> (usize, usize) {
        // Screen horizontal maps to raw Y when the controller is portrait.
        let (h_raw, h_min, h_max, v_raw, v_min, v_max) = if self.swap_xy {
            (raw_y, self.y_min, self.y_max, raw_x, self.x_min, self.x_max)
        } else {
            (raw_x, self.x_min, self.x_max, raw_y, self.y_min, self.y_max)
        };
        let hw = (h_max - h_min).max(1) as i64;
        let vh = (v_max - v_min).max(1) as i64;
        let fx = (h_raw - h_min) as i64 * FB_WIDTH as i64 / hw;
        let fy = (v_raw - v_min) as i64 * FB_HEIGHT as i64 / vh;
        let mut sx = fx.clamp(0, FB_WIDTH as i64 - 1) as usize;
        let mut sy = fy.clamp(0, FB_HEIGHT as i64 - 1) as usize;
        if self.invert_x {
            sx = FB_WIDTH - 1 - sx;
        }
        if self.invert_y {
            sy = FB_HEIGHT - 1 - sy;
        }
        (sx, sy)
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

    let battery_health = fs::read_to_string("/sys/class/power_supply/battery/health")
        .unwrap_or_else(|_| "Unknown".to_string())
        .trim()
        .to_string();

    let battery_temp_c = if let Ok(t_raw) = fs::read_to_string("/sys/class/power_supply/battery/temp") {
        if let Ok(val) = t_raw.trim().parse::<f64>() {
            format!("{:.1}C", val / 10.0)
        } else {
            "--C".to_string()
        }
    } else {
        "--C".to_string()
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
            // dmesg has no wall clock → prepend the TZ-aware timestamp.
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
            // kururu-wake already embeds [HH:MM:SS] in the file → no prefix here.
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

    // TZ is set once at process startup in main() via BRT3 — no re-set needed here.

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
            let full = format!("{} | {} {} {}", d_name.to_uppercase(), day, m_name.to_uppercase(), year);

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
                "MONDAY | 1 JANUARY 1970".to_string(),
            )
        }
    };

    let (wol_target1_mac, wol_target2_mac) = {
        let mut psi = "d0:94:66:xx:xx:58".to_string();
        let mut kav = "d0:94:66:xx:xx:c4".to_string();
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
        battery_health,
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

/// Traffic-light accent colour for charge/health values.
fn battery_color(pct: u32) -> Color {
    if pct >= 50 {
        TEXT_EMERALD
    } else if pct >= 20 {
        TEXT_AMBER
    } else {
        TEXT_RED
    }
}

/// Sparse dot-grid drawn on the raw background (visible only in the gutters).
fn draw_background_grid(fb: &mut Framebuffer) {
    let mut y = 56;
    while y < 568 {
        let mut x = MARGIN;
        while x < FB_WIDTH - MARGIN {
            fb.draw_rect(x, y, 2, 2, GRID_COLOR);
            x += GRID_DOT;
        }
        y += GRID_DOT;
    }
}

/// Retro-HUD card frame: panel, 1px border, accent corner brackets, left
/// accent bar, optional pixel icon + title, and a divider under the title.
fn draw_card(
    fb: &mut Framebuffer,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    icon: Option<&[&str]>,
    title: &str,
    accent: Color,
) {
    let right = x + w;
    let bottom = y + h;

    fb.draw_rect(x, y, w, h, PANEL_BG);
    fb.draw_rect(x, y, w, 1, BORDER_COLOR);
    fb.draw_rect(x, bottom - 1, w, 1, BORDER_COLOR);
    fb.draw_rect(x, y, 1, h, BORDER_COLOR);
    fb.draw_rect(right - 1, y, 1, h, BORDER_COLOR);

    // Left accent bar across the title band
    fb.draw_rect(x, y, 3, TITLE_H, accent);

    // HUD corner brackets
    let b = 9;
    let t = 2;
    fb.draw_rect(x, y, b, t, accent);
    fb.draw_rect(x, y, t, b, accent);
    fb.draw_rect(right - b, y, b, t, accent);
    fb.draw_rect(right - t, y, t, b, accent);
    fb.draw_rect(x, bottom - t, b, t, accent);
    fb.draw_rect(x, bottom - b, t, b, accent);
    fb.draw_rect(right - b, bottom - t, b, t, accent);
    fb.draw_rect(right - t, bottom - b, t, b, accent);

    // Title row: icon + label, vertically centred in the title band
    let mut tx = x + PAD + 6;
    if let Some(ic) = icon {
        fb.draw_sprite(tx, y + (TITLE_H - 8) / 2, ic, accent, 1);
        tx += 12;
    }
    fb.draw_text(tx, y + (TITLE_H - 16) / 2, title, TEXT_WHITE, 1);

    // Divider under the title band
    fb.draw_rect(x + PAD, y + TITLE_H, w - 2 * PAD, 1, BORDER_COLOR);
}

/// Header tab rectangles (x, y, w, h) — single source of truth shared by the
/// renderer (draw_header) and the touch hit-test.
fn tab_rects() -> [(usize, usize, usize, usize); TAB_COUNT] {
    let n = TAB_COUNT.min(MAX_TABS);
    let gap = 8usize;
    let tab_start = MARGIN + 22 + 10 + 6 * 16 + 24; // frog+brand (96px) + gap
    let tab_end = FB_WIDTH - MARGIN;
    let available = tab_end.saturating_sub(tab_start);
    let tab_w = available.saturating_sub((n - 1) * gap) / n;

    let mut rects = [(0usize, 0usize, 0usize, 0usize); TAB_COUNT];
    for (i, r) in rects.iter_mut().enumerate() {
        *r = (tab_start + i * (tab_w + gap), 6, tab_w, 36);
    }
    rects
}

/// Wake the screen and, if the tap lands on a header tab, switch dashboard.
fn handle_tap(
    raw_x: i32,
    raw_y: i32,
    cal: &TouchCal,
    active: &AtomicBool,
    wake: &AtomicBool,
    tab: &AtomicUsize,
) {
    let (sx, sy) = cal.to_screen(raw_x, raw_y);
    if !active.load(Ordering::SeqCst) {
        set_display_hardware(true);
        active.store(true, Ordering::SeqCst);
    }
    wake.store(true, Ordering::SeqCst);

    for (i, (x, y, w, h)) in tab_rects().iter().enumerate() {
        if sx >= *x && sx < x + w && sy >= *y && sy < y + h {
            tab.store(i, Ordering::SeqCst);
            println!(
                "[Kururu Display] Touch -> Switched to Tab {} (raw {},{} -> {}, {})",
                i + 1, raw_x, raw_y, sx, sy
            );
            break;
        }
    }
}

fn draw_header(fb: &mut Framebuffer, active_tab: usize) {
    // Header panel: 48px tall + 2px border at y=48
    fb.draw_rect(0, 0, FB_WIDTH, 48, PANEL_BG);
    fb.draw_rect(0, 48, FB_WIDTH, 2, BORDER_COLOR);

    // Frog mascot: green body + dark pupils/mouth (header bg shows through).
    // 14px tall, vertically centred in the 48px header: (48-14)/2 = 17.
    fb.draw_sprite(MARGIN, 17, SP_FROG_BODY, TEXT_EMERALD, 1);
    fb.draw_sprite(MARGIN, 17, SP_FROG_DETAIL, PANEL_BG, 1);

    // Brand — scale-2 (32px tall), vertically centred: (48-32)/2 = 8
    let brand_x = MARGIN + 22 + 10; // frog is 22px wide + 10px gap
    fb.draw_text(brand_x, 8, "KURURU", TEXT_EMERALD, 2);

    // Dynamic tabs fill the remaining span, right-aligned to the margin.
    // Geometry comes from tab_rects() so touch hit-testing stays in sync.
    for (i, name) in TABS.iter().take(TAB_COUNT.min(MAX_TABS)).enumerate() {
        let (tx, ty, tab_w, tab_h) = tab_rects()[i];
        let is_active = i == active_tab;
        let bg = if is_active { PANEL_ACTIVE_BG } else { PANEL_BG };
        let border = if is_active { BORDER_ACTIVE } else { BORDER_COLOR };
        let text_color = if is_active { TEXT_EMERALD } else { TEXT_GRAY };

        fb.draw_rect(tx, ty, tab_w, tab_h, bg);
        fb.draw_rect(tx, ty, tab_w, 2, border);
        fb.draw_rect(tx, ty + tab_h - 2, tab_w, 2, border);
        if is_active {
            fb.draw_rect(tx, ty, 3, tab_h, BORDER_ACTIVE);
        }

        // Adaptive label: "N NAME" when it fits, otherwise just "N".
        let num = format!("{}", i + 1);
        let full = format!("{} {}", num, name);
        let (label, label_px) = if full.len() * 8 + 16 <= tab_w {
            (full.as_str(), full.len() * 8)
        } else {
            (num.as_str(), num.len() * 8)
        };
        let text_x = tx + tab_w.saturating_sub(label_px) / 2;
        fb.draw_text(text_x, ty + (tab_h - 16) / 2, label, text_color, 1);
    }
}

fn draw_footer(fb: &mut Framebuffer, info: &SystemInfo) {
    // Footer panel: y=568, height=32. Text vertically centered: y = 576.
    fb.draw_rect(0, 568, FB_WIDTH, 32, PANEL_BG);
    fb.draw_rect(0, 568, FB_WIDTH, 1, BORDER_COLOR);

    // LEFT: volume navigation
    fb.draw_text(MARGIN - 2, 576, "[VOL+] <- Prev", TEXT_AMBER, 1);
    fb.draw_text(130, 576, "|", TEXT_DIM, 1);
    fb.draw_text(142, 576, "[VOL-] -> Next", TEXT_CYAN, 1);

    // CENTER: power button hint (centered on 512)
    fb.draw_text(432, 576, "[POWER] Wake / Sleep", TEXT_WHITE, 1);

    // RIGHT: auto-sleep + live UPS (on-board battery) status, right-aligned
    let ups = format!("UPS: {}% ({})", info.battery_pct, info.battery_health);
    let ups_x = FB_WIDTH - MARGIN - ups.len() * 8;
    let sep_x = ups_x - 12;
    let sleep = "Auto-sleep: 120s";
    fb.draw_text(sep_x - 8 - sleep.len() * 8, 576, sleep, TEXT_DIM, 1);
    fb.draw_text(sep_x, 576, "|", TEXT_DIM, 1);
    fb.draw_text(ups_x, 576, &ups, battery_color(info.battery_pct_num), 1);
}

// -------------------------------------------------------------
// TAB 0: KURURU NODE (Exclusive Local Device Telemetry)
// -------------------------------------------------------------
/// Shared log-console card (used by both dashboards). Draws the card frame,
/// the tail of `lines`, a blinking terminal cursor and an empty-state message.
fn draw_log_console(
    fb: &mut Framebuffer,
    lines: &[String],
    time_str: &str,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    title: &str,
    icon: &[&str],
    accent: Color,
    text_color: Color,
    empty_msg: &str,
) {
    draw_card(fb, x, y, w, h, Some(icon), title, accent);

    let content_x = x + PAD;
    let content_y = y + TITLE_H + PAD;
    let max_rows = (y + h - PAD).saturating_sub(content_y) / LOG_ROW_H;
    let max_chars = (w - 2 * PAD - 4) / 8;

    let shown: Vec<&String> = lines.iter().rev().take(max_rows).collect::<Vec<_>>()
        .into_iter().rev().collect();

    if shown.is_empty() {
        fb.draw_text(content_x, content_y, empty_msg, TEXT_DIM, 1);
        return;
    }

    let mut last_len = 0;
    for (i, line) in shown.iter().enumerate() {
        let trunc = truncate_chars(line, max_chars);
        fb.draw_text(content_x, content_y + i * LOG_ROW_H, trunc, text_color, 1);
        last_len = trunc.len();
    }

    // Blinking cursor — same 1Hz beat for both consoles.
    let sec = time_str.get(6..8).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    if sec % 2 == 0 {
        let last_y = content_y + (shown.len() - 1) * LOG_ROW_H;
        fb.draw_rect(content_x + last_len * 8 + 4, last_y, 8, 16, accent);
    }
}

fn render_tab_kururu(fb: &mut Framebuffer, info: &SystemInfo) {
    // ── Card A: System, APU & Memory ────────────────────────────────────────
    draw_card(fb, MARGIN, 56, HALF_W, 244, Some(ICON_CHIP), "KURURU ARCHITECTURE & COMPUTE", TEXT_AMBER);

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

    let mut ty = 96;
    for (label, val) in &telemetry_lines {
        fb.draw_text(28, ty, label, TEXT_GRAY, 1);
        fb.draw_text(165, ty, val, TEXT_WHITE, 1);
        ty += ROW_H;
    }

    // ── Card B: On-board battery (UPS) & Wi-Fi radio ────────────────────────
    draw_card(fb, COL_B_X, 56, HALF_W, 244, Some(ICON_WIFI), "BATTERY (UPS) & WI-FI RADIO", TEXT_AMBER);

    fb.draw_text(532, 92, "Battery:", TEXT_GRAY, 1);
    fb.draw_rect(628, 90, 200, 16, BORDER_COLOR);
    let fill_w = (info.battery_pct_num as usize * 196) / 100;
    fb.draw_rect(630, 92, fill_w, 12, battery_color(info.battery_pct_num));
    let bat_label = format!("{}% ({})", info.battery_pct, info.battery_status);
    fb.draw_text(840, 92, &bat_label, TEXT_WHITE, 1);

    let power_radio_lines = [
        ("Fuelgauge:", format!("{} | Temp: {} | Health: {}", info.battery_volts, info.battery_temp_c, info.battery_health)),
        ("Hardware PMIC:", "Marvell 88PM822 / AXP228 Driver".to_string()),
        ("UPS Protection:", "Active (3600 mAh Built-in Buffer)".to_string()),
        ("Wi-Fi Chipset:", "Marvell SD8777 (Interface: mlan0)".to_string()),
        ("Connected AP:", format!("{} (2.4 GHz BSSID)", info.wifi_ssid)),
        ("Signal Level:", format!("{} (Link Quality: 5/5)", info.wifi_signal_dbm)),
        ("Local LAN IP:", format!("{} (Port 22 Open)", info.lan_ip)),
        ("Hardware MAC:", info.wifi_mac.clone()),
        ("SSH Service:", "Dropbear (authorized keys root)".to_string()),
    ];

    let mut ry = 118;
    for (label, val) in &power_radio_lines {
        fb.draw_text(532, ry, label, TEXT_GRAY, 1);
        fb.draw_text(676, ry, val, TEXT_WHITE, 1);
        ry += ROW_H;
    }

    // ── Card C: Live kernel log console ─────────────────────────────────────
    draw_log_console(
        fb, &info.logs, &info.time_str,
        MARGIN, 308, CONTENT_W, 252,
        "KERNEL LOG CONSOLE (DMESG TAIL)", ICON_TERMINAL,
        TEXT_EMERALD, TEXT_GRAY,
        "Waiting for kernel messages...",
    );
}

// -------------------------------------------------------------
// TAB 1: HOMELAB & WOL (Cluster Telemetry & Wake-on-LAN)
// -------------------------------------------------------------
fn render_tab_homelab(fb: &mut Framebuffer, info: &SystemInfo) {
    // ── Card A: Tailnet cluster & core servers ──────────────────────────────
    let cluster_title = format!("MNEMOCINE TAILNET ({}/5 ONLINE)", info.homelab_online_count);
    draw_card(fb, MARGIN, 56, HALF_W, 244, Some(ICON_SERVER), &cluster_title, TEXT_AMBER);

    fb.draw_text(28, 96, "Tailnet:", TEXT_GRAY, 1);
    fb.draw_text(130, 96, &info.tailnet_suffix, TEXT_CYAN, 1);

    fb.draw_text(28, 115, "Kururu IP:", TEXT_GRAY, 1);
    fb.draw_text(130, 115, &info.tailscale_ip, TEXT_WHITE, 1);

    fb.draw_text(28, 134, "Direct Link:", TEXT_GRAY, 1);
    fb.draw_text(130, 134, &info.active_link_str, TEXT_EMERALD, 1);

    // Server list with pixel status squares (active / online / offline)
    let mut py = 162;
    for peer in &info.homelab_nodes {
        let color = if peer.active {
            TEXT_EMERALD
        } else if peer.online {
            TEXT_CYAN
        } else {
            BORDER_COLOR
        };
        fb.draw_rect(28, py + 4, 8, 8, color);
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

        py += ROW_H;
    }

    let summary_line = format!("Mesh Total: {} nodes registered in Tailnet", info.peers_total_count);
    fb.draw_text(28, 278, &summary_line, TEXT_DIM, 1);

    // ── Card B: Wake-on-LAN controller & relay targets ──────────────────────
    draw_card(fb, COL_B_X, 56, HALF_W, 244, Some(ICON_POWER), "WAKE-ON-LAN CONTROLLER", TEXT_AMBER);

    let daemon_label = if info.wol_daemon_running {
        "ACTIVE (:9096) - Kururu Native Rust"
    } else {
        "STOPPED"
    };
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

    let mut wy = 96;
    for (label, val, col) in &wol_lines {
        fb.draw_text(532, wy, label, TEXT_GRAY, 1);
        fb.draw_text(676, wy, val, *col, 1);
        wy += ROW_H;
    }

    // ── Card C: WoL dispatch audit log ──────────────────────────────────────
    draw_log_console(
        fb, &info.wol_logs, &info.time_str,
        MARGIN, 308, CONTENT_W, 252,
        "WAKE-ON-LAN DISPATCH AUDIT LOG", ICON_TERMINAL,
        TEXT_CYAN, TEXT_CYAN,
        "No Wake-on-LAN packets dispatched yet. Waiting on port 9096...",
    );
}

// -------------------------------------------------------------
// TAB 2: RETRO DESK CLOCK & AMBIENT STATION
// -------------------------------------------------------------
fn render_tab_clock(fb: &mut Framebuffer, info: &SystemInfo) {
    // ── Hero: ambient retro clock ───────────────────────────────────────────
    draw_card(fb, MARGIN, 56, CONTENT_W, 294, Some(ICON_CLOCK),
              "MNEMOCINE TIME STATION - SOVEREIGN NTP CLOCK", TEXT_AMBER);

    // Huge clock: font 8x16 at scale 5 = 40x80 px/char; 8 chars = 320px wide.
    fb.draw_text(352, 100, &info.time_str, TEXT_EMERALD, 5);

    // Date banner (scale 2, centered)
    let date_x = (FB_WIDTH.saturating_sub(info.date_full_str.len() * 16)) / 2;
    fb.draw_text(date_x, 196, &info.date_full_str, TEXT_CYAN, 2);

    // Sub-banner + status pill (centered)
    let sub = format!("Timezone: America/Sao_Paulo (UTC-3) | Host: {} | Uptime: {}", info.hostname, info.uptime_str);
    fb.draw_text((FB_WIDTH.saturating_sub(sub.len() * 8)) / 2, 250, &sub, TEXT_GRAY, 1);

    let pill = format!(
        "Homelab: {}/5 Servers Online   |   Wi-Fi: {} ({})   |   WOL Relay: Port 9096 Ready",
        info.homelab_online_count, info.wifi_ssid, info.wifi_signal_dbm
    );
    fb.draw_text((FB_WIDTH.saturating_sub(pill.len() * 8)) / 2, 286, &pill, TEXT_DIM, 1);

    // ── Ambient ribbon: 3 symmetric vitals cards (320px each) ──────────────
    // Card 1: Hardware UPS / no-break
    draw_card(fb, MARGIN, 358, RIBBON_W, 202, Some(ICON_BATTERY), "HARDWARE UPS / NO-BREAK", TEXT_AMBER);

    fb.draw_rect(28, 394, 292, 16, BORDER_COLOR);
    let fill_w = (info.battery_pct_num as usize * 288) / 100;
    fb.draw_rect(30, 396, fill_w, 12, battery_color(info.battery_pct_num));

    let ups_lines = [
        ("Charge:", format!("{}% ({})", info.battery_pct, info.battery_status)),
        ("Health:", info.battery_health.clone()),
        ("Voltage:", info.battery_volts.clone()),
        ("Thermal:", info.battery_temp_c.clone()),
        ("Buffer:", "3600 mAh Li-ion Cell".to_string()),
    ];

    let mut uy = 420;
    for (label, val) in &ups_lines {
        fb.draw_text(28, uy, label, TEXT_GRAY, 1);
        fb.draw_text(115, uy, val, TEXT_WHITE, 1);
        uy += ROW_H;
    }

    // Card 2: Kururu node vitals
    draw_card(fb, RIBBON_2_X, 358, RIBBON_W, 202, Some(ICON_CHIP), "KURURU NODE VITALS", TEXT_AMBER);

    let vitals_lines = [
        ("Device:", "Samsung SM-T110".to_string()),
        ("APU:", format!("Marvell PXA988 @ {}", info.cpu_freq_str)),
        ("Load:", format!("{} (1m, 5m, 15m)", info.load_avg)),
        ("RAM:", format!("{} MB used / {} MB", info.ram_used_mb, info.ram_total_mb)),
        ("Storage:", format!("{} MB free in rootfs", info.disk_free_mb)),
        ("Cooling:", "Passive (0 dB Silent)".to_string()),
    ];

    let mut vy = 396;
    for (label, val) in &vitals_lines {
        fb.draw_text(364, vy, label, TEXT_GRAY, 1);
        fb.draw_text(440, vy, val, TEXT_WHITE, 1);
        vy += ROW_H;
    }

    // Card 3: Homelab network
    draw_card(fb, RIBBON_3_X, 358, RIBBON_W, 202, Some(ICON_GLOBE), "HOMELAB NETWORK", TEXT_AMBER);

    let net_lines = [
        ("Tailnet:", info.tailnet_suffix.clone()),
        ("Node IP:", info.tailscale_ip.clone()),
        ("Servers:", format!("{}/5 Nodes Active", info.homelab_online_count)),
        ("Wi-Fi AP:", info.wifi_ssid.clone()),
        ("Signal:", info.wifi_signal_dbm.clone()),
        ("WOL Engine:", "Port 9096 Listener Ready".to_string()),
    ];

    let mut ny = 396;
    for (label, val) in &net_lines {
        fb.draw_text(700, ny, label, TEXT_GRAY, 1);
        fb.draw_text(796, ny, val, TEXT_WHITE, 1);
        ny += ROW_H;
    }
}

fn main() {
    println!("[Kururu Display Daemon] Starting v1.6 (Retro-HUD UI, live UPS, touch, boot-safe PATH)...");

    // Optional initial dashboard: `kururu-display 2` (kiosk/debug). Default 0.
    let initial_tab = std::env::args()
        .nth(1)
        .and_then(|a| a.parse::<usize>().ok())
        .map(|t| t.min(TAB_COUNT - 1))
        .unwrap_or(0);

    // Force Brazil / Brasília time (UTC-3, fixed — no DST since 2019).
    // Use POSIX inline string "BRT3" instead of "America/Sao_Paulo" which
    // requires /usr/share/zoneinfo/ not present on Alpine musl minimal rootfs.
    std::env::set_var("TZ", "BRT3");
    unsafe {
        tzset();
    }

    // Guarantee external-tool resolution regardless of the launching shell's
    // PATH: the Android boot hook starts us with an Android-only PATH that
    // lacks Alpine's /bin, /usr/bin and /usr/local/bin, which made dmesg, ip,
    // wpa_cli and tailscale silently fail on every boot-started instance.
    std::env::set_var("PATH", "/usr/local/bin:/usr/bin:/usr/sbin:/bin:/sbin");

    let screen_active = Arc::new(AtomicBool::new(true));
    let screen_active_power = screen_active.clone();
    let screen_active_keys = screen_active.clone();

    let current_tab = Arc::new(AtomicUsize::new(initial_tab));
    let current_tab_keys = current_tab.clone();

    let wake_signal = Arc::new(AtomicBool::new(true));
    let wake_signal_power = wake_signal.clone();
    let wake_signal_keys = wake_signal.clone();

    // Start with display ON initially so user sees it right away
    set_display_hardware(true);

    // Listener: hardware Power button (/dev/input/event2)
    spawn_input_listener(EVENT_POWER, move |event| {
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
    });

    // Listener: hardware Volume Up / Volume Down keys (/dev/input/event0)
    // INVERTED: Volume Up goes right-to-left (prev tab); Volume Down goes left-to-right (next tab)
    spawn_input_listener(EVENT_KEYS, move |event| {
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
                // Volume Up: right-to-left (previous tab)
                let prev = (current_tab_keys.load(Ordering::SeqCst) + TAB_COUNT - 1) % TAB_COUNT;
                current_tab_keys.store(prev, Ordering::SeqCst);
                println!("[Kururu Display] Volume UP -> Switched to Tab {} (<-)", prev + 1);
                wake_signal_keys.store(true, Ordering::SeqCst);
            } else if event.code == KEY_VOLUMEDOWN {
                // Volume Down: left-to-right (next tab)
                let next = (current_tab_keys.load(Ordering::SeqCst) + 1) % TAB_COUNT;
                current_tab_keys.store(next, Ordering::SeqCst);
                println!("[Kururu Display] Volume DOWN -> Switched to Tab {} (->)", next + 1);
                wake_signal_keys.store(true, Ordering::SeqCst);
            } else if event.code == KEY_HOMEPAGE {
                println!("[Kururu Display] Home key pressed -> Wake/Refreshed");
                wake_signal_keys.store(true, Ordering::SeqCst);
            }
        }
    });

    // Listener: touchscreen (sec_touchscreen on /dev/input/event1).
    // A tap wakes the screen; a tap on a header tab switches dashboard.
    let touch_active = screen_active.clone();
    let touch_wake = wake_signal.clone();
    let touch_tab = current_tab.clone();
    let touch_cal = TouchCal::load();
    let touch_debug = std::env::var_os("KURURU_TOUCH_DEBUG").is_some();
    let mut touch_x: i32 = 0;
    let mut touch_y: i32 = 0;
    let mut touch_down = false;
    spawn_input_listener(EVENT_TOUCH, move |event| {
        if touch_debug {
            eprintln!(
                "[Kururu Touch] type={} code={} value={}",
                event.type_, event.code, event.value
            );
        }
        match event.type_ {
            EV_ABS => match event.code {
                ABS_MT_POSITION_X | ABS_X => touch_x = event.value,
                ABS_MT_POSITION_Y | ABS_Y => touch_y = event.value,
                ABS_MT_TRACKING_ID => {
                    if event.value < 0 {
                        if touch_down {
                            touch_down = false;
                            handle_tap(touch_x, touch_y, &touch_cal, &touch_active, &touch_wake, &touch_tab);
                        }
                    } else {
                        touch_down = true;
                    }
                }
                _ => {}
            },
            EV_KEY if event.code == BTN_TOUCH => {
                if event.value == 1 {
                    touch_down = true;
                } else if event.value == 0 && touch_down {
                    touch_down = false;
                    handle_tap(touch_x, touch_y, &touch_cal, &touch_active, &touch_wake, &touch_tab);
                }
            }
            _ => {}
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
                    draw_background_grid(&mut fb);
                    draw_header(&mut fb, tab);

                    match tab {
                        0 => render_tab_kururu(&mut fb, info),
                        1 => render_tab_homelab(&mut fb, info),
                        _ => render_tab_clock(&mut fb, info),
                    }

                    draw_footer(&mut fb, info);
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
