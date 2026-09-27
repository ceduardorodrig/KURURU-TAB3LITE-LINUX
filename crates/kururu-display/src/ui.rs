//! ratatui integration: a `Backend` that blits ratatui's cell buffer onto our
//! BGRA framebuffer using the project font + theme, plus the app-shell screens
//! (Menu, About, Settings, keyboard) built with ratatui widgets.

use std::io;
use std::sync::{Mutex, OnceLock};

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect, Size};
use ratatui::style::{Color as RColor, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Gauge, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};

use crate::font::{FONT_DATA, FONT_HEIGHT, FONT_WIDTH};
use crate::{theme, Color, Framebuffer, Key, MenuAction, EFFECTS, MENU_ITEMS, FB_HEIGHT, FB_WIDTH};

pub const COLS: u16 = (FB_WIDTH / FONT_WIDTH) as u16;
pub const ROWS: u16 = (FB_HEIGHT / FONT_HEIGHT) as u16;

/// First ratatui cell row of the on-screen keyboard.
pub const KB_CELL_Y0: u16 = 22;
/// One keyboard width "unit" is this many cells (each row totals 16 units).
const KB_CELL_UNIT: u16 = 8;
/// Height of a keyboard key row, in cells (3 × 16px = 48px — comfortable touch).
const KB_KEY_H: usize = 3;

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

    fn glyph(&mut self, x0: usize, y0: usize, ch: char, fg: Color) {
        let code = (ch as usize).min(127);
        let bitmap = &FONT_DATA[code];
        for row in 0..FONT_HEIGHT {
            let byte = bitmap[row];
            for col in 0..FONT_WIDTH {
                if (byte & (1 << (7 - col))) != 0 {
                    self.put(x0 + col, y0 + row, fg);
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
        let px = x as usize * FONT_WIDTH;
        let py = y as usize * FONT_HEIGHT;
        self.fill(px, py, FONT_WIDTH, FONT_HEIGHT, bg);

        let sym = cell.symbol();
        match sym.chars().next() {
            Some('█') | Some('▉') | Some('▊') | Some('▋') | Some('▌') | Some('▍') | Some('▎') | Some('▏') => {
                self.fill(px, py, FONT_WIDTH, FONT_HEIGHT, fg);
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
    fn set_cursor_position<P: Into<Position>>(&mut self, _position: P) -> io::Result<()> {
        Ok(())
    }
    fn clear(&mut self) -> io::Result<()> {
        let bg = theme().bg;
        self.fill(0, 0, FB_WIDTH, FB_HEIGHT, bg);
        Ok(())
    }
    fn clear_region(&mut self, _clear_type: ClearType) -> io::Result<()> {
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

/// Our Color → ratatui Color.
pub fn rc(c: Color) -> RColor {
    RColor::Rgb(c.r, c.g, c.b)
}

/// ratatui Color → our Color (monochrome fallback in the theme).
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

/// Map Unicode box-drawing/block glyphs to ASCII our font can render.
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

/// Draw the app shell into `fb` using ratatui widgets via `draw`.
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
fn topbar(f: &mut Frame, title: &str, hint: &str) {
    let t = theme();
    let area = Rect::new(0, 0, COLS, 3);
    let line = Line::from(vec![
        Span::styled("## ", Style::new().fg(rc(t.accent))),
        Span::styled(title, Style::new().fg(rc(t.text)).add_modifier(Modifier::BOLD)),
    ]);
    f.render_widget(Paragraph::new(line).style(Style::new().bg(rc(t.panel))), area);
    if !hint.is_empty() {
        let w = hint.chars().count() as u16;
        if w + 2 < COLS {
            let buf = f.buffer_mut();
            buf.set_string(COLS - w - 2, 1, hint, Style::new().fg(rc(t.text_muted)).bg(rc(t.panel)));
        }
    }
    f.buffer_mut().set_style(Rect::new(0, 3, COLS, 1), Style::new().bg(rc(t.border)));
}

pub fn menu(f: &mut Frame, cursor: usize) {
    let t = theme();
    topbar(f, "MENU", "[HOME] Select");

    let brand = Paragraph::new(vec![
        Line::from(Span::styled("KURURU", Style::new().fg(rc(t.accent)).add_modifier(Modifier::BOLD))),
        Line::from(Span::styled("Mnemocine Homelab", Style::new().fg(rc(t.text_dim)))),
    ])
    .alignment(Alignment::Center);
    f.render_widget(brand, Rect::new(0, 5, COLS, 2));

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
            // Two cell rows per item → comfortable touch target.
            ListItem::new(Text::from(vec![Line::from(text), Line::from("")]))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(cursor.min(MENU_ITEMS.len() - 1)));
    let list = List::new(items)
        .block(Block::new().title(" Navigation "))
        .highlight_style(Style::new().fg(rc(t.accent)).bg(rc(t.panel_active)).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");
    f.render_stateful_widget(
        list,
        Rect::new((COLS - 60) / 2, 7, 60, (MENU_ITEMS.len() * 2) as u16),
        &mut state,
    );
}

pub fn about(f: &mut Frame, lines: &[String]) {
    let t = theme();
    topbar(f, "ABOUT", "[BACK] Back");
    let text: Vec<Line> = lines
        .iter()
        .map(|l| Line::from(Span::styled(l.clone(), Style::new().fg(rc(t.text_muted)))))
        .collect();
    f.render_widget(Paragraph::new(text).alignment(Alignment::Center), Rect::new(0, 8, COLS, (lines.len() + 1) as u16));
}

/// ratatui Settings screen (display / audio / wi-fi). Touch regions are derived
/// from the same layout constants (see `settings_regions`).
pub fn settings(f: &mut Frame) {
    let t = theme();
    let editing = crate::wifi_editing();
    topbar(f, "SETTINGS", if editing { "[BACK] Done" } else { "[HOME] Edit" });

    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(Rect::new(0, 4, COLS, ROWS - 4));
    let left = split[0];
    let right = split[1];

    // ── Left: DISPLAY + AUDIO ──────────────────────────────────────────
    f.render_widget(
        Paragraph::new(Span::styled("DISPLAY", Style::new().fg(rc(t.warn)))),
        Rect::new(left.x + 2, 4, 20, 1),
    );
    let br = crate::brightness_pct();
    f.render_widget(
        Gauge::default()
            .ratio(br)
            .label(Span::styled(format!("Brightness {}%", (br * 100.0) as u32), Style::new().fg(rc(t.text))))
            .gauge_style(Style::new().fg(rc(t.accent)).bg(rc(t.border))),
        Rect::new(left.x + 2, 5, left.width - 4, 2),
    );
    let sleep = crate::sleep_label();
    f.render_widget(
        Paragraph::new(format!("Auto-sleep: {sleep}   [-]/[+]")),
        Rect::new(left.x + 2, 8, left.width - 4, 2),
    );
    f.render_widget(
        Paragraph::new(Span::styled("AUDIO", Style::new().fg(rc(t.warn)))),
        Rect::new(left.x + 2, 11, 20, 1),
    );
    let vol = crate::volume_pct();
    f.render_widget(
        Gauge::default()
            .ratio(vol)
            .label(Span::styled(format!("Volume {}%", (vol * 100.0) as u32), Style::new().fg(rc(t.text))))
            .gauge_style(Style::new().fg(rc(t.accent)).bg(rc(t.border))),
        Rect::new(left.x + 2, 12, left.width - 4, 2),
    );

    // ── Right: WI-FI ───────────────────────────────────────────────────
    let ssid = crate::wifi_selected_ssid();
    let hdr = if ssid.is_empty() { "WI-FI".to_string() } else { format!("WI-FI  > {ssid}") };
    f.render_widget(
        Paragraph::new(Span::styled(hdr, Style::new().fg(rc(t.warn)))),
        Rect::new(right.x + 2, 4, right.width - 4, 1),
    );
    let nets = crate::wifi_nets();
    let sel = crate::wifi_sel();
    if nets.is_empty() {
        f.render_widget(Paragraph::new("(procurando redes...)"), Rect::new(right.x + 2, 6, right.width - 4, 1));
    } else {
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
        f.render_stateful_widget(list, Rect::new(right.x + 2, 6, right.width - 4, SET_LIST_ROWS * 2), &mut state);
    }
    let mask = crate::wifi_psk_mask();
    f.render_widget(
        Paragraph::new(format!("Password: {mask}")).style(Style::new().fg(rc(t.text)).bg(rc(t.panel_active))),
        Rect::new(right.x + 2, 15, right.width - 4, 2),
    );
    f.render_widget(
        Paragraph::new(" [ Connect ] ").style(Style::new().fg(rc(t.accent)).bg(rc(t.border)).add_modifier(Modifier::BOLD)),
        Rect::new(right.x + 2, 18, right.width - 4, 2),
    );
    if !editing {
        let status = crate::wifi_status();
        if !status.is_empty() {
            f.render_widget(Paragraph::new(status).style(Style::new().fg(rc(t.info))), Rect::new(right.x + 2, 21, right.width - 4, 1));
        }
    }
}

/// ratatui Terminal screen chrome: topbar + keyboard (the grid is drawn pixel
/// on top by the caller).
pub fn terminal(f: &mut Frame) {
    topbar(f, "TERMINAL", "[BACK] Back");
    keyboard(f);
}

// Settings layout (in cells).
const SET_LIST_ROWS: u16 = 4;

/// First visible network index for the scrolling list window.
pub fn wifi_start(nets_len: usize, sel: usize) -> usize {
    let n = (SET_LIST_ROWS as usize).min(nets_len);
    if n == 0 {
        0
    } else {
        sel.saturating_sub(1).min(nets_len - n)
    }
}

/// Map a pixel position to a Menu item index (matches `menu`'s list area).
pub fn menu_hit(px: usize, py: usize) -> Option<usize> {
    let col = (px / FONT_WIDTH) as u16;
    let row = (py / FONT_HEIGHT) as u16;
    let cx = (COLS - 60) / 2;
    let h = (MENU_ITEMS.len() * 2) as u16;
    if col >= cx && col < cx + 60 && row >= 7 && row < 7 + h {
        Some(((row - 7) / 2) as usize)
    } else {
        None
    }
}

/// Pixel regions for the Settings touch targets (two columns, generous rows).
pub fn settings_hit(sx: usize, sy: usize) -> Option<SettingsHit> {
    let cell = |cx: u16, cy: u16, cw: u16, ch: u16| {
        (cx as usize * 8, cy as usize * 16, cw as usize * 8, ch as usize * 16)
    };
    let inside = |r: (usize, usize, usize, usize), p: (usize, usize)| {
        p.0 >= r.0 && p.0 < r.0 + r.2 && p.1 >= r.1 && p.1 < r.1 + r.3
    };
    let p = (sx, sy);
    let lx = 2u16;
    let lw = COLS / 2 - 4;
    let rx = COLS / 2 + 2;
    let rw = COLS / 2 - 4;

    // Brightness gauge (left, rows 5-6)
    let g = cell(lx, 5, lw, 2);
    if inside(g, p) {
        return Some(if sx < g.0 + g.2 / 2 { SettingsHit::BrightDown } else { SettingsHit::BrightUp });
    }
    // Auto-sleep row (left, rows 8-9)
    let s = cell(lx, 8, lw, 2);
    if inside(s, p) {
        return Some(if sx < s.0 + s.2 / 2 { SettingsHit::SleepPrev } else { SettingsHit::SleepNext });
    }
    // Volume gauge (left, rows 12-13)
    let v = cell(lx, 12, lw, 2);
    if inside(v, p) {
        return Some(if sx < v.0 + v.2 / 2 { SettingsHit::VolDown } else { SettingsHit::VolUp });
    }
    // Network rows (right, 2-cell rows)
    let nets = crate::wifi_nets();
    if !nets.is_empty() {
        for i in 0..SET_LIST_ROWS {
            let r = cell(rx, 6 + i * 2, rw, 2);
            if inside(r, p) {
                return Some(SettingsHit::Network(i as usize));
            }
        }
    }
    // Password field (right, rows 15-16)
    if inside(cell(rx, 15, rw, 2), p) {
        return Some(SettingsHit::Password);
    }
    // Connect button (right, rows 18-19)
    if inside(cell(rx, 18, rw, 2), p) {
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
// On-screen keyboard (cell-based, fills the width)
// -------------------------------------------------------------
pub fn keyboard(f: &mut Frame) {
    let t = theme();
    let width = KB_CELL_UNIT;
    let buf = f.buffer_mut();
    for (r, keys) in crate::KB_ROWS.iter().enumerate() {
        let y = KB_CELL_Y0 + r as u16 * KB_KEY_H as u16;
        let mut x = 0u16;
        for key in keys.iter() {
            let w = key.w as u16 * width;
            let active = match key.key {
                Key::Shift => crate::kb_shift(),
                Key::Ctrl => crate::kb_ctrl(),
                Key::Alt => crate::kb_alt(),
                _ => false,
            };
            let bg = if active { rc(t.accent) } else { rc(t.border) };
            let fg = if active { rc(t.bg) } else { rc(t.text) };
            // Leave the last cell as a gap so adjacent keys are distinguishable.
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

/// Map a pixel position to a keyboard key, if within the keyboard area.
pub fn keyboard_key_at(px: usize, py: usize) -> Option<Key> {
    let cy = py / FONT_HEIGHT;
    if cy < KB_CELL_Y0 as usize {
        return None;
    }
    let row = (cy - KB_CELL_Y0 as usize) / KB_KEY_H;
    if row >= crate::KB_ROWS.len() {
        return None;
    }
    let cx = px / FONT_WIDTH;
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
