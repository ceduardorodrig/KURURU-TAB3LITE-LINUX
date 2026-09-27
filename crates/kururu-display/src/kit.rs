//! Pixel design system shared by the dashboards and the app shell.
//!
//! Everything (topbar, footer, cards, buttons, lists, the on-screen keyboard)
//! is drawn with the same primitives and the same layout tokens, so the whole
//! UI shares one visual identity. Each screen exposes a layout function that
//! returns the exact rectangles it draws, and the touch hit-testing reuses
//! those rectangles — draw and input can never drift apart.

use std::sync::Mutex;
use std::time::Instant;

use crate::{
    theme, Color, Framebuffer, Key, KB_ROWS, FB_HEIGHT, FB_WIDTH, SP_FROG_BODY, SP_FROG_DETAIL,
};

// ── Layout tokens ──────────────────────────────────────────────────────
/// Outer screen margin (also used as the left/right gutter of the keyboard).
pub const MARGIN: usize = 16;
/// Gap between adjacent cards/rows (vertical **and** horizontal).
pub const GUTTER: usize = 16;
/// Inner padding of a card.
pub const PAD: usize = 12;
/// Card title band height.
pub const TITLE_H: usize = 28;
/// Top bar panel height (matches the dashboard header).
pub const TOPBAR_H: usize = 48;
/// Divider under the top bar.
pub const TOPBAR_BORDER: usize = 2;
/// Bottom hint bar height.
pub const FOOTER_H: usize = 32;

/// Top of the content area (below the top bar + one gutter).
pub const CONTENT_Y: usize = TOPBAR_H + TOPBAR_BORDER + GUTTER; // 66
pub const CONTENT_W: usize = FB_WIDTH - 2 * MARGIN; // 992
/// Bottom of the content area (above the footer + one gutter).
pub const CONTENT_BOTTOM: usize = FB_HEIGHT - FOOTER_H - GUTTER; // 552
pub const CONTENT_H: usize = CONTENT_BOTTOM - CONTENT_Y; // 486
/// Two-column geometry (dashboards + Settings).
pub const HALF_W: usize = (CONTENT_W - GUTTER) / 2; // 488
pub const COL_B_X: usize = MARGIN + HALF_W + GUTTER; // 520
/// Two stacked cards in one column.
pub const CARD_H: usize = (CONTENT_H - GUTTER) / 2; // 235
pub const ROW2_Y: usize = CONTENT_Y + CARD_H + GUTTER; // 317
/// Three equal ribbons (Retro Clock tab).
pub const RIBBON_W: usize = (CONTENT_W - 2 * GUTTER) / 3; // 320
pub const RIBBON_2_X: usize = MARGIN + RIBBON_W + GUTTER; // 352
pub const RIBBON_3_X: usize = MARGIN + 2 * (RIBBON_W + GUTTER); // 688

// ── On-screen keyboard geometry ────────────────────────────────────────
pub const KB_ROWS_N: usize = 5;
pub const KB_KEY_H: usize = 56;
pub const KB_GAP: usize = 8;
/// One width unit in pixels (16 units fill the content width).
pub const KB_UNIT: usize = 62;
pub const KB_UNITS: usize = 16;
/// Left edge that centres the 16-unit keyboard inside the content width.
pub const KB_X0: usize = MARGIN + (CONTENT_W - (KB_UNITS * KB_UNIT - KB_GAP)) / 2; // 20
/// Top of the keyboard (sits flush above the content bottom).
pub const KB_TOP: usize = CONTENT_BOTTOM - (KB_ROWS_N * KB_KEY_H + (KB_ROWS_N - 1) * KB_GAP); // 240

/// Pixel rectangle `(x, y, w, h)` of a key starting at `unit` spanning `units`.
pub fn key_rect(unit: usize, units: usize, row: usize) -> (usize, usize, usize, usize) {
    (
        KB_X0 + unit * KB_UNIT,
        KB_TOP + row * (KB_KEY_H + KB_GAP),
        units * KB_UNIT - KB_GAP,
        KB_KEY_H,
    )
}

