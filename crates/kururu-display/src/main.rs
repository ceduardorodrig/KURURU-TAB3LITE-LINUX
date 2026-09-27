use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::os::unix::io::AsRawFd;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

mod font;
mod terminal;

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
// Capacitive touchkeys: delivered by the touchscreen controller (event1).
const KEY_MENU: u16 = 139;
const KEY_BACK: u16 = 158;

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

// ── Themes (runtime-selectable palettes) ───────────────────────────────
const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color { r, g, b, a: 255 }
}

/// Linear per-channel blend: `pct`% of `b` over `a` (0..=100).
const fn mix(a: Color, b: Color, pct: i32) -> Color {
    Color {
        r: (a.r as i32 + (b.r as i32 - a.r as i32) * pct / 100) as u8,
        g: (a.g as i32 + (b.g as i32 - a.g as i32) * pct / 100) as u8,
        b: (a.b as i32 + (b.b as i32 - a.b as i32) * pct / 100) as u8,
        a: 255,
    }
}

#[derive(Clone, Copy)]
struct Theme {
    key: &'static str,
    name: &'static str,
    bg: Color,
    panel: Color,
    panel_active: Color,
    border: Color,
    text: Color,
    text_muted: Color,
    text_dim: Color,
    accent: Color,
    info: Color,
    warn: Color,
    error: Color,
    grid: Color,
}

/// Derive a monochrome-phosphor theme from a (background, foreground) pair,
/// mirroring cool-retro-term's bg/fontColor schemes.
const fn pal(key: &'static str, name: &'static str, bg: Color, fg: Color) -> Theme {
    Theme {
        key,
        name,
        bg,
        panel: mix(bg, fg, 10),
        panel_active: mix(bg, fg, 22),
        border: mix(bg, fg, 32),
        text: fg,
        text_muted: mix(bg, fg, 68),
        text_dim: mix(bg, fg, 45),
        accent: fg,
        info: fg,
        warn: rgb(255, 176, 0),
        error: rgb(255, 77, 77),
        grid: mix(bg, fg, 12),
    }
}

impl Theme {
    /// Default palette of the project (emerald / cyan, multi-colour).
    const KURURU: Theme = Theme {
        key: "kururu",
        name: "Kururu",
        bg: rgb(10, 14, 20),
        panel: rgb(16, 22, 32),
        panel_active: rgb(24, 36, 54),
        border: rgb(35, 48, 68),
        text: rgb(240, 244, 250),
        text_muted: rgb(140, 155, 175),
        text_dim: rgb(80, 95, 115),
        accent: rgb(0, 230, 118),
        info: rgb(0, 210, 255),
        warn: rgb(255, 180, 0),
        error: rgb(255, 82, 82),
        grid: rgb(22, 30, 42),
    };
}

const THEME_COUNT: usize = 15;

/// Kururu (multi-colour, default) + the cool-retro-term schemes (monochrome phosphor).
static THEMES: [Theme; THEME_COUNT] = [
    Theme::KURURU,
    pal("amber", "Amber", rgb(0, 0, 0), rgb(0xff, 0x81, 0x00)),
    pal("monochrome_green", "Monochrome Green", rgb(0, 0, 0), rgb(0x0c, 0xcc, 0x68)),
    pal("deep_blue", "Deep Blue", rgb(0, 0, 0), rgb(0x7f, 0xb4, 0xff)),
    pal("c64", "Commodore 64", rgb(0x3b, 0x3b, 0x8f), rgb(0xa9, 0xa7, 0xff)),
    pal("pet", "Commodore PET", rgb(0, 0, 0), rgb(0xff, 0xff, 0xff)),
    pal("apple2", "Apple ][", rgb(0x00, 0x11, 0x00), rgb(0x4d, 0xff, 0x6b)),
    pal("atari400", "Atari 400", rgb(0x0f, 0x1f, 0x5a), rgb(0x8e, 0xd6, 0xff)),
    pal("ibm_vga", "IBM VGA 8x16", rgb(0, 0, 0), rgb(0xc0, 0xc0, 0xc0)),
    pal("ibm3278", "IBM 3278 Reborn", rgb(0, 0, 0), rgb(0x3c, 0xff, 0x7a)),
    pal("neon_cyan", "Neon Cyan", rgb(0x00, 0x10, 0x18), rgb(0x52, 0xf7, 0xff)),
    pal("ghost", "Ghost Terminal", rgb(0x0b, 0x10, 0x14), rgb(0xa6, 0xb3, 0xc0)),
    pal("plasma", "Plasma", rgb(0x07, 0x00, 0x14), rgb(0xff, 0x9b, 0xd6)),
    pal("boring", "Boring", rgb(0, 0, 0), rgb(0xff, 0xff, 0xff)),
    pal("eink", "E-Ink", rgb(0xf2, 0xf2, 0xec), rgb(0x10, 0x10, 0x10)),
];

// Start on Kururu (default); overridden by config/env.
static THEME_IDX: AtomicUsize = AtomicUsize::new(0);

/// Current palette (selected via config/env or the Menu).
fn theme() -> &'static Theme {
    &THEMES[THEME_IDX.load(Ordering::Relaxed).min(THEME_COUNT - 1)]
}

/// Resolve a theme index by its short key (used by the config file).
fn theme_index_by_key(key: &str) -> Option<usize> {
    THEMES.iter().position(|t| t.key.eq_ignore_ascii_case(key))
}

//── CRT effect flags (toggleable post-process) ────────────────────────
const EFFECT_SCANLINES: u32 = 1 << 0;
const EFFECT_VIGNETTE: u32 = 1 << 1;
const EFFECT_ALL: u32 = EFFECT_SCANLINES | EFFECT_VIGNETTE;
static EFFECTS: AtomicU32 = AtomicU32::new(EFFECT_ALL);

/// Backlight level (8..=255) applied when the screen is on.
static BRIGHTNESS: AtomicUsize = AtomicUsize::new(180);
/// Auto-sleep timeout in seconds (0 = never).
static SLEEP_SECS: AtomicUsize = AtomicUsize::new(120);

const BACKLIGHT_PATHS: [&str; 2] = [
    "/sys/class/backlight/panel/brightness",
    "/sys/class/backlight/pwm-backlight/brightness",
];

fn write_backlight(v: usize) {
    let val = format!("{}\n", v.min(255));
    for p in BACKLIGHT_PATHS {
        let _ = fs::write(p, val.as_bytes());
    }
}

// ── Layout design system (dark retro-HUD) ──────────────────────────────
const MARGIN: usize = 16;        // outer screen margin
const GUTTER: usize = 16;        // gap between adjacent cards
const PAD: usize = 12;           // inner card padding
const TITLE_H: usize = 28;       // card title band height
const ROW_H: usize = 19;         // telemetry row pitch
const LOG_ROW_H: usize = 17;     // log console row pitch
const GRID_DOT: usize = 32;      // background dot-grid spacing

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

// ── Unified UI shell: screens + menu registry ──────────────────────────
const SCREEN_DASHBOARD: usize = 0;
const SCREEN_MENU: usize = 1;
const SCREEN_ABOUT: usize = 2;
const SCREEN_SETTINGS: usize = 3;
const SCREEN_TERMINAL: usize = 4;

/// Action triggered by selecting a menu entry.
#[derive(Clone, Copy)]
enum MenuAction {
    Screen(usize),
    ToggleTheme,
    ToggleEffects,
}

/// Menu entries: (label, action). Single source of truth for the renderer AND
/// the input handlers (buttons + touch).
const MENU_ITEMS: [(&str, MenuAction); 6] = [
    ("Dashboard", MenuAction::Screen(SCREEN_DASHBOARD)),
    ("Settings", MenuAction::Screen(SCREEN_SETTINGS)),
    ("Terminal", MenuAction::Screen(SCREEN_TERMINAL)),
    ("Theme", MenuAction::ToggleTheme),
    ("Effects", MenuAction::ToggleEffects),
    ("About", MenuAction::Screen(SCREEN_ABOUT)),
];
const MENU_COUNT: usize = MENU_ITEMS.len();
const MENU_ITEM_H: usize = 40;
const MENU_ITEM_GAP: usize = 8;
const MENU_X: usize = 300;
const MENU_W: usize = FB_WIDTH - 2 * MENU_X; // centered column (424)
const MENU_Y0: usize = 200;

/// Dashboard header title (its width drives the tab start, see tab_rects()).
const HEADER_TITLE: &str = "DASHBOARD";

/// Rectangle of a menu row — shared by render_menu and the touch hit-test.
fn menu_item_rect(i: usize) -> (usize, usize, usize, usize) {
    (MENU_X, MENU_Y0 + i * (MENU_ITEM_H + MENU_ITEM_GAP), MENU_W, MENU_ITEM_H)
}

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
#[derive(Default)]
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

/// Iterate `key=value` lines of a config file, skipping blanks and comments.
/// Shared by the touch calibration and display config parsers (DRY).
fn for_each_conf_line<F: FnMut(&str, &str)>(path: &str, mut f: F) {
    if let Ok(content) = fs::read_to_string(path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                f(k.trim(), v.trim());
            }
        }
    }
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
        for_each_conf_line(TOUCH_CONF_PATH, |k, val| {
            match k.to_ascii_lowercase().as_str() {
                "x_min" => if let Ok(n) = val.parse() { cal.x_min = n; saw_range_key = true; },
                "x_max" => if let Ok(n) = val.parse() { cal.x_max = n; saw_range_key = true; },
                "y_min" => if let Ok(n) = val.parse() { cal.y_min = n; saw_range_key = true; },
                "y_max" => if let Ok(n) = val.parse() { cal.y_max = n; saw_range_key = true; },
                "swap_xy" => cal.swap_xy = parse_flag(val),
                "invert_x" => cal.invert_x = parse_flag(val),
                "invert_y" => cal.invert_y = parse_flag(val),
                _ => {}
            }
        });
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

