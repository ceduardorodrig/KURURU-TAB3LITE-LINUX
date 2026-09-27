//! App shell screens (Menu / About / Settings / Wi-Fi form / Terminal).
//!
//! Rendered entirely with the shared pixel design system in [`crate::kit`],
//! so the shell and the dashboards share one visual identity. Every screen
//! exposes the rectangles it draws through the same constants used by the
//! hit-tests below.

use crate::kit::{self, CONTENT_W, MARGIN};
use crate::{theme, Framebuffer, MenuAction, EFFECTS, MENU_ITEMS};

// ── Menu ───────────────────────────────────────────────────────────────
const MENU_CARD_Y: usize = 132;
const MENU_LIST_Y: usize = 172;
const MENU_PITCH: usize = 64;
const MENU_ROW_H: usize = 56;
const MENU_ROW_X: usize = 28;
const MENU_ROW_W: usize = CONTENT_W - 2 * kit::PAD;

pub fn menu(fb: &mut Framebuffer, cursor: usize) {
    let t = theme();
    kit::topbar(fb, "MENU", "[HOME] Select");
    kit::footer(
        fb,
        &[("[HOME] Select", t.accent), ("[VOL] Navigate", t.info), ("[BACK] Back", t.text_muted)],
        &[],
    );

    // Brand: big pixel frog beside the wordmark.
    let scale = 4;
    let (fw, _fh) = kit::frog_size(scale);
    let word = "KURURU";
    let word_w = word.len() * 24;
    let total = fw + 16 + word_w;
    let x0 = (crate::FB_WIDTH - total) / 2;
    kit::frog(fb, x0, 68, scale);
    fb.draw_text(x0 + fw + 16, 72, word, t.accent, 3);

    kit::card(fb, MARGIN, MENU_CARD_Y, CONTENT_W, kit::CONTENT_BOTTOM - MENU_CARD_Y, None, "NAVIGATION", t.accent);

    for (i, (label, action)) in MENU_ITEMS.iter().enumerate() {
        let y = MENU_LIST_Y + i * MENU_PITCH;
        let selected = i == cursor;
        kit::row_bg(fb, MENU_ROW_X, y, MENU_ROW_W, MENU_ROW_H, selected);
        if selected {
            fb.draw_text(MENU_ROW_X + 10, y + 12, ">", t.accent, 2);
        }
        fb.draw_text(MENU_ROW_X + 40, y + 12, label, if selected { t.accent } else { t.text }, 2);
        if let Some(value) = menu_value(*action) {
            let vw = value.len() * 16;
            fb.draw_text(MENU_ROW_X + MENU_ROW_W - 24 - vw, y + 12, &value, t.text_muted, 2);
        }
    }
}

fn menu_value(action: MenuAction) -> Option<String> {
    match action {
        MenuAction::ToggleTheme => Some(theme().name.to_string()),
        MenuAction::ToggleEffects => {
            let on = EFFECTS.load(std::sync::atomic::Ordering::Relaxed) != 0;
            Some(if on { "ON".to_string() } else { "OFF".to_string() })
        }
        MenuAction::Screen(_) => None,
    }
}

/// Map a touch position to a Menu item index (same rows the renderer draws).
pub fn menu_hit(px: usize, py: usize) -> Option<usize> {
    if px < MENU_ROW_X || px >= MENU_ROW_X + MENU_ROW_W || py < MENU_LIST_Y {
        return None;
    }
    let idx = (py - MENU_LIST_Y) / MENU_PITCH;
    if (py - MENU_LIST_Y) % MENU_PITCH < MENU_ROW_H && idx < MENU_ITEMS.len() {
        Some(idx)
    } else {
        None
    }
}

// ── About ──────────────────────────────────────────────────────────────
pub fn about(fb: &mut Framebuffer, lines: &[String]) {
    let t = theme();
    kit::topbar(fb, "ABOUT", "[BACK] Back");
    kit::footer(fb, &[("[BACK] Back", t.accent), ("[HOME] Menu", t.text_muted)], &[("v3.0", t.text_dim)]);
    kit::card(fb, MARGIN, kit::CONTENT_Y, CONTENT_W, kit::CONTENT_H, None, "KURURU NODE", t.accent);

    let scale = 4;
    let (fw, fh) = kit::frog_size(scale);
    kit::frog(fb, (crate::FB_WIDTH - fw) / 2, 116, scale);
    let mut y = 116 + fh + 28;
    for line in lines {
        let w = line.len() * 16;
        fb.draw_text((crate::FB_WIDTH - w) / 2, y, line, t.text_muted, 2);
        y += 40;
    }
}