// ── Shared widgets ─────────────────────────────────────────────────────
/// Top bar: accent marker + title (scale 2) + optional right-aligned hint,
/// with a divider underneath. Shared by every screen.
pub fn topbar(fb: &mut Framebuffer, title: &str, hint: &str) {
    let t = theme();
    fb.draw_rect(0, 0, FB_WIDTH, TOPBAR_H, t.panel);
    fb.draw_rect(0, TOPBAR_H, FB_WIDTH, TOPBAR_BORDER, t.border);
    fb.draw_rect(MARGIN, 18, 12, 12, t.accent);
    fb.draw_text(MARGIN + 24, 8, title, t.text, 2);
    if !hint.is_empty() {
        let w = hint.len() * 8;
        if w + MARGIN < FB_WIDTH {
            fb.draw_text(FB_WIDTH - MARGIN - w, 16, hint, t.text_muted, 1);
        }
    }
}

/// Width in pixels of a footer segment list (text + ` | ` separators).
fn segments_width(segs: &[(&str, Color)]) -> usize {
    let text: usize = segs.iter().map(|(s, _)| s.len() * 8).sum();
    text + segs.len().saturating_sub(1) * 24
}

/// Bottom hint bar: left-aligned and right-aligned segments of `(text, colour)`.
pub fn footer(fb: &mut Framebuffer, left: &[(&str, Color)], right: &[(&str, Color)]) {
    let t = theme();
    let y0 = FB_HEIGHT - FOOTER_H;
    fb.draw_rect(0, y0, FB_WIDTH, FOOTER_H, t.panel);
    fb.draw_rect(0, y0, FB_WIDTH, 1, t.border);
    let y = y0 + (FOOTER_H - 16) / 2;

    let mut x = MARGIN;
    for (i, (txt, col)) in left.iter().enumerate() {
        if i > 0 {
            fb.draw_text(x, y, " | ", t.text_dim, 1);
            x += 24;
        }
        fb.draw_text(x, y, txt, *col, 1);
        x += txt.len() * 8;
    }

    let mut x = FB_WIDTH - MARGIN - segments_width(right);
    for (i, (txt, col)) in right.iter().enumerate() {
        if i > 0 {
            fb.draw_text(x, y, " | ", t.text_dim, 1);
            x += 24;
        }
        fb.draw_text(x, y, txt, *col, 1);
        x += txt.len() * 8;
    }
}

/// Retro-HUD card frame: panel, 1px border, accent corner brackets, left
/// accent bar, optional pixel icon + title, and a divider under the title.
pub fn card(
    fb: &mut Framebuffer,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    icon: Option<&[&str]>,
    title: &str,
    accent: Color,
) {
    let t = theme();
    let right = x + w;
    let bottom = y + h;

    fb.draw_rect(x, y, w, h, t.panel);
    fb.draw_rect(x, y, w, 1, t.border);
    fb.draw_rect(x, bottom - 1, w, 1, t.border);
    fb.draw_rect(x, y, 1, h, t.border);
    fb.draw_rect(right - 1, y, 1, h, t.border);

    fb.draw_rect(x, y, 3, TITLE_H, accent);

    let b = 9;
    let th = 2;
    fb.draw_rect(x, y, b, th, accent);
    fb.draw_rect(x, y, th, b, accent);
    fb.draw_rect(right - b, y, b, th, accent);
    fb.draw_rect(right - th, y, th, b, accent);
    fb.draw_rect(x, bottom - th, b, th, accent);
    fb.draw_rect(x, bottom - b, th, b, accent);
    fb.draw_rect(right - b, bottom - th, b, th, accent);
    fb.draw_rect(right - th, bottom - b, th, b, accent);

    let mut tx = x + PAD + 6;
    if let Some(ic) = icon {
        fb.draw_sprite(tx, y + (TITLE_H - 8) / 2, ic, accent, 1);
        tx += 12;
    }
    fb.draw_text(tx, y + (TITLE_H - 16) / 2, title, t.text, 1);

    fb.draw_rect(x + PAD, y + TITLE_H, w - 2 * PAD, 1, t.border);
}

