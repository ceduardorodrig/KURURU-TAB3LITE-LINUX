//! ratatui integration for the app shell.
//!
//! The backend renders ratatui cells at **scale 2** (16×32 px) so text and
//! touch targets are large (Material recommends ≥48×48 dp with ≥8 dp gaps;
//! here each cell is 16×32 and key targets are 64×64). Dashboards and the
//! terminal grid remain pixel-rendered.

use std::io;
use std::sync::{Mutex, OnceLock};

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Alignment, Position, Rect, Size};
use ratatui::style::{Color as RColor, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Gauge, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};

use crate::font::{FONT_DATA, FONT_HEIGHT, FONT_WIDTH};
use crate::{theme, Color, Framebuffer, Key, MenuAction, EFFECTS, MENU_ITEMS, FB_HEIGHT, FB_WIDTH};

/// Cell size in pixels (scale 2 of the 8×16 font).
const CELL_W: usize = FONT_WIDTH * 2;
const CELL_H: usize = FONT_HEIGHT * 2;
pub const COLS: u16 = (FB_WIDTH / CELL_W) as u16;
pub const ROWS: u16 = (FB_HEIGHT / CELL_H) as u16;

/// First ratatui cell row of the on-screen keyboard (keys are 2 cells = 64px).
pub const KB_CELL_Y0: u16 = 8;
/// Pixel Y where the keyboard starts (exported for the terminal grid geometry).
pub const KB_TOP_PX: usize = KB_CELL_Y0 as usize * CELL_H;
const KB_KEY_H: usize = 2;
/// One keyboard width "unit" is this many cells (each row totals 16 units = 64 cells).
const KB_CELL_UNIT: u16 = 4;
const KB_ROWS_N: u16 = 5;

// -------------------------------------------------------------
// Backend
// -------------------------------------------------------------
pub struct FbBackend {
    buf: Vec<u8>,
}

impl FbBackend {
    fn new() -> Self {
        Self { buf: vec![0; FB_WIDTH * FB_HEIGHT * 4] }
    }
    pub fn pixels(&self) -> &[u8] {
        &self.buf
    }
    fn put(&mut self, x: usize, y: usize, c: Color) {
        if x >= FB_WIDTH || y >= FB_HEIGHT {
            return;
        }
        let o = y * FB_WIDTH * 4 + x * 4;
        self.buf[o] = c.b;
        self.buf[o + 1] = c.g;
        self.buf[o + 2] = c.r;
        self.buf[o + 3] = 255;
    }
    fn fill(&mut self, x0: usize, y0: usize, w: usize, h: usize, c: Color) {
        for y in y0..(y0 + h).min(FB_HEIGHT) {
            for x in x0..(x0 + w).min(FB_WIDTH) {
                self.put(x, y, c);
            }
        }
    }
    /// Draw a glyph at scale 2 (each font pixel becomes a 2×2 block).
    fn glyph(&mut self, x0: usize, y0: usize, ch: char, fg: Color) {
        let code = (ch as usize).min(127);
        let bitmap = &FONT_DATA[code];
        for row in 0..FONT_HEIGHT {
            let byte = bitmap[row];
            for col in 0..FONT_WIDTH {
                if (byte & (1 << (7 - col))) != 0 {
                    let px = x0 + col * 2;
                    let py = y0 + row * 2;
                    self.put(px, py, fg);
                    self.put(px + 1, py, fg);
                    self.put(px, py + 1, fg);
                    self.put(px + 1, py + 1, fg);
                }
            }
        }
    }
    fn put_cell(&mut self, x: u16, y: u16, cell: &Cell) {
        let t = theme();
        let reversed = cell.modifier.contains(Modifier::REVERSED);
        let mut fg = if matches!(cell.fg, RColor::Reset) { t.text } else { map_color(cell.fg) };
        let mut bg = if matches!(cell.bg, RColor::Reset | RColor::Black) { t.bg } else { map_color(cell.bg) };
        if reversed {
            std::mem::swap(&mut fg, &mut bg);
        }
        let px = x as usize * CELL_W;
        let py = y as usize * CELL_H;
        self.fill(px, py, CELL_W, CELL_H, bg);

        let sym = cell.symbol();
        match sym.chars().next() {
            Some('█') | Some('▉') | Some('▊') | Some('▋') | Some('▌') | Some('▍') | Some('▎') | Some('▏') => {
                self.fill(px, py, CELL_W, CELL_H, fg);
            }
            Some(ch) => {
                if ch != ' ' {
                    self.glyph(px, py, map_glyph(ch), fg);
                }
            }
            None => {}
        }
    }
}

