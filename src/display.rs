//! Fullscreen gate.
//!
//! ttycraft paints Minecraft with half-block characters, so every usable
//! character cell doubles the vertical resolution. A windowed terminal wastes
//! most of the screen, therefore the launcher only starts when the terminal
//! fills the whole display. If the terminal size in pixels cannot be measured
//! (framebuffer console, exotic terminal) the check is skipped with a warning
//! instead of blocking the user.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, window_size, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};
use x11rb::connection::Connection;

/// The terminal counts as fullscreen when it covers at least this fraction of
/// the display in each dimension. Slightly below 1.0 so window decorations and
/// taskbars (i.e. a maximized terminal) still qualify.
const MIN_COVERAGE: f64 = 0.90;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Terminal fills (almost) the whole screen.
    Fullscreen { term: (u32, u32), screen: (u32, u32) },
    /// Terminal is measurably smaller than the screen.
    Windowed { term: (u32, u32), screen: (u32, u32) },
    /// Pixel sizes are not available; cannot tell.
    Unknown,
}

/// Measure the screen the terminal lives on, in pixels.
pub(crate) fn screen_pixels() -> Option<(u32, u32)> {
    // X11: the root window of the display the terminal runs on.
    if let Ok(display) = std::env::var("DISPLAY")
        && !display.is_empty()
        && let Ok((conn, screen_num)) = x11rb::connect(Some(&display))
    {
        let root = &conn.setup().roots[screen_num];
        if root.width_in_pixels > 0 && root.height_in_pixels > 0 {
            return Some((root.width_in_pixels as u32, root.height_in_pixels as u32));
        }
    }
    // Linux framebuffer console (no X).
    if let Ok(text) = std::fs::read_to_string("/sys/class/graphics/fb0/virtual_size")
        && let Some((w, h)) = text.trim().split_once(',')
        && let (Ok(w), Ok(h)) = (w.trim().parse::<u32>(), h.trim().parse::<u32>())
        && w > 0
        && h > 0
    {
        return Some((w, h));
    }
    None
}

/// Terminal size in pixels as reported by the pty (0 on terminals that do not
/// fill the pixel fields of `winsize`).
pub(crate) fn terminal_pixels() -> Option<(u32, u32)> {
    let ws = window_size().ok()?;
    if ws.width == 0 || ws.height == 0 {
        // many terminals leave the pixel fields at zero; a real framebuffer
        // console reports zero too, so this does not mean "windowed".
        return None;
    }
    Some((ws.width as u32, ws.height as u32))
}

pub fn check() -> Verdict {
    let term = terminal_pixels();
    let screen = screen_pixels();
    let (Some(term), Some(screen)) = (term, screen) else {
        return Verdict::Unknown;
    };
    let w_ok = term.0 as f64 >= screen.0 as f64 * MIN_COVERAGE;
    let h_ok = term.1 as f64 >= screen.1 as f64 * MIN_COVERAGE;
    if w_ok && h_ok {
        Verdict::Fullscreen { term, screen }
    } else {
        Verdict::Windowed { term, screen }
    }
}

/// Block until the terminal is fullscreen, or return an error if the user
/// gives up. Unknown pixel sizes pass with a note on stderr.
pub fn require_fullscreen() -> Result<()> {
    match check() {
        Verdict::Fullscreen { .. } => Ok(()),
        Verdict::Unknown => {
            eprintln!(
                "note: could not measure the terminal/screen size — \
                 skipping the fullscreen check (use a fullscreen terminal for best results)"
            );
            Ok(())
        }
        Verdict::Windowed { term, screen } => {
            eprintln!(
                "terminal is {}x{} px but the screen is {}x{} px — \
                 please make the terminal fullscreen",
                term.0, term.1, screen.0, screen.1
            );
            if wait_until_fullscreen()? {
                Ok(())
            } else {
                bail!("terminal is not fullscreen")
            }
        }
    }
}

/// Interactive waiting screen. Returns Ok(true) once fullscreen, Ok(false) if
/// the user quits. Re-measures on resize and about twice a second.
fn wait_until_fullscreen() -> Result<bool> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, cursor::Hide)?;

    let result = (|| -> Result<bool> {
        let mut next_probe = Instant::now();
        loop {
            if crate::signals::shutdown_requested() {
                return Ok(false);
            }
            if Instant::now() >= next_probe {
                match check() {
                    Verdict::Fullscreen { .. } | Verdict::Unknown => return Ok(true),
                    Verdict::Windowed { term, screen } => draw(&mut out, term, screen)?,
                }
                next_probe = Instant::now() + Duration::from_millis(500);
            }
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => match k.code {
                        KeyCode::Esc | KeyCode::Char('q') => return Ok(false),
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            return Ok(false);
                        }
                        _ => next_probe = Instant::now(),
                    },
                    Event::Resize(_, _) => next_probe = Instant::now(),
                    _ => {}
                }
            }
        }
    })();

    let _ = execute!(out, cursor::Show, LeaveAlternateScreen);
    let _ = disable_raw_mode();
    result
}

fn centred(text: &str, cols: usize) -> String {
    let pad = cols.saturating_sub(text.chars().count()) / 2;
    format!("{}{text}\x1b[0m", " ".repeat(pad))
}

fn draw(out: &mut impl Write, term: (u32, u32), screen: (u32, u32)) -> Result<()> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let (cols, rows) = (cols as usize, rows as usize);
    let mut buf = String::from("\x1b[2J\x1b[H");

    let blank = |n: usize, buf: &mut String| {
        for _ in 0..n {
            buf.push_str("\r\n");
        }
    };
    blank(rows.saturating_sub(8) / 2, &mut buf);
    buf.push_str(&centred("ttycraft needs a fullscreen terminal", cols));
    buf.push_str("\r\n\r\n");
    buf.push_str(&centred(
        &format!("terminal: {}x{} px", term.0, term.1),
        cols,
    ));
    buf.push_str("\r\n");
    buf.push_str(&centred(
        &format!("screen:   {}x{} px", screen.0, screen.1),
        cols,
    ));
    buf.push_str("\r\n\r\n");
    buf.push_str(&centred(
        "make the terminal fullscreen (F11, or double-click the title bar)",
        cols,
    ));
    buf.push_str("\r\n\r\n");
    buf.push_str(&centred(
        "the launcher continues automatically once the screen is filled",
        cols,
    ));
    buf.push_str("\r\n");
    buf.push_str(&centred("[q] quit", cols));

    out.write_all(buf.as_bytes())?;
    out.flush()?;
    Ok(())
}