/// Cheap telemetry: file reads / syscalls / clock only (no subprocesses).
/// Refreshed every second so the clock and vitals stay live.
fn gather_fast(info: &mut SystemInfo) {
    info.hostname = fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap_or_else(|_| "kururu".to_string())
        .trim()
        .to_string();

    info.kernel_version = fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_else(|_| "3.4.5".to_string())
        .trim()
        .to_string();

    info.uptime_str = if let Ok(u) = fs::read_to_string("/proc/uptime") {
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

    info.load_avg = if let Ok(l) = fs::read_to_string("/proc/loadavg") {
        let parts: Vec<&str> = l.split_whitespace().take(3).collect();
        parts.join(", ")
    } else {
        "Unknown".to_string()
    };

    info.cpu_freq_str = if let Ok(f) = fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq") {
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
    info.ram_total_mb = total_kb / 1024;
    let available_kb = free_kb + buffers_kb + cached_kb;
    let used_kb = if total_kb > available_kb { total_kb - available_kb } else { 0 };
    info.ram_used_mb = used_kb / 1024;

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
    info.disk_total_mb = disk_total_mb;
    info.disk_free_mb = disk_free_mb;
    info.disk_used_mb = if disk_total_mb > disk_free_mb { disk_total_mb - disk_free_mb } else { 0 };

    let battery_pct_raw = fs::read_to_string("/sys/class/power_supply/battery/capacity")
        .unwrap_or_else(|_| "50".to_string());
    info.battery_pct_num = battery_pct_raw.trim().parse::<u32>().unwrap_or(50);
    info.battery_pct = info.battery_pct_num.to_string();

    info.battery_status = fs::read_to_string("/sys/class/power_supply/battery/status")
        .unwrap_or_else(|_| "Unknown".to_string())
        .trim()
        .to_string();

    info.battery_health = fs::read_to_string("/sys/class/power_supply/battery/health")
        .unwrap_or_else(|_| "Unknown".to_string())
        .trim()
        .to_string();

    info.battery_temp_c = if let Ok(t_raw) = fs::read_to_string("/sys/class/power_supply/battery/temp") {
        if let Ok(val) = t_raw.trim().parse::<f64>() {
            format!("{:.1}C", val / 10.0)
        } else {
            "--C".to_string()
        }
    } else {
        "--C".to_string()
    };

    info.battery_volts = if let Ok(v_raw) = fs::read_to_string("/sys/class/power_supply/battery/voltage_now") {
        if let Ok(val) = v_raw.trim().parse::<f64>() {
            format!("{:.2}V", val / 1_000_000.0)
        } else {
            "--V".to_string()
        }
    } else {
        "--V".to_string()
    };

    info.wifi_signal_dbm = "-35 dBm".to_string();
    if let Ok(content) = fs::read_to_string("/proc/net/wireless") {
        for line in content.lines() {
            if line.contains("mlan0:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 4 {
                    let lvl = parts[3].trim_end_matches('.');
                    info.wifi_signal_dbm = format!("{} dBm", lvl);
                }
            }
        }
    }

    const DAY_NAMES: [&str; 7] = [
        "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday",
    ];
    const MONTH_NAMES: [&str; 12] = [
        "January", "February", "March", "April", "May", "June", "July", "August",
        "September", "October", "November", "December",
    ];
    // TZ is set once at process startup in main() via BRT3.
    unsafe {
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
            info.time_str = format!("{:02}:{:02}:{:02}", h, m, s);
            info.date_str = format!("{:02}/{:02}/{}", day, mon + 1, year);
            info.day_name = DAY_NAMES[wday].to_string();
            info.date_full_str = format!(
                "{} | {} {} {}",
                DAY_NAMES[wday].to_uppercase(), day, MONTH_NAMES[mon_idx].to_uppercase(), year
            );
        } else {
            info.time_str = "00:00:00".to_string();
            info.date_str = "01/01/1970".to_string();
            info.day_name = "Monday".to_string();
            info.date_full_str = "MONDAY | 1 JANUARY 1970".to_string();
        }
    }
}

