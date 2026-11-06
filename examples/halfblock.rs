//! Example 1: render an RGB frame buffer as ANSI half-block characters.
//!
//! Every terminal cell holds two vertically stacked pixels:
//!   fg = top pixel, bg = bottom pixel, glyph = '▀' (U+2580).
//! Only cells that changed since the previous frame are rewritten (frame diff).
//!
//! Run:  cargo run --example halfblock

use std::io::{self, Write};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq)]
struct Cell {
    fg: [u8; 3],
    bg: [u8; 3],
}

fn render_diff(
    prev: &mut [Cell],
    width: usize,
    height: usize,
    rgb: &[u8],
    out: &mut String,
) -> usize {
    out.clear();
    let rows = height / 2;
    let mut written = 0usize;
    // state of SGR colors we last emitted, to skip redundant codes
    let mut cur_fg: Option<[u8; 3]> = None;
    let mut cur_bg: Option<[u8; 3]> = None;
    let mut cursor_ok = false; // terminal cursor is where we need it

    for row in 0..rows {
        for col in 0..width {
            let top = (row * 2) * width + col;
            let bot = (row * 2 + 1) * width + col;
            let cell = Cell {
                fg: [rgb[top * 3], rgb[top * 3 + 1], rgb[top * 3 + 2]],
                bg: [rgb[bot * 3], rgb[bot * 3 + 1], rgb[bot * 3 + 2]],
            };
            let idx = row * width + col;
            if prev[idx] == cell {
                cursor_ok = false; // a gap appears in the run of changed cells
                continue;
            }
            if !cursor_ok {
                // CSI row;col H — 1-based cursor position
                out.push_str(&format!("\x1b[{};{}H", row + 1, col + 1));
                cur_fg = None;
                cur_bg = None;
            }
            if cur_fg != Some(cell.fg) {
                out.push_str(&format!("\x1b[38;2;{};{};{}m", cell.fg[0], cell.fg[1], cell.fg[2]));
                cur_fg = Some(cell.fg);
            }
            if cur_bg != Some(cell.bg) {
                out.push_str(&format!("\x1b[48;2;{};{};{}m", cell.bg[0], cell.bg[1], cell.bg[2]));
                cur_bg = Some(cell.bg);
            }
            out.push('▀');
            prev[idx] = cell;
            written += 1;
            cursor_ok = true;
        }
        cursor_ok = false;
    }
    if written > 0 {
        out.push_str("\x1b[0m");
    }
    written
}

fn main() -> io::Result<()> {
    // 120 columns x 60 rows -> 120 x 120 pixels (each cell = 2 vertical pixels)
    let width = 120usize;
    let height = 120usize;
    let rows = height / 2;

    let mut buf = vec![0u8; width * height * 3];
    let mut prev = vec![
        Cell { fg: [0; 3], bg: [0; 3] };
        width * rows
    ];
    let mut out = String::with_capacity(width * rows * 40);

    let stdout = io::stdout();
    let mut w = stdout.lock();

    // alt screen, hide cursor
    write!(w, "\x1b[?1049h\x1b[?25l\x1b[2J")?;
    w.flush()?;

    let start = Instant::now();
    let frame_time = Duration::from_millis(33); // ~30 FPS
    let mut frames = 0u32;
    let mut total_bytes = 0u64;

    while start.elapsed() < Duration::from_secs(5) {
        let t0 = Instant::now();
        let phase = start.elapsed().as_secs_f32();

        // synthetic frame: swirling two-color gradient + a "sun" ball
        for y in 0..height {
            for x in 0..width {
                let fx = x as f32 / width as f32;
                let fy = y as f32 / height as f32;
                let v = (fx * 6.0 + phase).sin() * (fy * 6.0 - phase).cos();
                let r = ((v * 0.5 + 0.5) * 200.0) as u8;
                let g = (((fx - fy + phase * 0.3).sin() * 0.5 + 0.5) * 180.0) as u8;
                let b = ((fy * 4.0 + phase * 0.7).sin() * 0.5 + 0.5 * 1.0) as u8 * 127;
                let i = (y * width + x) * 3;
                buf[i] = r;
                buf[i + 1] = g;
                buf[i + 2] = b;
            }
        }
        // moving bright ball
        let bx = (width as f32 * (0.5 + 0.4 * phase.sin())) as usize;
        let by = (height as f32 * (0.5 + 0.4 * (phase * 1.3).cos())) as usize;
        for dy in 0..16i32 {
            for dx in 0..16i32 {
                let x = bx as i32 + dx - 8;
                let y = by as i32 + dy - 8;
                if x >= 0 && y >= 0 && (x as usize) < width && (y as usize) < height {
                    let i = ((y as usize) * width + x as usize) * 3;
                    buf[i] = 255;
                    buf[i + 1] = 240;
                    buf[i + 2] = 80;
                }
            }
        }

        let _changed = render_diff(&mut prev, width, height, &buf, &mut out);
        write!(w, "{out}")?;
        w.flush()?;
        total_bytes += out.len() as u64;
        frames += 1;

        let elapsed = t0.elapsed();
        if elapsed < frame_time {
            std::thread::sleep(frame_time - elapsed);
        }
    }

    // stats + restore terminal
    write!(w, "\x1b[?25h\x1b[?1049l")?;
    w.flush()?;
    println!(
        "{frames} frames in 5s ({:.1} FPS avg), {} bytes/frame avg, {width}x{height} px = {width}x{rows} cells",
        frames as f64 / 5.0,
        total_bytes / frames.max(1) as u64
    );
    Ok(())
}
