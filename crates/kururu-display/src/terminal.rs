//! Terminal emulation: `alacritty_terminal` (grid/ANSI) over a PTY running a
//! shell. The screen is rendered with the daemon's own framebuffer + theme.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

use crate::{theme, Framebuffer};

const CELL_W: usize = 8;
const CELL_H: usize = 16;

#[derive(Clone)]
struct Listener;
impl EventListener for Listener {}

struct Dims {
    cols: usize,
    rows: usize,
}
impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

struct Terminal {
    term: Term<Listener>,
    parser: Processor,
    writer: Box<dyn Write + Send>,
    _master: Box<dyn MasterPty + Send>,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
    cols: usize,
    rows: usize,
}

static TERM: OnceLock<Mutex<Terminal>> = OnceLock::new();

/// Initialise the terminal once: open a PTY, spawn `sh`, start the reader
/// thread feeding the emulator and waking the display on output.
pub fn init(cols: usize, rows: usize, wake: Arc<AtomicBool>) -> Result<(), String> {
    if TERM.get().is_some() {
        return Ok(());
    }
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: (cols * CELL_W) as u16,
            pixel_height: (rows * CELL_H) as u16,
        })
        .map_err(|e| format!("openpty: {e}"))?;

    let mut cmd = CommandBuilder::new("/bin/sh");
    cmd.env("TERM", "xterm-256color");
    cmd.env("PATH", "/usr/local/bin:/usr/bin:/usr/sbin:/bin:/sbin");
    cmd.env("TZ", "BRT3");
    let child = pair.slave.spawn_command(cmd).map_err(|e| format!("spawn: {e}"))?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().map_err(|e| format!("reader: {e}"))?;
    let writer = pair.master.take_writer().map_err(|e| format!("writer: {e}"))?;

    let config = Config::default();
    let term = Term::new(config, &Dims { cols, rows }, Listener);

    let terminal = Terminal {
        term,
        parser: Processor::new(),
        writer,
        _master: pair.master,
        _child: child,
        cols,
        rows,
    };
    TERM.set(Mutex::new(terminal)).map_err(|_| "already initialized".to_string())?;

    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            if let Some(t) = TERM.get() {
                if let Ok(mut guard) = t.lock() {
                    let Terminal { term, parser, .. } = &mut *guard;
                    parser.advance(term, &buf[..n]);
                }
            }
            wake.store(true, Ordering::SeqCst);
        }
    });

    Ok(())
}

pub fn is_ready() -> bool {
    TERM.get().is_some()
}

/// Write raw bytes to the shell's stdin.
pub fn write(bytes: &[u8]) {
    if let Some(t) = TERM.get() {
        if let Ok(mut guard) = t.lock() {
            let _ = guard.writer.write_all(bytes);
            let _ = guard.writer.flush();
        }
    }
}

/// Scroll the viewport by `delta` lines (positive = up).
pub fn scroll(delta: i32) {
    if let Some(t) = TERM.get() {
        if let Ok(mut guard) = t.lock() {
            guard.term.scroll_display(Scroll::Delta(delta));
        }
    }
}

/// Render the visible grid at (x0, y0) in the current theme (monochrome
/// phosphor: colours collapse to the theme's text colour, INVERSE flipped).
pub fn render(fb: &mut Framebuffer, x0: usize, y0: usize) {
    let Some(t) = TERM.get() else { return };
    let Ok(guard) = t.lock() else { return };
    let content = guard.term.renderable_content();
    let (rows, cols) = (guard.rows, guard.cols);
    let fg0 = theme().text;
    let bg0 = theme().bg;

    for indexed in content.display_iter {
        let row = indexed.point.line.0;
        let col = indexed.point.column.0;
        if row < 0 || row as usize >= rows || col >= cols {
            continue;
        }
        let (row, col) = (row as usize, col);
        let cell = &indexed.cell;
        let px = x0 + col * CELL_W;
        let py = y0 + row * CELL_H;
        let inverse = cell.flags.contains(Flags::INVERSE);
        if inverse {
            fb.draw_rect(px, py, CELL_W, CELL_H, fg0);
        }
        if cell.c != ' ' {
            fb.draw_char(px, py, cell.c, if inverse { bg0 } else { fg0 }, 1);
        }
    }

    // Cursor: a small underline (non-destructive).
    let cur = content.cursor.point;
    let crow = cur.line.0;
    if crow >= 0 && (crow as usize) < rows {
        let px = x0 + cur.column.0 * CELL_W;
        let py = y0 + crow as usize * CELL_H + CELL_H - 2;
        fb.draw_rect(px, py, CELL_W, 2, theme().accent);
    }
}