// ── Settings ───────────────────────────────────────────────────────────
// Left column: DISPLAY & AUDIO. Right column: WI-FI.
const SET_LEFT_Y: usize = kit::CONTENT_Y;
const SET_LEFT_H: usize = kit::CONTENT_H;
const SET_WIFI_X: usize = kit::COL_B_X;
const SET_INNER_X: usize = MARGIN + kit::PAD; // 28
const SET_INNER_W: usize = kit::HALF_W - 2 * kit::PAD; // 464
const SET_BRIGHT_ZONE: (usize, usize, usize, usize) = (28, 102, 464, 80);
const SET_SLEEP_DOWN: (usize, usize, usize, usize) = (344, 190, 64, 48);
const SET_SLEEP_UP: (usize, usize, usize, usize) = (416, 190, 64, 48);
const SET_VOL_ZONE: (usize, usize, usize, usize) = (28, 250, 464, 80);
const SET_EFFECTS_BTN: (usize, usize, usize, usize) = (200, 338, 160, 48);
const SET_LIST_X: usize = 532;
const SET_LIST_Y: usize = 150;
const SET_LIST_W: usize = 464;
const SET_LIST_PITCH: usize = 60;
const SET_LIST_H: usize = 56;
const SET_LIST_ROWS: usize = 5;

fn effects_on() -> bool {
    EFFECTS.load(std::sync::atomic::Ordering::Relaxed) != 0
}

pub fn settings(fb: &mut Framebuffer) {
    let t = theme();
    kit::topbar(fb, "SETTINGS", "[HOME] Rescan");
    kit::footer(
        fb,
        &[("[HOME] Rescan", t.accent), ("[VOL] Navigate", t.info), ("[BACK] Back", t.text_muted)],
        &[],
    );

    // ── DISPLAY & AUDIO (left column) ──────────────────────────────────
    kit::card(fb, MARGIN, SET_LEFT_Y, kit::HALF_W, SET_LEFT_H, None, "DISPLAY & AUDIO", t.accent);

    let br = crate::brightness_pct();
    label_value(fb, SET_INNER_X, 106, SET_INNER_W, "Brightness", &format!("{}%", (br * 100.0) as u32), t);
    kit::bar(fb, SET_INNER_X, 146, SET_INNER_W, 28, br, t.accent);

    fb.draw_text(SET_INNER_X, 196, "Auto-sleep", t.text, 2);
    fb.draw_text(260, 196, &crate::sleep_label(), t.text_muted, 2);
    button(fb, SET_SLEEP_DOWN.0, SET_SLEEP_DOWN.1, SET_SLEEP_DOWN.2, SET_SLEEP_DOWN.3, "[-]", false);
    button(fb, SET_SLEEP_UP.0, SET_SLEEP_UP.1, SET_SLEEP_UP.2, SET_SLEEP_UP.3, "[+]", false);

    let vol = crate::volume_pct();
    label_value(fb, SET_INNER_X, 254, SET_INNER_W, "Volume", &format!("{}%", (vol * 100.0) as u32), t);
    kit::bar(fb, SET_INNER_X, 294, SET_INNER_W, 28, vol, t.accent);

    fb.draw_text(SET_INNER_X, 344, "Effects", t.text, 2);
    button(fb, SET_EFFECTS_BTN.0, SET_EFFECTS_BTN.1, SET_EFFECTS_BTN.2, SET_EFFECTS_BTN.3,
           if effects_on() { "[ ON ]" } else { "[ OFF ]" }, effects_on());

    // ── WI-FI (right column) ───────────────────────────────────────────
    kit::card(fb, SET_WIFI_X, kit::CONTENT_Y, kit::HALF_W, kit::CONTENT_H, None, "WI-FI", t.accent);
    let conn = crate::wifi_connected();
    if !conn.is_empty() {
        let sig = crate::wifi_signal();
        let txt = if sig.is_empty() { format!("Connected: {conn}") } else { format!("Connected: {conn} ({sig})") };
        fb.draw_text(SET_LIST_X, 106, &txt, t.accent, 1);
    }

    let nets = crate::wifi_nets();
    let sel = crate::wifi_sel();
    if nets.is_empty() {
        fb.draw_text(SET_LIST_X, 150, "(scanning networks...)", t.text_dim, 2);
    } else {
        let start = wifi_start(nets.len(), sel);
        for (row, (name, signal, _)) in nets.iter().skip(start).take(SET_LIST_ROWS).enumerate() {
            let y = SET_LIST_Y + row * SET_LIST_PITCH;
            let selected = start + row == sel;
            wifi_row(fb, y, name, *signal, name == &conn, selected);
        }
    }
    let status = crate::wifi_status();
    if !status.is_empty() {
        fb.draw_text(SET_LIST_X, 476, &status, t.info, 1);
    }
}