impl Backend for FbBackend {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        for (x, y, cell) in content {
            self.put_cell(x, y, cell);
        }
        Ok(())
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(Position::new(0, 0))
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, _p: P) -> io::Result<()> {
        Ok(())
    }
    fn clear(&mut self) -> io::Result<()> {
        let bg = theme().bg;
        self.fill(0, 0, FB_WIDTH, FB_HEIGHT, bg);
        Ok(())
    }
    fn clear_region(&mut self, _c: ClearType) -> io::Result<()> {
        self.clear()
    }
    fn size(&self) -> io::Result<Size> {
        Ok(Size::new(COLS, ROWS))
    }
    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: Size::new(COLS, ROWS),
            pixels: Size::new(FB_WIDTH as u16, FB_HEIGHT as u16),
        })
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn rc(c: Color) -> RColor {
    RColor::Rgb(c.r, c.g, c.b)
}

fn map_color(c: RColor) -> Color {
    let t = theme();
    match c {
        RColor::Rgb(r, g, b) => Color { r, g, b, a: 255 },
        RColor::Black => t.bg,
        RColor::Red => t.error,
        RColor::Green => t.accent,
        RColor::Yellow => t.warn,
        RColor::Cyan | RColor::LightCyan => t.info,
        RColor::Reset | RColor::Gray | RColor::DarkGray | RColor::White => t.text,
        _ => t.text,
    }
}

fn map_glyph(ch: char) -> char {
    match ch {
        '─' | '━' | '═' | '╌' | '┄' | '┈' => '-',
        '│' | '┃' | '║' | '╎' | '┆' | '┊' => '|',
        '┌' | '┐' | '└' | '┘' | '├' | '┤' | '┬' | '┴' | '┼' | '╭' | '╮' | '╰' | '╯' | '╔'
        | '╗' | '╚' | '╝' | '╠' | '╣' | '╦' | '╩' | '╬' => '+',
        '▶' | '►' | '»' | '›' => '>',
        '◀' | '◄' | '«' | '‹' => '<',
        '▲' => '^',
        '▼' => 'v',
        '•' | '●' | '·' => '*',
        '✓' | '✔' => '+',
        '✗' | '✘' => 'x',
        c if c.is_ascii() => c,
        _ => '?',
    }
}

// -------------------------------------------------------------
// Terminal (ratatui)
// -------------------------------------------------------------
static UI: OnceLock<Mutex<Option<Terminal<FbBackend>>>> = OnceLock::new();

pub fn render(fb: &mut Framebuffer, draw: impl FnOnce(&mut Frame)) {
    let m = UI.get_or_init(|| match Terminal::new(FbBackend::new()) {
        Ok(t) => Mutex::new(Some(t)),
        Err(_) => Mutex::new(None),
    });
    if let Ok(mut guard) = m.lock() {
        if let Some(t) = guard.as_mut() {
            let _ = t.draw(|f| {
                let full = f.area();
                f.buffer_mut().set_style(full, Style::new().bg(rc(theme().bg)));
                draw(f);
            });
            let px = t.backend().pixels();
            fb.buffer.copy_from_slice(px);
        }
    }
}