/// Expensive telemetry: spawns subprocesses (ip, wpa_cli, tailscale, dmesg)
/// and reads config/log files. Refreshed every few seconds.
fn gather_slow(info: &mut SystemInfo) {
    info.lan_ip = "192.168.3.55".to_string();
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
                        info.lan_ip = ip.to_string();
                    }
                }
            }
        }
    }

    info.wifi_ssid = "Cratos".to_string();
    info.wifi_mac = "00:50:43:XX:XX:XX".to_string();
    if let Ok(output) = Command::new("wpa_cli").arg("status").output() {
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("ssid=") {
                info.wifi_ssid = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("address=") {
                info.wifi_mac = rest.trim().to_string();
            }
        }
    }

    info.tailscale_ip = "100.127.188.45".to_string();
    info.tailnet_suffix = "chimaera-heptatonic.ts.net".to_string();
    info.homelab_online_count = 0;
    info.peers_total_count = 0;
    info.active_link_str = "None (Idle)".to_string();
    info.homelab_nodes.clear();

    if let Ok(output) = Command::new("tailscale").args(["status", "--json"]).output() {
        if let Ok(ts) = serde_json::from_slice::<TailscaleJson>(&output.stdout) {
            if let Some(suffix) = ts.magic_dns_suffix {
                if !suffix.is_empty() {
                    info.tailnet_suffix = suffix;
                }
            } else if let Some(ct) = ts.current_tailnet {
                if let Some(suffix) = ct.magic_dns_suffix {
                    if !suffix.is_empty() {
                        info.tailnet_suffix = suffix;
                    }
                }
            }

            if let Some(self_node) = ts.self_node {
                if let Some(ips) = self_node.tailscale_ips {
                    if let Some(first_ip) = ips.first() {
                        info.tailscale_ip = first_ip.clone();
                    }
                }
            }

            if let Some(peer_map) = ts.peer {
                info.peers_total_count = peer_map.len();
                let mut all_peers = Vec::new();

                for (_k, v) in peer_map {
                    let name = v.hostname.unwrap_or_else(|| "unknown".to_string());
                    let online = v.online.unwrap_or(false);
                    let active = v.active.unwrap_or(false);
                    let ip = v.tailscale_ips.and_then(|ips| ips.first().cloned()).unwrap_or_default();
                    let cur_addr = v.cur_addr.unwrap_or_default();

                    if active && !name.is_empty() {
                        info.active_link_str = if !cur_addr.is_empty() {
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
                            info.homelab_online_count += 1;
                        }
                        info.homelab_nodes.push(PeerDisplayInfo {
                            name: canonical.to_string(),
                            ip: p.ip.clone(),
                            online: p.online,
                            active: p.active,
                            cur_addr: p.cur_addr.clone(),
                        });
                    } else {
                        info.homelab_nodes.push(PeerDisplayInfo {
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
    info.logs.clear();
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
            info.logs.push(format!("{} {}", ts, line.trim_end()));
        }
    }

    info.wol_logs.clear();
    if let Ok(content) = fs::read_to_string(WOL_LOG_PATH) {
        let lines: Vec<&str> = content.lines().collect();
        let start = if lines.len() > 14 {
            lines.len() - 14
        } else {
            0
        };
        for line in &lines[start..] {
            // kururu-wake already embeds [HH:MM:SS] in the file → no prefix here.
            info.wol_logs.push(line.trim_end().to_string());
        }
    }

    info.wol_daemon_running = check_wol_daemon();

    info.wol_target1_mac = "d0:94:66:xx:xx:58".to_string();
    info.wol_target2_mac = "d0:94:66:xx:xx:c4".to_string();
    if let Ok(content) = fs::read_to_string("/etc/kururu-wake.conf") {
        for line in content.lines() {
            let trimmed = line.trim();
            if let Some((k, v)) = trimmed.split_once('=') {
                let k_clean = k.trim().to_lowercase();
                let v_clean = v.trim().trim_matches('"').trim_matches('\'');
                if k_clean == "psicopompo" {
                    info.wol_target1_mac = v_clean.to_string();
                } else if k_clean == "kavure" {
                    info.wol_target2_mac = v_clean.to_string();
                }
            }
        }
    }
}

fn set_display_hardware(enable: bool) {
    let power_path = "/sys/class/graphics/fb0/blank";
    let val = if enable { "0\n" } else { "4\n" };
    let _ = fs::write(power_path, val);

    write_backlight(if enable { BRIGHTNESS.load(Ordering::Relaxed) } else { 0 });
}

/// Traffic-light accent colour for charge/health values.
fn battery_color(pct: u32) -> Color {
    if pct >= 50 {
        theme().accent
    } else if pct >= 20 {
        theme().warn
    } else {
        theme().error
    }
}

/// Sparse dot-grid drawn on the raw background (visible only in the gutters).
fn draw_background_grid(fb: &mut Framebuffer) {
    let mut y = 56;
    while y < 568 {
        let mut x = MARGIN;
        while x < FB_WIDTH - MARGIN {
            fb.draw_rect(x, y, 2, 2, theme().grid);
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

    fb.draw_rect(x, y, w, h, theme().panel);
    fb.draw_rect(x, y, w, 1, theme().border);
    fb.draw_rect(x, bottom - 1, w, 1, theme().border);
    fb.draw_rect(x, y, 1, h, theme().border);
    fb.draw_rect(right - 1, y, 1, h, theme().border);

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
    fb.draw_text(tx, y + (TITLE_H - 16) / 2, title, theme().text, 1);

    // Divider under the title band
    fb.draw_rect(x + PAD, y + TITLE_H, w - 2 * PAD, 1, theme().border);
}

/// Header tab rectangles (x, y, w, h) — single source of truth shared by the
/// renderer (draw_header) and the touch hit-test.
fn tab_rects() -> [(usize, usize, usize, usize); TAB_COUNT] {
    let n = TAB_COUNT.min(MAX_TABS);
    let gap = 8usize;
    let tab_start = MARGIN + 24 + HEADER_TITLE.len() * 16 + 24; // accent+title + gap
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
    screen: &AtomicUsize,
    menu_cursor: &AtomicUsize,
) {
    let (sx, sy) = cal.to_screen(raw_x, raw_y);
    if !active.load(Ordering::SeqCst) {
        set_display_hardware(true);
        active.store(true, Ordering::SeqCst);
    }
    wake.store(true, Ordering::SeqCst);

    match screen.load(Ordering::SeqCst) {
        SCREEN_DASHBOARD => {
            for (i, (x, y, w, h)) in tab_rects().iter().enumerate() {
                if sx >= *x && sx < x + w && sy >= *y && sy < y + h {
                    tab.store(i, Ordering::SeqCst);
                    println!(
                        "[Kururu Display] Touch -> Tab {} (raw {},{} -> {}, {})",
                        i + 1, raw_x, raw_y, sx, sy
                    );
                    break;
                }
            }
        }
        SCREEN_MENU => {
            for i in 0..MENU_COUNT {
                let (x, y, w, h) = menu_item_rect(i);
                if sx >= x && sx < x + w && sy >= y && sy < y + h {
                    menu_cursor.store(i, Ordering::SeqCst);
                    println!("[Kururu Display] Touch -> Menu '{}'", MENU_ITEMS[i].0);
                    if let Some(target) = apply_menu_action(MENU_ITEMS[i].1) {
                        screen.store(target, Ordering::SeqCst);
                    }
                    break;
                }
            }
        }
        SCREEN_SETTINGS => {
            let editing = wifi_form().lock().map(|f| f.editing).unwrap_or(false);
            if editing {
                keyboard_tap(sx, sy, false);
            } else {
                settings_tap(sx, sy);
            }
        }
        SCREEN_TERMINAL => {
            keyboard_tap(sx, sy, true);
        }
        _ => {
            // Any tap on a secondary screen goes back to the menu.
            screen.store(SCREEN_MENU, Ordering::SeqCst);
        }
    }
}

fn draw_header(fb: &mut Framebuffer, active_tab: usize) {
    // Header panel: 48px tall + 2px border at y=48
    fb.draw_rect(0, 0, FB_WIDTH, 48, theme().panel);
    fb.draw_rect(0, 48, FB_WIDTH, 2, theme().border);

    // Unified top bar: accent marker + screen title (matches Menu/Settings/...).
    fb.draw_rect(MARGIN, 18, 12, 12, theme().accent);
    fb.draw_text(MARGIN + 24, 8, HEADER_TITLE, theme().text, 2);

    // Dynamic tabs fill the remaining span, right-aligned to the margin.
    // Geometry comes from tab_rects() so touch hit-testing stays in sync.
    for (i, name) in TABS.iter().take(TAB_COUNT.min(MAX_TABS)).enumerate() {
        let (tx, ty, tab_w, tab_h) = tab_rects()[i];
        let is_active = i == active_tab;
        let bg = if is_active { theme().panel_active } else { theme().panel };
        let border = if is_active { theme().accent } else { theme().border };
        let text_color = if is_active { theme().accent } else { theme().text_muted };

        fb.draw_rect(tx, ty, tab_w, tab_h, bg);
        fb.draw_rect(tx, ty, tab_w, 2, border);
        fb.draw_rect(tx, ty + tab_h - 2, tab_w, 2, border);
        if is_active {
            fb.draw_rect(tx, ty, 3, tab_h, theme().accent);
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
    fb.draw_rect(0, 568, FB_WIDTH, 32, theme().panel);
    fb.draw_rect(0, 568, FB_WIDTH, 1, theme().border);

    // LEFT: volume navigation
    fb.draw_text(MARGIN - 2, 576, "[VOL+] <- Prev", theme().warn, 1);
    fb.draw_text(130, 576, "|", theme().text_dim, 1);
    fb.draw_text(142, 576, "[VOL-] -> Next", theme().info, 1);

    // CENTER: power button hint (centered on 512)
    fb.draw_text(432, 576, "[POWER] Wake / Sleep", theme().text, 1);

    // RIGHT: auto-sleep + live UPS (on-board battery) status, right-aligned
    let ups = format!("UPS: {}% ({})", info.battery_pct, info.battery_health);
    let ups_x = FB_WIDTH - MARGIN - ups.len() * 8;
    let sep_x = ups_x - 12;
    let sleep = "Auto-sleep: 120s";
    fb.draw_text(sep_x - 8 - sleep.len() * 8, 576, sleep, theme().text_dim, 1);
    fb.draw_text(sep_x, 576, "|", theme().text_dim, 1);
    fb.draw_text(ups_x, 576, &ups, battery_color(info.battery_pct_num), 1);
}

// -------------------------------------------------------------
// Unified UI shell — design-system widgets + non-dashboard screens
// -------------------------------------------------------------
/// Top bar for non-dashboard screens (consistent with the dashboard header).
fn draw_topbar(fb: &mut Framebuffer, title: &str, hint: &str) {
    fb.draw_rect(0, 0, FB_WIDTH, 48, theme().panel);
    fb.draw_rect(0, 48, FB_WIDTH, 2, theme().border);
    fb.draw_rect(MARGIN, 18, 12, 12, theme().accent); // accent marker
    fb.draw_text(MARGIN + 24, 8, title, theme().text, 2);
    if !hint.is_empty() {
        let hx = FB_WIDTH - MARGIN - hint.len() * 8;
        fb.draw_text(hx, 16, hint, theme().text_muted, 1);
    }
}

/// Bottom hint bar for non-dashboard screens.
fn draw_hint_footer(fb: &mut Framebuffer, hint: &str) {
    fb.draw_rect(0, 568, FB_WIDTH, 32, theme().panel);
    fb.draw_rect(0, 568, FB_WIDTH, 1, theme().border);
    let x = (FB_WIDTH.saturating_sub(hint.len() * 8)) / 2;
    fb.draw_text(x, 576, hint, theme().text_dim, 1);
}

/// Reusable list row (menu / settings). `selected` draws the active style.
fn draw_list_item(fb: &mut Framebuffer, x: usize, y: usize, w: usize, label: &str, selected: bool) {
    let h = MENU_ITEM_H;
    let bg = if selected { theme().panel_active } else { theme().panel };
    let border = if selected { theme().accent } else { theme().border };
    let fg = if selected { theme().accent } else { theme().text };
    fb.draw_rect(x, y, w, h, bg);
    fb.draw_rect(x, y, w, 1, border);
    fb.draw_rect(x, y + h - 1, w, 1, border);
    if selected {
        fb.draw_rect(x, y, 3, h, theme().accent);
    }
    fb.draw_rect(x + 16, y + (h - 8) / 2, 8, 8, fg); // bullet
    fb.draw_text(x + 36, y + (h - 16) / 2, label, fg, 1);
}

fn render_menu(fb: &mut Framebuffer, cursor: usize) {
    draw_topbar(fb, "MENU", "[HOME] Enter");

    // ── Brand block: the frog + KURURU live here, prominent ──────────────
    let frog_scale = 3;
    let frog_w = 22 * frog_scale;
    let frog_h = 14 * frog_scale;
    let frog_x = (FB_WIDTH - frog_w) / 2;
    let frog_y = 66;
    fb.draw_sprite(frog_x, frog_y, SP_FROG_BODY, theme().accent, frog_scale);
    fb.draw_sprite(frog_x, frog_y, SP_FROG_DETAIL, theme().bg, frog_scale);

    let brand = "KURURU";
    let brand_x = (FB_WIDTH.saturating_sub(brand.len() * 8 * 3)) / 2;
    fb.draw_text(brand_x, frog_y + frog_h + 6, brand, theme().accent, 3);

    let sub = "Mnemocine Homelab";
    let sub_x = (FB_WIDTH.saturating_sub(sub.len() * 8)) / 2;
    fb.draw_text(sub_x, frog_y + frog_h + 6 + 48 + 4, sub, theme().text_dim, 1);

    for (i, (label, action)) in MENU_ITEMS.iter().enumerate() {
        let (x, y, w, _) = menu_item_rect(i);
        let text = match action {
            MenuAction::ToggleTheme => format!("{}: {}", label, theme().name),
            MenuAction::ToggleEffects => {
                let on = EFFECTS.load(Ordering::Relaxed) != 0;
                format!("{}: {}", label, if on { "ON" } else { "OFF" })
            }
            MenuAction::Screen(_) => (*label).to_string(),
        };
        draw_list_item(fb, x, y, w, &text, i == cursor);
    }
    draw_hint_footer(fb, "[VOL+] Up   [VOL-] Down   [HOME] Select   [POWER] Sleep");
}

fn render_about(fb: &mut Framebuffer, info: &SystemInfo) {
    draw_topbar(fb, "ABOUT", "[HOME] Back");
    let lines = [
        "KURURU - Native Headless Linux Node".to_string(),
        "Samsung Galaxy Tab 3 Lite (SM-T110 / goyawifi)".to_string(),
        format!("Alpine Linux / musl  ·  Kernel {}", info.kernel_version),
        "Display: kururu-display v1.8".to_string(),
        format!("Uptime: {}", info.uptime_str),
        "github.com/ceduardorodrig/KURURU-TAB3LITE-LINUX".to_string(),
    ];
    let mut y = 150;
    for line in &lines {
        let x = (FB_WIDTH.saturating_sub(line.len() * 8)) / 2;
        fb.draw_text(x, y, line, theme().text_muted, 1);
        y += 28;
    }
    draw_hint_footer(fb, "[HOME] Back");
}

// -------------------------------------------------------------
// On-screen keyboard (QWERTY + modifiers/arrows) — reusable
// -------------------------------------------------------------
#[derive(Clone, Copy, PartialEq, Eq)]
enum Key {
    Ch(char),
    Backspace,
    Enter,
    Space,
    Tab,
    Esc,
    Shift,
    Ctrl,
    Alt,
    Left,
    Up,
    Down,
    Right,
}

struct KKey {
    label: &'static str,
    key: Key,
    w: usize, // width in units
}

const fn k(label: &'static str, key: Key, w: usize) -> KKey {
    KKey { label, key, w }
}

const KB_ROWS: &[&[KKey]] = &[
    &[
        k("1", Key::Ch('1'), 1), k("2", Key::Ch('2'), 1), k("3", Key::Ch('3'), 1),
        k("4", Key::Ch('4'), 1), k("5", Key::Ch('5'), 1), k("6", Key::Ch('6'), 1),
        k("7", Key::Ch('7'), 1), k("8", Key::Ch('8'), 1), k("9", Key::Ch('9'), 1),
        k("0", Key::Ch('0'), 1), k("-", Key::Ch('-'), 1), k("=", Key::Ch('='), 1),
        k("Bksp", Key::Backspace, 2),
    ],
    &[
        k("Tab", Key::Tab, 2), k("q", Key::Ch('q'), 1), k("w", Key::Ch('w'), 1),
        k("e", Key::Ch('e'), 1), k("r", Key::Ch('r'), 1), k("t", Key::Ch('t'), 1),
        k("y", Key::Ch('y'), 1), k("u", Key::Ch('u'), 1), k("i", Key::Ch('i'), 1),
        k("o", Key::Ch('o'), 1), k("p", Key::Ch('p'), 1), k("[", Key::Ch('['), 1),
        k("]", Key::Ch(']'), 1),
    ],
    &[
        k("Ctrl", Key::Ctrl, 2), k("a", Key::Ch('a'), 1), k("s", Key::Ch('s'), 1),
        k("d", Key::Ch('d'), 1), k("f", Key::Ch('f'), 1), k("g", Key::Ch('g'), 1),
        k("h", Key::Ch('h'), 1), k("j", Key::Ch('j'), 1), k("k", Key::Ch('k'), 1),
        k("l", Key::Ch('l'), 1), k(";", Key::Ch(';'), 1), k("'", Key::Ch('\''), 1),
        k("Enter", Key::Enter, 2),
    ],
    &[
        k("Shift", Key::Shift, 3), k("z", Key::Ch('z'), 1), k("x", Key::Ch('x'), 1),
        k("c", Key::Ch('c'), 1), k("v", Key::Ch('v'), 1), k("b", Key::Ch('b'), 1),
        k("n", Key::Ch('n'), 1), k("m", Key::Ch('m'), 1), k(",", Key::Ch(','), 1),
        k(".", Key::Ch('.'), 1), k("/", Key::Ch('/'), 1), k("Shift", Key::Shift, 3),
    ],
    &[
        k("Alt", Key::Alt, 2), k("Esc", Key::Esc, 2), k("Space", Key::Space, 8),
        k("<-", Key::Left, 1), k("^", Key::Up, 1), k("v", Key::Down, 1), k("->", Key::Right, 1),
    ],
];

const KB_U: usize = 56;      // key unit width
const KB_GAP: usize = 6;
const KB_H: usize = 44;
const KB_TOP: usize = 300;

// Terminal grid geometry (the area above the keyboard).
const TERM_X: usize = MARGIN;
const TERM_Y: usize = 56;
const TERM_COLS: usize = (FB_WIDTH - 2 * MARGIN) / 8;
const TERM_ROWS: usize = (KB_TOP - TERM_Y) / 16;
const KB_ROW_GAP: usize = 8;

/// Bounding box (x, y, w, h) of the key at (row, col).
fn kb_key_rect(row: usize, col: usize) -> (usize, usize, usize, usize) {
    let keys = KB_ROWS[row];
    let total: usize = keys.iter().map(|k| k.w).sum();
    let row_w = total * KB_U + keys.len().saturating_sub(1) * KB_GAP;
    let mut x = (FB_WIDTH.saturating_sub(row_w)) / 2;
    for (i, key) in keys.iter().enumerate() {
        let w = key.w * KB_U + key.w.saturating_sub(1) * KB_GAP;
        if i == col {
            return (x, KB_TOP + row * (KB_H + KB_ROW_GAP), w, KB_H);
        }
        x += w + KB_GAP;
    }
    (0, 0, 0, 0)
}

static KB_SHIFT: AtomicBool = AtomicBool::new(false);
static KB_CTRL: AtomicBool = AtomicBool::new(false);
static KB_ALT: AtomicBool = AtomicBool::new(false);

fn shift_char(c: char) -> char {
    match c {
        '1' => '!', '2' => '@', '3' => '#', '4' => '$', '5' => '%',
        '6' => '^', '7' => '&', '8' => '*', '9' => '(', '0' => ')',
        '-' => '_', '=' => '+', '[' => '{', ']' => '}', ';' => ':',
        '\'' => '"', ',' => '<', '.' => '>', '/' => '?',
        c => c.to_ascii_uppercase(),
    }
}

fn draw_keyboard(fb: &mut Framebuffer) {
    fb.draw_rect(0, KB_TOP - 6, FB_WIDTH, FB_HEIGHT - (KB_TOP - 6), theme().bg);

    for (r, keys) in KB_ROWS.iter().enumerate() {
        for (c, key) in keys.iter().enumerate() {
            let (x, y, w, h) = kb_key_rect(r, c);
            let active = match key.key {
                Key::Shift => KB_SHIFT.load(Ordering::Relaxed),
                Key::Ctrl => KB_CTRL.load(Ordering::Relaxed),
                Key::Alt => KB_ALT.load(Ordering::Relaxed),
                _ => false,
            };
            let bg = if active { theme().panel_active } else { theme().panel };
            let border = if active { theme().accent } else { theme().border };
            fb.draw_rect(x, y, w, h, bg);
            fb.draw_rect(x, y, w, 1, border);
            fb.draw_rect(x, y + h - 1, w, 1, border);
            let scale = if key.label.len() == 1 { 2 } else { 1 };
            let tw = key.label.len() * 8 * scale;
            let tx = x + w.saturating_sub(tw) / 2;
            let ty = y + (h.saturating_sub(16 * scale)) / 2;
            let fg = if active { theme().accent } else { theme().text };
            fb.draw_text(tx, ty, key.label, fg, scale);
        }
    }
}

// -------------------------------------------------------------
// Settings: Wi-Fi connect (uses the keyboard)
// -------------------------------------------------------------
#[derive(Default)]
struct WifiForm {
    ssid: String,
    psk: String,
    focus: usize, // 0 = SSID, 1 = Password, 2 = Connect
    editing: bool,
    status: String,
}

static WIFI_FORM: OnceLock<Mutex<WifiForm>> = OnceLock::new();
fn wifi_form() -> &'static Mutex<WifiForm> {
    WIFI_FORM.get_or_init(|| Mutex::new(WifiForm::default()))
}

const SET_X: usize = 200;
const SET_FIELD_BX: usize = SET_X + 130;
const SET_FIELD_BW: usize = FB_WIDTH - SET_FIELD_BX - MARGIN;
const SET_BTN_W: usize = 36;
const SET_BTN_H: usize = 24;
const SET_MINUS_X: usize = SET_X + 140;
const SET_BAR_X: usize = SET_MINUS_X + SET_BTN_W + 8;
const SET_BAR_W: usize = 300;
const SET_PLUS_X: usize = SET_BAR_X + SET_BAR_W + 8;
const SET_Y_BRIGHT: usize = 78;
const SET_Y_SLEEP: usize = 108;
const SET_Y_VOLUME: usize = 156;
const SET_Y_SSID: usize = 204;
const SET_Y_PSK: usize = 234;
const SET_Y_CONNECT: usize = 264;
const SET_CONNECT_W: usize = 160;
const SET_CONNECT_H: usize = 28;

fn draw_field(fb: &mut Framebuffer, y: usize, label: &str, value: &str, focused: bool, editing: bool) {
    fb.draw_text(SET_X, y, label, theme().text_muted, 1);
    let bg = if focused { theme().panel_active } else { theme().panel };
    let border = if focused { theme().accent } else { theme().border };
    fb.draw_rect(SET_FIELD_BX, y - 4, SET_FIELD_BW, 24, bg);
    fb.draw_rect(SET_FIELD_BX, y - 4, SET_FIELD_BW, 1, border);
    fb.draw_rect(SET_FIELD_BX, y + 19, SET_FIELD_BW, 1, border);
    let shown = if value.is_empty() { "<vazio>" } else { value };
    fb.draw_text(SET_FIELD_BX + 8, y, shown, theme().text, 1);
    if focused && editing {
        let cx = SET_FIELD_BX + 8 + value.len() * 8;
        fb.draw_rect(cx, y, 8, 16, theme().accent); // cursor
    }
}

fn draw_button(fb: &mut Framebuffer, x: usize, y: usize, w: usize, h: usize, label: &str, focused: bool) {
    let bg = if focused { theme().panel_active } else { theme().panel };
    let border = if focused { theme().accent } else { theme().border };
    let fg = if focused { theme().accent } else { theme().text };
    fb.draw_rect(x, y, w, h, bg);
    fb.draw_rect(x, y, w, 1, border);
    fb.draw_rect(x, y + h - 1, w, 1, border);
    fb.draw_text(x + w.saturating_sub(label.len() * 8) / 2, y + h.saturating_sub(16) / 2, label, fg, 1);
}

fn draw_slider(fb: &mut Framebuffer, y: usize, label: &str, value: usize, max: usize) {
    fb.draw_text(SET_X, y, label, theme().text_muted, 1);
    draw_button(fb, SET_MINUS_X, y - 4, SET_BTN_W, SET_BTN_H, "-", false);
    fb.draw_rect(SET_BAR_X, y, SET_BAR_W, 16, theme().border);
    let fill = (value.min(max) * (SET_BAR_W - 4)) / max.max(1);
    fb.draw_rect(SET_BAR_X + 2, y + 2, fill, 12, theme().accent);
    draw_button(fb, SET_PLUS_X, y - 4, SET_BTN_W, SET_BTN_H, "+", false);
}

fn draw_stepper(fb: &mut Framebuffer, y: usize, label: &str, value: &str) {
    fb.draw_text(SET_X, y, label, theme().text_muted, 1);
    draw_button(fb, SET_MINUS_X, y - 4, SET_BTN_W, SET_BTN_H, "-", false);
    fb.draw_text(SET_BAR_X, y, value, theme().text, 1);
    draw_button(fb, SET_PLUS_X, y - 4, SET_BTN_W, SET_BTN_H, "+", false);
}

fn render_settings(fb: &mut Framebuffer) {
    let (ssid, psk_mask, focus, editing, status) = match wifi_form().lock() {
        Ok(f) => (f.ssid.clone(), "*".repeat(f.psk.chars().count()), f.focus, f.editing, f.status.clone()),
        Err(_) => (String::new(), String::new(), 0, false, String::new()),
    };

    let hint = if editing { "[BACK] Done" } else { "[HOME] Edit" };
    draw_topbar(fb, "SETTINGS", hint);

    fb.draw_text(SET_X, 56, "DISPLAY", theme().warn, 1);
    draw_slider(fb, SET_Y_BRIGHT, "Brightness", BRIGHTNESS.load(Ordering::Relaxed), 255);
    let sleep = SLEEP_SECS.load(Ordering::Relaxed);
    let sleep_str = if sleep == 0 { "Off".to_string() } else { format!("{}s", sleep) };
    draw_stepper(fb, SET_Y_SLEEP, "Auto-sleep", &sleep_str);

    fb.draw_text(SET_X, 128, "AUDIO", theme().warn, 1);
    fb.draw_text(SET_X, SET_Y_VOLUME, "Volume", theme().text_muted, 1);
    fb.draw_text(SET_FIELD_BX, SET_Y_VOLUME, "n/d (codec sem mixer)", theme().text_dim, 1);

    fb.draw_text(SET_X, 180, "WI-FI", theme().warn, 1);
    draw_field(fb, SET_Y_SSID, "SSID", &ssid, focus == 0, editing && focus == 0);
    draw_field(fb, SET_Y_PSK, "Password", &psk_mask, focus == 1, editing && focus == 1);
    draw_button(fb, SET_FIELD_BX, SET_Y_CONNECT, SET_CONNECT_W, SET_CONNECT_H, "Connect", focus == 2);

    if editing {
        draw_keyboard(fb);
    } else {
        if !status.is_empty() {
            fb.draw_text(SET_X, 308, &status, theme().info, 1);
        }
        draw_hint_footer(fb, "[VOL+] Field   [VOL-] Field   [HOME] Edit/Connect   [BACK] Menu");
    }
}

fn render_terminal(fb: &mut Framebuffer) {
    draw_topbar(fb, "TERMINAL", "[BACK] Menu");
    if terminal::is_ready() {
        terminal::render(fb, TERM_X, TERM_Y);
    } else {
        fb.draw_text(TERM_X, TERM_Y + 8, "Terminal indisponivel (PTY falhou).", theme().text_dim, 1);
    }
    draw_keyboard(fb);
}

/// Connect wpa_supplicant to an SSID (PSK if provided). Returns a status line.
fn wifi_connect(ssid: &str, psk: &str) -> String {
    let run = |args: &[&str]| -> Option<String> {
        Command::new("wpa_cli")
            .args(args)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let Some(add) = run(&["add_network"]) else {
        return "wpa_cli indisponivel".to_string();
    };
    let id: String = add.chars().filter(|c| c.is_ascii_digit()).collect();
    if id.is_empty() {
        return format!("Falha ao criar rede ({})", add);
    }
    run(&["set_network", &id, "ssid", &format!("\"{}\"", ssid)]);
    if psk.is_empty() {
        run(&["set_network", &id, "key_mgmt", "NONE"]);
    } else {
        run(&["set_network", &id, "psk", &format!("\"{}\"", psk)]);
    }
    run(&["enable_network", &id]);
    run(&["select_network", &id]);
    run(&["save_config"]);
    format!("Conectando a {}...", ssid)
}

fn kb_type_char(ch: char) {
    if let Ok(mut f) = wifi_form().lock() {
        match f.focus {
            0 => f.ssid.push(ch),
            1 => f.psk.push(ch),
            _ => {}
        }
    }
}

fn kb_backspace() {
    if let Ok(mut f) = wifi_form().lock() {
        match f.focus {
            0 => { f.ssid.pop(); }
            1 => { f.psk.pop(); }
            _ => {}
        }
    }
}

fn wifi_submit() {
    let (ssid, psk) = match wifi_form().lock() {
        Ok(f) => (f.ssid.clone(), f.psk.clone()),
        Err(_) => return,
    };
    let status = if ssid.is_empty() {
        "Informe o SSID".to_string()
    } else {
        wifi_connect(&ssid, &psk)
    };
    if let Ok(mut f) = wifi_form().lock() {
        f.status = status;
        f.editing = false;
    }
}

/// Apply a keyboard key press. `terminal_mode` routes characters/keys to the
/// PTY; otherwise they edit the Wi-Fi form.
fn kb_press(key: Key, terminal_mode: bool) {
    match key {
        Key::Shift => KB_SHIFT.store(!KB_SHIFT.load(Ordering::Relaxed), Ordering::Relaxed),
        Key::Ctrl => KB_CTRL.store(!KB_CTRL.load(Ordering::Relaxed), Ordering::Relaxed),
        Key::Alt => KB_ALT.store(!KB_ALT.load(Ordering::Relaxed), Ordering::Relaxed),
        _ if terminal_mode => term_key(key),
        Key::Backspace => kb_backspace(),
        Key::Space => kb_type_char(' '),
        Key::Enter => wifi_submit(),
        Key::Ch(c) => {
            let ch = if KB_SHIFT.load(Ordering::Relaxed) { shift_char(c) } else { c };
            kb_type_char(ch);
            KB_SHIFT.store(false, Ordering::Relaxed);
        }
        _ => {}
    }
}

/// Map a key to terminal bytes (respecting Ctrl/Alt/Shift).
fn term_key(key: Key) {
    let ctrl = KB_CTRL.load(Ordering::Relaxed);
    let alt = KB_ALT.load(Ordering::Relaxed);
    let shift = KB_SHIFT.load(Ordering::Relaxed);
    match key {
        Key::Ch(c) => {
            let mut out: Vec<u8> = Vec::new();
            if ctrl {
                out.push((c as u8) & 0x1f);
            } else {
                let c = if shift { shift_char(c) } else { c };
                if alt {
                    out.push(0x1b);
                }
                let mut b = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            }
            terminal::write(&out);
            KB_SHIFT.store(false, Ordering::Relaxed);
        }
        Key::Enter => terminal::write(b"\r"),
        Key::Backspace => terminal::write(&[0x7f]),
        Key::Space => terminal::write(b" "),
        Key::Tab => terminal::write(b"\t"),
        Key::Esc => terminal::write(&[0x1b]),
        Key::Up => terminal::write(b"\x1b[A"),
        Key::Down => terminal::write(b"\x1b[B"),
        Key::Right => terminal::write(b"\x1b[C"),
        Key::Left => terminal::write(b"\x1b[D"),
        _ => {}
    }
}

/// Touch hit-test over the on-screen keyboard. Returns true if a key was hit.
fn keyboard_tap(sx: usize, sy: usize, terminal_mode: bool) -> bool {
    for (r, keys) in KB_ROWS.iter().enumerate() {
        for c in 0..keys.len() {
            let (x, y, w, h) = kb_key_rect(r, c);
            if sx >= x && sx < x + w && sy >= y && sy < y + h {
                kb_press(keys[c].key, terminal_mode);
                return true;
            }
        }
    }
    false
}

/// Cycle the auto-sleep timeout through a fixed set of values.
fn cycle_sleep(dir: i32) {
    const VALUES: [usize; 5] = [0, 30, 60, 120, 300];
    let cur = SLEEP_SECS.load(Ordering::Relaxed);
    let idx = VALUES.iter().position(|&v| v == cur).unwrap_or(3);
    let next = ((idx as i32 + dir).rem_euclid(VALUES.len() as i32)) as usize;
    SLEEP_SECS.store(VALUES[next], Ordering::Relaxed);
    save_display_config();
}

fn settings_tap(sx: usize, sy: usize) {
    let in_rect = |x: usize, y: usize, w: usize, h: usize| sx >= x && sx < x + w && sy >= y && sy < y + h;

    if in_rect(SET_MINUS_X, SET_Y_BRIGHT - 4, SET_BTN_W, SET_BTN_H) {
        let v = BRIGHTNESS.load(Ordering::Relaxed).saturating_sub(16).max(8);
        BRIGHTNESS.store(v, Ordering::Relaxed);
        write_backlight(v);
        save_display_config();
    } else if in_rect(SET_PLUS_X, SET_Y_BRIGHT - 4, SET_BTN_W, SET_BTN_H) {
        let v = (BRIGHTNESS.load(Ordering::Relaxed) + 16).min(255);
        BRIGHTNESS.store(v, Ordering::Relaxed);
        write_backlight(v);
        save_display_config();
    } else if in_rect(SET_MINUS_X, SET_Y_SLEEP - 4, SET_BTN_W, SET_BTN_H) {
        cycle_sleep(-1);
    } else if in_rect(SET_PLUS_X, SET_Y_SLEEP - 4, SET_BTN_W, SET_BTN_H) {
        cycle_sleep(1);
    } else if in_rect(SET_FIELD_BX, SET_Y_SSID - 4, SET_FIELD_BW, 24) {
        if let Ok(mut f) = wifi_form().lock() { f.focus = 0; f.editing = true; }
    } else if in_rect(SET_FIELD_BX, SET_Y_PSK - 4, SET_FIELD_BW, 24) {
        if let Ok(mut f) = wifi_form().lock() { f.focus = 1; f.editing = true; }
    } else if in_rect(SET_FIELD_BX, SET_Y_CONNECT, SET_CONNECT_W, SET_CONNECT_H) {
        if let Ok(mut f) = wifi_form().lock() { f.focus = 2; }
        wifi_submit();
    }
}

/// Execute a menu action. Returns Some(screen) when the UI must switch screens.
fn apply_menu_action(action: MenuAction) -> Option<usize> {
    match action {
        MenuAction::Screen(target) => Some(target),
        MenuAction::ToggleTheme => {
            let next = (THEME_IDX.load(Ordering::Relaxed) + 1) % THEME_COUNT;
            THEME_IDX.store(next, Ordering::Relaxed);
            save_display_config();
            None
        }
        MenuAction::ToggleEffects => {
            let next = if EFFECTS.load(Ordering::Relaxed) != 0 { 0 } else { EFFECT_ALL };
            EFFECTS.store(next, Ordering::Relaxed);
            save_display_config();
            None
        }
    }
}

/// Central navigation "activate" (HOME / MENU touchkey).
fn nav_activate(screen: &AtomicUsize, cursor: &AtomicUsize) {
    match screen.load(Ordering::SeqCst) {
        SCREEN_DASHBOARD => {
            cursor.store(0, Ordering::SeqCst);
            screen.store(SCREEN_MENU, Ordering::SeqCst);
        }
        SCREEN_MENU => {
            let c = cursor.load(Ordering::SeqCst);
            if let Some(target) = apply_menu_action(MENU_ITEMS[c].1) {
                screen.store(target, Ordering::SeqCst);
            }
        }
        _ => {
            screen.store(SCREEN_MENU, Ordering::SeqCst);
        }
    }
}

/// Central navigation "back" (BACK touchkey).
fn nav_back(screen: &AtomicUsize) {
    let target = match screen.load(Ordering::SeqCst) {
        SCREEN_DASHBOARD => SCREEN_DASHBOARD,
        SCREEN_MENU => SCREEN_DASHBOARD,
        _ => SCREEN_MENU,
    };
    screen.store(target, Ordering::SeqCst);
}

fn set_effect(bit: u32, on: bool) {
    let cur = EFFECTS.load(Ordering::Relaxed);
    EFFECTS.store(if on { cur | bit } else { cur & !bit }, Ordering::Relaxed);
}

/// Load theme/effects from the environment (override) and /etc/kururu-display.conf.
fn load_display_config() {
    let env_theme = std::env::var("KURURU_THEME").ok();
    if let Some(t) = &env_theme {
        if let Some(i) = theme_index_by_key(t) {
            THEME_IDX.store(i, Ordering::Relaxed);
        }
    }
    for_each_conf_line("/etc/kururu-display.conf", |k, val| {
        match k.to_ascii_lowercase().as_str() {
            "theme" => {
                if env_theme.is_none() {
                    if let Some(i) = theme_index_by_key(val) {
                        THEME_IDX.store(i, Ordering::Relaxed);
                    }
                }
            }
            "scanlines" => set_effect(EFFECT_SCANLINES, parse_flag(val)),
            "vignette" => set_effect(EFFECT_VIGNETTE, parse_flag(val)),
            "brightness" => if let Ok(n) = val.parse() { BRIGHTNESS.store(n, Ordering::Relaxed); },
            "sleep" => if let Ok(n) = val.parse() { SLEEP_SECS.store(n, Ordering::Relaxed); },
            _ => {}
        }
    });
    println!(
        "[Kururu Display] Theme: {}  Effects: {:?}",
        theme().name,
        EFFECTS.load(Ordering::Relaxed)
    );

    // Debug/kiosk aid: open Settings with the keyboard already active.
    if std::env::var_os("KURURU_EDIT").is_some() {
        if let Ok(mut f) = wifi_form().lock() {
            f.editing = true;
        }
    }
}

/// Persist the current theme/effects/brightness/sleep to the config file.
fn save_display_config() {
    let e = EFFECTS.load(Ordering::Relaxed);
    let content = format!(
        "# Kururu display config (managed; env KURURU_THEME overrides theme)\ntheme={}\nscanlines={}\nvignette={}\nbrightness={}\nsleep={}\n",
        theme().key,
        e & EFFECT_SCANLINES != 0,
        e & EFFECT_VIGNETTE != 0,
        BRIGHTNESS.load(Ordering::Relaxed),
        SLEEP_SECS.load(Ordering::Relaxed),
    );
    let _ = fs::write("/etc/kururu-display.conf", content);
}

/// Vignette brightness mask (0..=255), computed once.
fn vignette_mask() -> &'static [u8] {
    static MASK: OnceLock<Vec<u8>> = OnceLock::new();
    MASK.get_or_init(|| {
        let mut m = vec![255u8; FB_WIDTH * FB_HEIGHT];
        let cx = FB_WIDTH as f32 / 2.0;
        let cy = FB_HEIGHT as f32 / 2.0;
        let max_d2 = cx * cx + cy * cy;
        for y in 0..FB_HEIGHT {
            for x in 0..FB_WIDTH {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                let t = ((dx * dx + dy * dy) / max_d2).min(1.0);
                m[y * FB_WIDTH + x] = ((1.0 - 0.45 * t) * 255.0) as u8;
            }
        }
        m
    })
}

/// Cheap CRT post-process on the BGRA buffer: scanlines + vignette in a single
/// pass. Runs once per rendered frame.
fn apply_crt_effects(fb: &mut Framebuffer) {
    let effects = EFFECTS.load(Ordering::Relaxed);
    if effects == 0 {
        return;
    }
    let scan = effects & EFFECT_SCANLINES != 0;
    let vign = effects & EFFECT_VIGNETTE != 0;
    let mask = if vign { vignette_mask() } else { &[] };

    for y in 0..FB_HEIGHT {
        let f_base = if scan && (y & 1) == 1 { 72 } else { 100 };
        let row_off = y * FB_STRIDE;
        let mrow = y * FB_WIDTH;
        for x in 0..FB_WIDTH {
            let mut f = f_base;
            if vign {
                f = f * mask[mrow + x] as u32 / 255;
            }
            if f >= 100 {
                continue;
            }
            let off = row_off + x * 4;
            fb.buffer[off] = (fb.buffer[off] as u32 * f / 100) as u8;
            fb.buffer[off + 1] = (fb.buffer[off + 1] as u32 * f / 100) as u8;
            fb.buffer[off + 2] = (fb.buffer[off + 2] as u32 * f / 100) as u8;
        }
    }
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
        fb.draw_text(content_x, content_y, empty_msg, theme().text_dim, 1);
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
    draw_card(fb, MARGIN, 56, HALF_W, 244, Some(ICON_CHIP), "KURURU ARCHITECTURE & COMPUTE", theme().warn);

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
        fb.draw_text(28, ty, label, theme().text_muted, 1);
        fb.draw_text(165, ty, val, theme().text, 1);
        ty += ROW_H;
    }

    // ── Card B: On-board battery (UPS) & Wi-Fi radio ────────────────────────
    draw_card(fb, COL_B_X, 56, HALF_W, 244, Some(ICON_WIFI), "BATTERY (UPS) & WI-FI RADIO", theme().warn);

    fb.draw_text(532, 92, "Battery:", theme().text_muted, 1);
    fb.draw_rect(628, 90, 200, 16, theme().border);
    let fill_w = (info.battery_pct_num as usize * 196) / 100;
    fb.draw_rect(630, 92, fill_w, 12, battery_color(info.battery_pct_num));
    let bat_label = format!("{}% ({})", info.battery_pct, info.battery_status);
    fb.draw_text(840, 92, &bat_label, theme().text, 1);

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
        fb.draw_text(532, ry, label, theme().text_muted, 1);
        fb.draw_text(676, ry, val, theme().text, 1);
        ry += ROW_H;
    }

    // ── Card C: Live kernel log console ─────────────────────────────────────
    draw_log_console(
        fb, &info.logs, &info.time_str,
        MARGIN, 308, CONTENT_W, 252,
        "KERNEL LOG CONSOLE (DMESG TAIL)", ICON_TERMINAL,
        theme().accent, theme().text_muted,
        "Waiting for kernel messages...",
    );
}

// -------------------------------------------------------------
// TAB 1: HOMELAB & WOL (Cluster Telemetry & Wake-on-LAN)
// -------------------------------------------------------------
fn render_tab_homelab(fb: &mut Framebuffer, info: &SystemInfo) {
    // ── Card A: Tailnet cluster & core servers ──────────────────────────────
    let cluster_title = format!("MNEMOCINE TAILNET ({}/5 ONLINE)", info.homelab_online_count);
    draw_card(fb, MARGIN, 56, HALF_W, 244, Some(ICON_SERVER), &cluster_title, theme().warn);

    fb.draw_text(28, 96, "Tailnet:", theme().text_muted, 1);
    fb.draw_text(130, 96, &info.tailnet_suffix, theme().info, 1);

    fb.draw_text(28, 115, "Kururu IP:", theme().text_muted, 1);
    fb.draw_text(130, 115, &info.tailscale_ip, theme().text, 1);

    fb.draw_text(28, 134, "Direct Link:", theme().text_muted, 1);
    fb.draw_text(130, 134, &info.active_link_str, theme().accent, 1);

    // Server list with pixel status squares (active / online / offline)
    let mut py = 162;
    for peer in &info.homelab_nodes {
        let color = if peer.active {
            theme().accent
        } else if peer.online {
            theme().info
        } else {
            theme().border
        };
        fb.draw_rect(28, py + 4, 8, 8, color);
        fb.draw_text(44, py, &peer.name, if peer.online { theme().text } else { theme().text_dim }, 1);
        fb.draw_text(145, py, &peer.ip, theme().text_muted, 1);

        let status_desc = if peer.active {
            "direct"
        } else if peer.online {
            "online"
        } else {
            "offline"
        };
        fb.draw_text(295, py, status_desc, if peer.online { color } else { theme().text_dim }, 1);

        py += ROW_H;
    }

    let summary_line = format!("Mesh Total: {} nodes registered in Tailnet", info.peers_total_count);
    fb.draw_text(28, 278, &summary_line, theme().text_dim, 1);

    // ── Card B: Wake-on-LAN controller & relay targets ──────────────────────
    draw_card(fb, COL_B_X, 56, HALF_W, 244, Some(ICON_POWER), "WAKE-ON-LAN CONTROLLER", theme().warn);

    let daemon_label = if info.wol_daemon_running {
        "ACTIVE (:9096) - Kururu Native Rust"
    } else {
        "STOPPED"
    };
    let daemon_color = if info.wol_daemon_running { theme().accent } else { theme().error };

    let wol_lines = [
        ("Daemon Status:", daemon_label, daemon_color),
        ("Broadcast Target:", "192.168.3.255:9 (mlan0 direct AP)", theme().text),
        ("Target 1 (Host):", "Psicopompo (Workstation & Gaming)", theme().text),
        ("Target 1 (MAC):", info.wol_target1_mac.as_str(), theme().warn),
        ("Target 1 (URI):", "http://kururu:9096/wake/psicopompo", theme().info),
        ("Target 2 (Host):", "Kavure (Services & Microserver)", theme().text),
        ("Target 2 (MAC):", info.wol_target2_mac.as_str(), theme().warn),
        ("Target 2 (URI):", "http://kururu:9096/wake/kavure", theme().info),
        ("Transmission:", "Layer 2 Magic Packet Burst (5x / 25ms)", theme().text_muted),
    ];

    let mut wy = 96;
    for (label, val, col) in &wol_lines {
        fb.draw_text(532, wy, label, theme().text_muted, 1);
        fb.draw_text(676, wy, val, *col, 1);
        wy += ROW_H;
    }

    // ── Card C: WoL dispatch audit log ──────────────────────────────────────
    draw_log_console(
        fb, &info.wol_logs, &info.time_str,
        MARGIN, 308, CONTENT_W, 252,
        "WAKE-ON-LAN DISPATCH AUDIT LOG", ICON_TERMINAL,
        theme().info, theme().info,
        "No Wake-on-LAN packets dispatched yet. Waiting on port 9096...",
    );
}

// -------------------------------------------------------------
// TAB 2: RETRO DESK CLOCK & AMBIENT STATION
// -------------------------------------------------------------
fn render_tab_clock(fb: &mut Framebuffer, info: &SystemInfo) {
    // ── Hero: ambient retro clock ───────────────────────────────────────────
    draw_card(fb, MARGIN, 56, CONTENT_W, 294, Some(ICON_CLOCK),
              "MNEMOCINE TIME STATION - SOVEREIGN NTP CLOCK", theme().warn);

    // Huge clock: font 8x16 at scale 5 = 40x80 px/char; 8 chars = 320px wide.
    fb.draw_text(352, 100, &info.time_str, theme().accent, 5);

    // Date banner (scale 2, centered)
    let date_x = (FB_WIDTH.saturating_sub(info.date_full_str.len() * 16)) / 2;
    fb.draw_text(date_x, 196, &info.date_full_str, theme().info, 2);

    // Sub-banner + status pill (centered)
    let sub = format!("Timezone: America/Sao_Paulo (UTC-3) | Host: {} | Uptime: {}", info.hostname, info.uptime_str);
    fb.draw_text((FB_WIDTH.saturating_sub(sub.len() * 8)) / 2, 250, &sub, theme().text_muted, 1);

    let pill = format!(
        "Homelab: {}/5 Servers Online   |   Wi-Fi: {} ({})   |   WOL Relay: Port 9096 Ready",
        info.homelab_online_count, info.wifi_ssid, info.wifi_signal_dbm
    );
    fb.draw_text((FB_WIDTH.saturating_sub(pill.len() * 8)) / 2, 286, &pill, theme().text_dim, 1);

    // ── Ambient ribbon: 3 symmetric vitals cards (320px each) ──────────────
    // Card 1: Hardware UPS / no-break
    draw_card(fb, MARGIN, 358, RIBBON_W, 202, Some(ICON_BATTERY), "HARDWARE UPS / NO-BREAK", theme().warn);

    fb.draw_rect(28, 394, 292, 16, theme().border);
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
        fb.draw_text(28, uy, label, theme().text_muted, 1);
        fb.draw_text(115, uy, val, theme().text, 1);
        uy += ROW_H;
    }

    // Card 2: Kururu node vitals
    draw_card(fb, RIBBON_2_X, 358, RIBBON_W, 202, Some(ICON_CHIP), "KURURU NODE VITALS", theme().warn);

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
        fb.draw_text(364, vy, label, theme().text_muted, 1);
        fb.draw_text(440, vy, val, theme().text, 1);
        vy += ROW_H;
    }

    // Card 3: Homelab network
    draw_card(fb, RIBBON_3_X, 358, RIBBON_W, 202, Some(ICON_GLOBE), "HOMELAB NETWORK", theme().warn);

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
        fb.draw_text(700, ny, label, theme().text_muted, 1);
        fb.draw_text(796, ny, val, theme().text, 1);
        ny += ROW_H;
    }
}

fn main() {
    println!("[Kururu Display Daemon] Starting v2.4 (terminal with alacritty_terminal)...");

    // Optional initial dashboard: `kururu-display 2` (kiosk/debug). Default 0.
    let initial_tab = std::env::args()
        .nth(1)
        .and_then(|a| a.parse::<usize>().ok())
        .map(|t| t.min(TAB_COUNT - 1))
        .unwrap_or(0);

    // Optional initial screen: `kururu-display <tab> <screen>` (kiosk/debug).
    let initial_screen = std::env::args()
        .nth(2)
        .and_then(|a| a.parse::<usize>().ok())
        .map(|s| s.min(SCREEN_TERMINAL))
        .unwrap_or(SCREEN_DASHBOARD);

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

    // Theme + CRT effects (env overrides config file).
    load_display_config();

    let screen_active = Arc::new(AtomicBool::new(true));
    let screen_active_power = screen_active.clone();
    let screen_active_keys = screen_active.clone();

    let current_tab = Arc::new(AtomicUsize::new(initial_tab));
    let current_tab_keys = current_tab.clone();

    let current_screen = Arc::new(AtomicUsize::new(initial_screen));
    let current_screen_keys = current_screen.clone();

    let menu_cursor = Arc::new(AtomicUsize::new(0));
    let menu_cursor_keys = menu_cursor.clone();

    let wake_signal = Arc::new(AtomicBool::new(true));
    let wake_signal_power = wake_signal.clone();
    let wake_signal_keys = wake_signal.clone();

    // Terminal: PTY + shell feeding the alacritty_terminal emulator.
    if let Err(e) = terminal::init(TERM_COLS, TERM_ROWS, wake_signal.clone()) {
        eprintln!("[Kururu Display] Terminal indisponivel: {}", e);
    }

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

    // Listener: hardware Volume Up / Down + Home keys (/dev/input/event0).
    // Screen-aware: on the Dashboard, VOL± switch tabs and HOME opens the menu;
    // elsewhere VOL± move the cursor and HOME selects / goes back.
    spawn_input_listener(EVENT_KEYS, move |event| {
        // Only respond on key down (value == 1)
        if event.type_ == EV_KEY && event.value == 1 {
            let was_asleep = !screen_active_keys.load(Ordering::SeqCst);
            if was_asleep {
                println!("[Kururu Display] Key pressed while asleep -> Waking display up");
                set_display_hardware(true);
                screen_active_keys.store(true, Ordering::SeqCst);
            }
            wake_signal_keys.store(true, Ordering::SeqCst);

            let screen = current_screen_keys.load(Ordering::SeqCst);
            match event.code {
                KEY_VOLUMEUP | KEY_VOLUMEDOWN => {
                    let up = event.code == KEY_VOLUMEUP;
                    if screen == SCREEN_DASHBOARD {
                        let cur = current_tab_keys.load(Ordering::SeqCst);
                        let next = if up {
                            (cur + TAB_COUNT - 1) % TAB_COUNT
                        } else {
                            (cur + 1) % TAB_COUNT
                        };
                        current_tab_keys.store(next, Ordering::SeqCst);
                    } else if screen == SCREEN_MENU {
                        let cur = menu_cursor_keys.load(Ordering::SeqCst);
                        let next = if up {
                            (cur + MENU_COUNT - 1) % MENU_COUNT
                        } else {
                            (cur + 1) % MENU_COUNT
                        };
                        menu_cursor_keys.store(next, Ordering::SeqCst);
                    } else if screen == SCREEN_SETTINGS {
                        let editing = wifi_form().lock().map(|f| f.editing).unwrap_or(false);
                        if !editing {
                            if let Ok(mut f) = wifi_form().lock() {
                                f.focus = if up { (f.focus + 2) % 3 } else { (f.focus + 1) % 3 };
                            }
                        }
                    } else if screen == SCREEN_TERMINAL {
                        terminal::scroll(if up { 3 } else { -3 });
                    }
                }
                KEY_HOMEPAGE => {
                    if screen == SCREEN_SETTINGS {
                        let editing = wifi_form().lock().map(|f| f.editing).unwrap_or(false);
                        if editing {
                            if let Ok(mut f) = wifi_form().lock() {
                                f.editing = false;
                            }
                        } else {
                            let focus = wifi_form().lock().map(|f| f.focus).unwrap_or(0);
                            if focus == 2 {
                                wifi_submit();
                            } else if let Ok(mut f) = wifi_form().lock() {
                                f.editing = true;
                            }
                        }
                    } else {
                        nav_activate(&current_screen_keys, &menu_cursor_keys);
                        println!(
                            "[Kururu Display] HOME: screen {} -> {}",
                            screen,
                            current_screen_keys.load(Ordering::SeqCst)
                        );
                    }
                }
                _ => {}
            }
        }
    });

    // Listener: touchscreen (sec_touchscreen on /dev/input/event1).
    // A tap wakes the screen; a tap on a header tab switches dashboard.
    let touch_active = screen_active.clone();
    let touch_wake = wake_signal.clone();
    let touch_tab = current_tab.clone();
    let touch_screen = current_screen.clone();
    let touch_menu = menu_cursor.clone();
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
                            handle_tap(touch_x, touch_y, &touch_cal, &touch_active, &touch_wake, &touch_tab, &touch_screen, &touch_menu);
                        }
                    } else {
                        touch_down = true;
                    }
                }
                _ => {}
            },
            EV_KEY => match event.code {
                BTN_TOUCH => {
                    if event.value == 1 {
                        touch_down = true;
                    } else if event.value == 0 && touch_down {
                        touch_down = false;
                        handle_tap(touch_x, touch_y, &touch_cal, &touch_active, &touch_wake, &touch_tab, &touch_screen, &touch_menu);
                    }
                }
                KEY_MENU | KEY_BACK => {
                    if event.value == 1 {
                        if !touch_active.load(Ordering::SeqCst) {
                            set_display_hardware(true);
                            touch_active.store(true, Ordering::SeqCst);
                        }
                        touch_wake.store(true, Ordering::SeqCst);
                        if event.code == KEY_MENU {
                            println!("[Kururu Display] MENU touchkey");
                            nav_activate(&touch_screen, &touch_menu);
                        } else {
                            println!("[Kururu Display] BACK touchkey");
                            let editing = touch_screen.load(Ordering::SeqCst) == SCREEN_SETTINGS
                                && wifi_form().lock().map(|f| f.editing).unwrap_or(false);
                            if editing {
                                if let Ok(mut f) = wifi_form().lock() {
                                    f.editing = false;
                                }
                            } else {
                                nav_back(&touch_screen);
                            }
                        }
                    }
                }
                _ => {}
            },
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

    // Telemetry cache: cheap fields refresh every FAST_SECS (so the clock ticks
    // second-by-second); subprocess-heavy fields every SLOW_SECS.
    let mut cached_info: Option<SystemInfo> = None;
    let mut last_fast = Instant::now()
        .checked_sub(Duration::from_secs(10))
        .unwrap_or_else(Instant::now);
    let mut last_slow = last_fast;

    const TICK_MS: u64 = 50;          // Poll interval: 50ms max button latency
    const FAST_SECS: u64 = 1;         // Cheap telemetry + render cadence
    const SLOW_SECS: u64 = 5;         // Subprocess-heavy telemetry cadence

    loop {
        let is_active = screen_active.load(Ordering::SeqCst);

        if is_active {
            let got_wake = wake_signal.swap(false, Ordering::SeqCst);

            if !was_active {
                // Just woke up — reset activity timer and force a full refresh
                last_awake_time = Instant::now();
                last_fast = last_awake_time
                    .checked_sub(Duration::from_secs(10))
                    .unwrap_or(last_awake_time);
                last_slow = last_fast;
                was_active = true;
            }

            if got_wake {
                // Button pressed — reset inactivity timer
                last_awake_time = Instant::now();
            }

            // Check auto-sleep timeout (0 = never)
            let sleep_secs = SLEEP_SECS.load(Ordering::Relaxed) as u64;
            if sleep_secs > 0 && last_awake_time.elapsed() > Duration::from_secs(sleep_secs) {
                println!("[Kururu Display] Inactivity timeout ({}s) -> Sleeping display", sleep_secs);
                set_display_hardware(false);
                screen_active.store(false, Ordering::SeqCst);
                was_active = false;
                cached_info = None;
                thread::sleep(Duration::from_millis(200));
                continue;
            }

            // Cheap refresh (and render) every FAST_SECS; heavy telemetry every
            // SLOW_SECS. A wake forces both. Rendering each fast tick keeps the
            // retro clock's seconds live.
            let fast_due = cached_info.is_none()
                || last_fast.elapsed() >= Duration::from_secs(FAST_SECS);
            let slow_due = cached_info.is_none()
                || last_slow.elapsed() >= Duration::from_secs(SLOW_SECS);

            let should_render = got_wake || fast_due;

            if fast_due {
                if cached_info.is_none() {
                    cached_info = Some(SystemInfo::default());
                }
                if let Some(info) = cached_info.as_mut() {
                    gather_fast(info);
                    last_fast = Instant::now();
                    if slow_due {
                        gather_slow(info);
                        last_slow = Instant::now();
                    }
                }
            }

            if should_render {
                if let Some(ref info) = cached_info {
                    let screen = current_screen.load(Ordering::SeqCst);

                    fb.clear(theme().bg);
                    draw_background_grid(&mut fb);

                    match screen {
                        SCREEN_MENU => render_menu(&mut fb, menu_cursor.load(Ordering::SeqCst)),
                        SCREEN_ABOUT => render_about(&mut fb, info),
                        SCREEN_SETTINGS => render_settings(&mut fb),
                        SCREEN_TERMINAL => render_terminal(&mut fb),
                        _ => {
                            let tab = current_tab.load(Ordering::SeqCst);
                            draw_header(&mut fb, tab);
                            match tab {
                                0 => render_tab_kururu(&mut fb, info),
                                1 => render_tab_homelab(&mut fb, info),
                                _ => render_tab_clock(&mut fb, info),
                            }
                            draw_footer(&mut fb, info);
                        }
                    }

                    apply_crt_effects(&mut fb);
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