fn wifi_row(fb: &mut Framebuffer, y: usize, name: &str, signal: i32, connected: bool, selected: bool) {
    let t = theme();
    kit::row_bg(fb, SET_LIST_X, y, SET_LIST_W, SET_LIST_H, selected);
    let mut nx = SET_LIST_X + 12;
    if connected {
        fb.draw_rect(SET_LIST_X + 12, y + 24, 8, 8, t.accent);
    }
    nx += 16;
    if selected {
        fb.draw_text(nx, y + 12, ">", t.accent, 2);
        nx += 20;
    }
    fb.draw_text(nx, y + 12, name, if selected { t.accent } else { t.text }, 2);
    let sig = format!("{signal} dBm");
    let sw = sig.len() * 8;
    fb.draw_text(SET_LIST_X + SET_LIST_W - 12 - sw, y + 20, &sig, t.text_muted, 1);
}

/// Left/right split touch zones for a two-direction control.
fn split_hit(rect: (usize, usize, usize, usize), px: usize) -> bool {
    px < rect.0 + rect.2 / 2
}

pub fn settings_hit(px: usize, py: usize) -> Option<SettingsHit> {
    if inside(SET_BRIGHT_ZONE, px, py) {
        return Some(if split_hit(SET_BRIGHT_ZONE, px) { SettingsHit::BrightDown } else { SettingsHit::BrightUp });
    }
    if inside(SET_SLEEP_DOWN, px, py) {
        return Some(SettingsHit::SleepPrev);
    }
    if inside(SET_SLEEP_UP, px, py) {
        return Some(SettingsHit::SleepNext);
    }
    if inside(SET_VOL_ZONE, px, py) {
        return Some(if split_hit(SET_VOL_ZONE, px) { SettingsHit::VolDown } else { SettingsHit::VolUp });
    }
    if inside(SET_EFFECTS_BTN, px, py) {
        return Some(SettingsHit::Effects);
    }
    let nets = crate::wifi_nets();
    if !nets.is_empty() {
        for row in 0..SET_LIST_ROWS {
            let r = (SET_LIST_X, SET_LIST_Y + row * SET_LIST_PITCH, SET_LIST_W, SET_LIST_H);
            if inside(r, px, py) {
                return Some(SettingsHit::Network(row));
            }
        }
    }
    None
}

// ── Wi-Fi connect form (shown while editing) ───────────────────────────
const FORM_Y: usize = kit::CONTENT_Y;
const FORM_H: usize = 166; // 66..232, keyboard starts at 240
const FORM_FIELD: (usize, usize, usize, usize) = (28, 140, 508, 48);
const FORM_REVEAL: (usize, usize, usize, usize) = (544, 140, 120, 48);
const FORM_CONNECT: (usize, usize, usize, usize) = (672, 140, 304, 48);