// -------------------------------------------------------------
// Widgets
// -------------------------------------------------------------
/// Two-row header: accent marker + title, right-aligned hint, bottom divider.
fn topbar(f: &mut Frame, title: &str, hint: &str) {
    let t = theme();
    f.render_widget(Paragraph::new("").style(Style::new().bg(rc(t.panel))), Rect::new(0, 0, COLS, 2));
    f.buffer_mut().set_string(0, 0, "##", Style::new().fg(rc(t.accent)).bg(rc(t.panel)));
    f.buffer_mut().set_string(3, 0, title, Style::new().fg(rc(t.text)).bg(rc(t.panel)));
    if !hint.is_empty() {
        let w = hint.chars().count() as u16;
        if w + 1 < COLS {
            f.buffer_mut().set_string(COLS - w - 1, 1, hint, Style::new().fg(rc(t.text_muted)).bg(rc(t.panel)));
        }
    }
    f.buffer_mut().set_style(Rect::new(0, 2, COLS, 1), Style::new().bg(rc(t.border)));
}

/// Section header line.
fn section(f: &mut Frame, x: u16, y: u16, text: &str) {
    f.buffer_mut().set_string(x, y, text, Style::new().fg(rc(theme().warn)));
}

pub fn menu(f: &mut Frame, cursor: usize) {
    let t = theme();
    topbar(f, "MENU", "[HOME] Select");

    let brand = Paragraph::new(Line::from(vec![
        Span::styled("KURURU", Style::new().fg(rc(t.accent)).add_modifier(Modifier::BOLD)),
        Span::styled("   -   Mnemocine Homelab", Style::new().fg(rc(t.text_dim))),
    ]))
    .alignment(Alignment::Center);
    f.render_widget(brand, Rect::new(0, 5, COLS, 1));

    let items: Vec<ListItem> = MENU_ITEMS
        .iter()
        .map(|(label, action)| {
            let text = match action {
                MenuAction::ToggleTheme => format!("{}: {}", label, theme().name),
                MenuAction::ToggleEffects => {
                    let on = EFFECTS.load(std::sync::atomic::Ordering::Relaxed) != 0;
                    format!("{}: {}", label, if on { "ON" } else { "OFF" })
                }
                MenuAction::Screen(_) => (*label).to_string(),
            };
            // Two cell rows per item → 64px touch target.
            ListItem::new(Text::from(vec![Line::from(text), Line::from("")]))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(cursor.min(MENU_ITEMS.len() - 1)));
    let list = List::new(items)
        .block(Block::new().title(" Navigation "))
        .highlight_style(Style::new().fg(rc(t.accent)).bg(rc(t.panel_active)).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, Rect::new(2, 6, COLS - 4, (MENU_ITEMS.len() * 2) as u16), &mut state);
}

pub fn about(f: &mut Frame, lines: &[String]) {
    let t = theme();
    topbar(f, "ABOUT", "[BACK] Back");
    let text: Vec<Line> = lines
        .iter()
        .flat_map(|l| vec![Line::from(Span::styled(l.clone(), Style::new().fg(rc(t.text_muted)))), Line::from("")])
        .collect();
    f.render_widget(Paragraph::new(text).alignment(Alignment::Center), Rect::new(0, 3, COLS, (lines.len() * 2 + 1) as u16));
}

/// Settings: two columns (display/audio | wi-fi) with large touch targets.
pub fn settings(f: &mut Frame) {
    let t = theme();
    let editing = crate::wifi_editing();
    topbar(f, "SETTINGS", if editing { "[BACK] Done" } else { "[HOME] Edit" });

    // Left column (x 1..31), right column (x 33..63).
    let lx = 1u16;
    let lw = 30u16;
    let rx = 33u16;
    let rw = 30u16;

    // ── Left: DISPLAY + AUDIO ──────────────────────────────────────────
    section(f, lx, 3, "DISPLAY");
    let br = crate::brightness_pct();
    f.buffer_mut().set_string(lx, 4, &format!("Brightness {}%", (br * 100.0) as u32), Style::new().fg(rc(t.text)));
    f.render_widget(
        Gauge::default().ratio(br).label("").gauge_style(Style::new().fg(rc(t.accent)).bg(rc(t.border))),
        Rect::new(lx, 5, lw, 1),
    );
    let sleep = crate::sleep_label();
    f.render_widget(Paragraph::new(format!("Auto-sleep: {sleep}")), Rect::new(lx, 7, lw, 1));
    f.buffer_mut().set_string(lx + lw - 7, 7, "[-] [+]", Style::new().fg(rc(t.accent)));
    section(f, lx, 9, "AUDIO");
    let vol = crate::volume_pct();
    f.buffer_mut().set_string(lx, 10, &format!("Volume {}%", (vol * 100.0) as u32), Style::new().fg(rc(t.text)));
    f.render_widget(
        Gauge::default().ratio(vol).label("").gauge_style(Style::new().fg(rc(t.accent)).bg(rc(t.border))),
        Rect::new(lx, 11, lw, 1),
    );

    // ── Right: WI-FI ───────────────────────────────────────────────────
    let ssid = crate::wifi_selected_ssid();
    let hdr = if ssid.is_empty() { "WI-FI".to_string() } else { format!("WI-FI > {ssid}") };
    section(f, rx, 3, &hdr);
    let nets = crate::wifi_nets();
    let sel = crate::wifi_sel();
    if nets.is_empty() {
        f.render_widget(Paragraph::new("(procurando redes...)"), Rect::new(rx, 4, rw, 1));
    } else if !editing {
        let start = wifi_start(nets.len(), sel);
        let items: Vec<ListItem> = nets
            .iter()
            .skip(start)
            .take(SET_LIST_ROWS as usize)
            .map(|(name, sig, _)| {
                let conn = name == &crate::wifi_connected();
                let mark = if conn { "*" } else { " " };
                ListItem::new(Text::from(vec![
                    Line::from(format!("{mark} {name}")),
                    Line::from(format!("   {sig} dBm")),
                ]))
            })
            .collect();
        let mut state = ListState::default();
        state.select(Some(sel.saturating_sub(start) as usize));
        let list = List::new(items)
            .highlight_style(Style::new().fg(rc(t.accent)).add_modifier(Modifier::BOLD))
            .highlight_symbol("> ");
        f.render_stateful_widget(list, Rect::new(rx, 4, rw, SET_LIST_ROWS * 2), &mut state);
    }
    let mask = crate::wifi_psk_mask();
    f.render_widget(
        Paragraph::new(format!("Password: {mask}")).style(Style::new().fg(rc(t.text)).bg(rc(t.panel_active))),
        Rect::new(rx, 12, rw, 1),
    );
    f.render_widget(
        Paragraph::new("[ CONNECT ]").style(Style::new().fg(rc(t.accent)).bg(rc(t.border)).add_modifier(Modifier::BOLD)),
        Rect::new(rx, 14, rw, 1),
    );
    if !editing {
        let status = crate::wifi_status();
        if !status.is_empty() {
            f.render_widget(Paragraph::new(status).style(Style::new().fg(rc(t.info))), Rect::new(rx, 16, rw, 1));
        }
    }
}

/// Terminal screen chrome: topbar + keyboard (grid drawn pixel on top).
pub fn terminal(f: &mut Frame) {
    topbar(f, "TERMINAL", "[BACK] Back");
    keyboard(f);
}

// Settings layout (in cells).
const SET_LIST_ROWS: u16 = 3;

pub fn wifi_start(nets_len: usize, sel: usize) -> usize {
    let n = (SET_LIST_ROWS as usize).min(nets_len);
    if n == 0 {
        0
    } else {
        sel.saturating_sub(1).min(nets_len - n)
    }
}

/// Map a pixel position to a Menu item index (2-row items).
pub fn menu_hit(px: usize, py: usize) -> Option<usize> {
    let col = (px / CELL_W) as u16;
    let row = (py / CELL_H) as u16;
    let h = (MENU_ITEMS.len() * 2) as u16;
    if col >= 2 && col < COLS - 2 && row >= 6 && row < 6 + h {
        Some(((row - 6) / 2) as usize)
    } else {
        None
    }
}

pub fn settings_hit(sx: usize, sy: usize) -> Option<SettingsHit> {
    let inside = |cx: u16, cy: u16, cw: u16, ch: u16, p: (usize, usize)| {
        let r = (cx as usize * CELL_W, cy as usize * CELL_H, cw as usize * CELL_W, ch as usize * CELL_H);
        p.0 >= r.0 && p.0 < r.0 + r.2 && p.1 >= r.1 && p.1 < r.1 + r.3
    };
    let p = (sx, sy);
    let lx = 1u16;
    let lw = 30u16;
    let rx = 33u16;
    let rw = 30u16;

    if inside(lx, 5, lw, 1, p) {
        return Some(if sx < (lx as usize + lw as usize / 2) * CELL_W { SettingsHit::BrightDown } else { SettingsHit::BrightUp });
    }
    if inside(lx, 7, lw, 1, p) {
        return Some(if sx < (lx as usize + lw as usize / 2) * CELL_W { SettingsHit::SleepPrev } else { SettingsHit::SleepNext });
    }
    if inside(lx, 11, lw, 1, p) {
        return Some(if sx < (lx as usize + lw as usize / 2) * CELL_W { SettingsHit::VolDown } else { SettingsHit::VolUp });
    }
    let nets = crate::wifi_nets();
    if !nets.is_empty() && !crate::wifi_editing() {
        for i in 0..SET_LIST_ROWS {
            if inside(rx, 4 + i * 2, rw, 2, p) {
                return Some(SettingsHit::Network(i as usize));
            }
        }
    }
    if inside(rx, 12, rw, 1, p) {
        return Some(SettingsHit::Password);
    }
    if inside(rx, 14, rw, 1, p) {
        return Some(SettingsHit::Connect);
    }
    None
}

pub enum SettingsHit {
    BrightUp,
    BrightDown,
    VolUp,
    VolDown,
    SleepPrev,
    SleepNext,
    Network(usize),
    Password,
    Connect,
}

// -------------------------------------------------------------
// On-screen keyboard (large keys: 4 cells wide × 2 tall = 64×64)
// -------------------------------------------------------------
pub fn keyboard(f: &mut Frame) {
    let t = theme();
    let buf = f.buffer_mut();
    for (r, keys) in crate::KB_ROWS.iter().enumerate() {
        let y = KB_CELL_Y0 + r as u16 * KB_KEY_H as u16;
        let mut x = 0u16;
        for key in keys.iter() {
            let w = key.w as u16 * KB_CELL_UNIT;
            let active = match key.key {
                Key::Shift => crate::kb_shift(),
                Key::Ctrl => crate::kb_ctrl(),
                Key::Alt => crate::kb_alt(),
                _ => false,
            };
            let bg = if active { rc(t.accent) } else { rc(t.panel_active) };
            let fg = if active { rc(t.bg) } else { rc(t.text) };
            let kw = w.saturating_sub(1).max(1);
            for cx in x..(x + kw).min(COLS) {
                for cy in y..(y + KB_KEY_H as u16).min(ROWS) {
                    if let Some(cell) = buf.cell_mut((cx, cy)) {
                        cell.set_symbol(" ").set_style(Style::new().bg(bg));
                    }
                }
            }
            let lw = key.label.chars().count() as u16;
            let lx = x + kw.saturating_sub(lw) / 2;
            let ly = y + (KB_KEY_H as u16 - 1) / 2;
            if lx + lw <= COLS && ly < ROWS {
                buf.set_string(lx, ly, key.label, Style::new().fg(fg).bg(bg));
            }
            x += w;
        }
    }
}

pub fn keyboard_key_at(px: usize, py: usize) -> Option<Key> {
    let cy = py / CELL_H;
    if cy < KB_CELL_Y0 as usize {
        return None;
    }
    let row = (cy - KB_CELL_Y0 as usize) / KB_KEY_H;
    if row >= crate::KB_ROWS.len() || row >= KB_ROWS_N as usize {
        return None;
    }
    let cx = px / CELL_W;
    let mut x = 0usize;
    for key in crate::KB_ROWS[row].iter() {
        let w = key.w * KB_CELL_UNIT as usize;
        if cx >= x && cx < x + w {
            return Some(key.key);
        }
        x += w;
    }
    None
}