/// Horizontal gauge: border, track and proportional fill.
pub fn bar(fb: &mut Framebuffer, x: usize, y: usize, w: usize, h: usize, ratio: f64, fill: Color) {
    if w < 4 || h < 4 {
        return;
    }
    fb.draw_rect(x, y, w, h, theme().border);
    let inner = w - 4;
    let fw = (inner as f64 * ratio.clamp(0.0, 1.0)) as usize;
    if fw > 0 {
        fb.draw_rect(x + 2, y + 2, fw, h - 4, fill);
    }
}

/// Selection background for a list row: full-width fill + left accent bar.
pub fn row_bg(fb: &mut Framebuffer, x: usize, y: usize, w: usize, h: usize, selected: bool) {
    if selected {
        fb.draw_rect(x, y, w, h, theme().panel_active);
        fb.draw_rect(x, y, 4, h, theme().accent);
    }
}

/// Pixel-art frog mascot (data-driven masks from the crate root).
pub fn frog(fb: &mut Framebuffer, x: usize, y: usize, scale: usize) {
    fb.draw_sprite(x, y, SP_FROG_BODY, theme().accent, scale);
    fb.draw_sprite(x, y, SP_FROG_DETAIL, theme().bg, scale);
}

/// Width/height in pixels of the frog sprite at a given scale.
pub const fn frog_size(scale: usize) -> (usize, usize) {
    (22 * scale, 14 * scale)
}

// ── On-screen keyboard ─────────────────────────────────────────────────
static FLASH: Mutex<Option<(Key, Instant)>> = Mutex::new(None);

/// Register a key press so it renders inverted for a short moment.
pub fn flash(key: Key) {
    if let Ok(mut f) = FLASH.lock() {
        *f = Some((key, Instant::now()));
    }
}

fn flash_active(key: Key) -> bool {
    FLASH
        .lock()
        .map(|f| matches!(&*f, Some((k, t)) if *k == key && t.elapsed().as_millis() < 140))
        .unwrap_or(false)
}

fn modifier_active(key: Key) -> bool {
    match key {
        Key::Shift => crate::kb_shift(),
        Key::Ctrl => crate::kb_ctrl(),
        Key::Alt => crate::kb_alt(),
        _ => false,
    }
}

/// Draw the on-screen keyboard with symmetric side margins and uniform gaps.
pub fn keyboard(fb: &mut Framebuffer) {
    let t = theme();
    for (r, keys) in KB_ROWS.iter().enumerate() {
        let mut unit = 0usize;
        for key in keys.iter() {
            let (x, y, w, h) = key_rect(unit, key.w, r);
            let (bg, fg) = if flash_active(key.key) {
                (t.text, t.bg)
            } else if modifier_active(key.key) {
                (t.accent, t.bg)
            } else {
                (t.panel_active, t.text)
            };
            fb.draw_rect(x, y, w, h, bg);
            let lw = key.label.len() * 16;
            let lx = x + w.saturating_sub(lw) / 2;
            let ly = y + (KB_KEY_H.saturating_sub(32)) / 2;
            fb.draw_text(lx, ly, key.label, fg, 2);
            unit += key.w;
        }
    }
}

/// Hit-test the keyboard; gaps between keys snap to the nearest key.
pub fn keyboard_key_at(px: usize, py: usize) -> Option<Key> {
    if py < KB_TOP {
        return None;
    }
    let row = (py - KB_TOP) / (KB_KEY_H + KB_GAP);
    if row >= KB_ROWS.len() || row >= KB_ROWS_N {
        return None;
    }
    let mut unit = 0usize;
    for key in KB_ROWS[row].iter() {
        let x = KB_X0 + unit * KB_UNIT;
        let w = key.w * KB_UNIT; // include the trailing gap for forgiving hits
        if px >= x && px < x + w {
            return Some(key.key);
        }
        unit += key.w;
    }
    None
}