pub fn settings_form(fb: &mut Framebuffer) {
    let t = theme();
    kit::topbar(fb, "WI-FI", "[BACK] Cancel");
    kit::footer(fb, &[("[HOME] Connect", t.accent), ("[BACK] Cancel", t.text_muted)], &[]);
    kit::card(fb, MARGIN, FORM_Y, CONTENT_W, FORM_H, None, "CONNECT TO NETWORK", t.accent);

    let ssid = crate::wifi_selected_ssid();
    let connected = !ssid.is_empty() && ssid == crate::wifi_connected();
    let txt = if connected { format!("Network: {ssid}  (connected)") } else { format!("Network: {ssid}") };
    fb.draw_text(SET_INNER_X + 4, 100, &txt, t.text, 2);

    // Password field (always visible above the keyboard).
    let (fx, fy, fw, fh) = FORM_FIELD;
    fb.draw_rect(fx, fy, fw, fh, t.bg);
    fb.draw_rect(fx, fy, fw, 1, t.border);
    fb.draw_rect(fx, fy + fh - 1, fw, 1, t.border);
    fb.draw_rect(fx, fy, 1, fh, t.border);
    fb.draw_rect(fx + fw - 1, fy, 1, fh, t.border);
    fb.draw_text(fx + 12, fy + 16, "Password:", t.text_muted, 1);
    let value = crate::wifi_psk_display();
    let vx = fx + 110;
    fb.draw_text(vx, fy + 8, &value, t.text, 2);
    if crate::wifi_reveal() {
        fb.draw_text(vx + value.len() * 16 + 4, fy + 8, "_", t.accent, 2);
    } else {
        fb.draw_rect(vx + value.len() * 16 + 4, fy + 36, 12, 3, t.accent);
    }

    button(fb, FORM_REVEAL.0, FORM_REVEAL.1, FORM_REVEAL.2, FORM_REVEAL.3,
           if crate::wifi_reveal() { "[ hide ]" } else { "[ show ]" }, false);
    button(fb, FORM_CONNECT.0, FORM_CONNECT.1, FORM_CONNECT.2, FORM_CONNECT.3, "[ CONNECT ]", true);

    let status = crate::wifi_status();
    if !status.is_empty() {
        fb.draw_text(SET_INNER_X + 4, 196, &status, t.info, 1);
    }
}

pub fn settings_form_hit(px: usize, py: usize) -> Option<SettingsHit> {
    if inside(FORM_REVEAL, px, py) {
        return Some(SettingsHit::Reveal);
    }
    if inside(FORM_CONNECT, px, py) {
        return Some(SettingsHit::Connect);
    }
    if inside(FORM_FIELD, px, py) {
        return Some(SettingsHit::Password);
    }
    None
}

// ── Terminal ───────────────────────────────────────────────────────────
pub fn terminal(fb: &mut Framebuffer) {
    let t = theme();
    kit::topbar(fb, "TERMINAL", "[VOL] Scroll");
    kit::footer(fb, &[("[BACK] Back", t.accent), ("[HOME] Enter", t.info)], &[]);
}

// ── Helpers ────────────────────────────────────────────────────────────
fn label_value(fb: &mut Framebuffer, x: usize, y: usize, w: usize, label: &str, value: &str, t: &'static crate::Theme) {
    fb.draw_text(x, y, label, t.text, 2);
    let vw = value.len() * 16;
    fb.draw_text(x + w - vw, y, value, t.text_muted, 2);
}

fn button(fb: &mut Framebuffer, x: usize, y: usize, w: usize, h: usize, label: &str, primary: bool) {
    let t = theme();
    let (bg, fg) = if primary { (t.accent, t.bg) } else { (t.panel_active, t.text) };
    fb.draw_rect(x, y, w, h, bg);
    let lw = label.len() * 16;
    fb.draw_text(x + w.saturating_sub(lw) / 2, y + (h.saturating_sub(32)) / 2, label, fg, 2);
}

fn inside(r: (usize, usize, usize, usize), px: usize, py: usize) -> bool {
    px >= r.0 && px < r.0 + r.2 && py >= r.1 && py < r.1 + r.3
}

pub fn wifi_start(nets_len: usize, sel: usize) -> usize {
    let n = SET_LIST_ROWS.min(nets_len);
    if n == 0 {
        0
    } else {
        sel.saturating_sub(1).min(nets_len - n)
    }
}

pub enum SettingsHit {
    BrightUp,
    BrightDown,
    VolUp,
    VolDown,
    SleepPrev,
    SleepNext,
    Effects,
    Network(usize),
    Password,
    Reveal,
    Connect,
}
