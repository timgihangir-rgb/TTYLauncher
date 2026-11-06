//! Example 5: capture an X11 window and stream it into the terminal
//! as half-block ANSI characters (live, with frame diffing).
//!
//! Run:  cargo run --example x11_capture -- <window name substring>
//!       cargo run --example x11_capture -- --root     # capture the whole screen
//! Quit: q (Ctrl+C), or after 60s
//!
//! Requires: X11 session ($DISPLAY), a truecolor terminal.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, ImageOrder, MapState, Window};

// ---------------------------------------------------------------- pixel format

struct PixelFormat {
    bpp: u8,
    lsb: bool,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
}

fn channel(v: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let bits = (mask >> shift).count_ones();
    let max = (1u32 << bits) - 1;
    let raw = (v & mask) >> shift;
    ((raw * 255 + max / 2) / max) as u8
}

fn decode(data: &[u8], w: usize, h: usize, fmt: &PixelFormat) -> Vec<u8> {
    let mut rgb = vec![0u8; w * h * 3];
    let stride = w * fmt.bpp as usize / 8;
    for y in 0..h {
        for x in 0..w {
            let off = y * stride + x * fmt.bpp as usize / 8;
            let px = if fmt.bpp == 32 {
                if fmt.lsb {
                    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                } else {
                    u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                }
            } else if fmt.lsb {
                u32::from_le_bytes([0, data[off + 2], data[off + 1], data[off]])
            } else {
                u32::from_be_bytes([0, data[off], data[off + 1], data[off + 2]])
            };
            let i = (y * w + x) * 3;
            rgb[i] = channel(px, fmt.red_mask);
            rgb[i + 1] = channel(px, fmt.green_mask);
            rgb[i + 2] = channel(px, fmt.blue_mask);
        }
    }
    rgb
}

fn pixel_format_for(conn: &impl Connection, depth: u8, _visual: u32) -> PixelFormat {
    let setup = conn.setup();
    let bpp = setup
        .pixmap_formats
        .iter()
        .find(|f| f.depth == depth)
        .map(|f| f.bits_per_pixel)
        .unwrap_or(32);
    let lsb = setup.image_byte_order == ImageOrder::LSB_FIRST;
    // Standard TrueColor masks — true for Xvfb, Xorg and Xwayland on Linux.
    PixelFormat { bpp, lsb, red_mask: 0x00ff0000, green_mask: 0x0000ff00, blue_mask: 0x000000ff }
}

// ---------------------------------------------------------------- ANSI render

#[derive(Clone, Copy, PartialEq)]
struct Cell {
    fg: [u8; 3],
    bg: [u8; 3],
}

fn render_diff(prev: &mut [Cell], width: usize, height: usize, rgb: &[u8], out: &mut String) {
    out.clear();
    let rows = height / 2;
    let mut cur_fg: Option<[u8; 3]> = None;
    let mut cur_bg: Option<[u8; 3]> = None;
    let mut cursor_ok = false;

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
                cursor_ok = false;
                continue;
            }
            if !cursor_ok {
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
            cursor_ok = true;
        }
        cursor_ok = false;
    }
    if !out.is_empty() {
        out.push_str("\x1b[0m");
    }
}

// ---------------------------------------------------------------- window find

fn window_name(conn: &impl Connection, win: Window) -> Option<String> {
    let utf8 = conn.intern_atom(false, b"UTF8_STRING").ok()?.reply().ok()?.atom;
    let plain = conn.intern_atom(false, b"STRING").ok()?.reply().ok()?.atom;
    let net = conn.intern_atom(false, b"_NET_WM_NAME").ok()?.reply().ok()?.atom;
    let legacy = conn.intern_atom(false, b"WM_NAME").ok()?.reply().ok()?.atom;
    for (prop, ty) in [(net, utf8), (legacy, plain)] {
        let reply = conn.get_property(false, win, prop, ty, 0, 1024).ok()?.reply().ok()?;
        if !reply.value.is_empty() {
            return Some(String::from_utf8_lossy(&reply.value).into_owned());
        }
    }
    None
}

fn find_window(conn: &impl Connection, root: Window, needle: &str) -> Option<Window> {
    let children = conn.query_tree(root).ok()?.reply().ok()?.children;
    for win in children {
        let attrs = match conn.get_window_attributes(win) {
            Ok(cookie) => match cookie.reply() {
                Ok(a) => a,
                Err(_) => continue,
            },
            Err(_) => continue,
        };
        if attrs.map_state != MapState::VIEWABLE {
            continue;
        }
        if let Some(name) = window_name(conn, win)
            && name.to_lowercase().contains(needle)
        {
            return Some(win);
        }
        if let Some(found) = find_window(conn, win, needle) {
            return Some(found);
        }
    }
    None
}

// ---------------------------------------------------------------- main

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let needle = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    let capture_root = args.iter().any(|a| a == "--root");

    let (conn, screen_num) = x11rb::connect(None).expect("no X11 display — set $DISPLAY");
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    let (target, w, h, depth, visual) = if capture_root {
        let geo = conn.get_geometry(root)?.reply()?;
        (root, geo.width as usize, geo.height as usize, geo.depth, screen.root_visual)
    } else {
        let needle = if needle.is_empty() { "x".into() } else { needle };
        let win = find_window(&conn, root, &needle)
            .unwrap_or_else(|| panic!("no visible window matching {needle:?}"));
        let geo = conn.get_geometry(win)?.reply()?;
        (win, geo.width as usize, geo.height as usize, geo.depth, screen.root_visual)
    };
    eprintln!("capturing window 0x{target:x} ({w}x{h}, depth {depth})");

    let fmt = pixel_format_for(&conn, depth, visual);
    let cells = (w * (h / 2)).max(1);
    let mut prev = vec![Cell { fg: [0; 3], bg: [0; 3] }; cells];
    let mut out = String::with_capacity(w * h);

    let mut stdout = io::stdout();
    crossterm::terminal::enable_raw_mode()?;
    write!(stdout, "\x1b[?1049h\x1b[?25l\x1b[2J")?;
    stdout.flush()?;

    let start = Instant::now();
    let mut frames = 0u64;
    let target_frametime = Duration::from_millis(33);

    loop {
        if start.elapsed() > Duration::from_secs(60) {
            break;
        }
        if crossterm::event::poll(Duration::from_millis(0))?
            && let crossterm::event::Event::Key(k) = crossterm::event::read()?
        {
            use crossterm::event::{KeyCode, KeyModifiers};
            if k.code == KeyCode::Char('q')
                || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL))
            {
                break;
            }
        }

        let t0 = Instant::now();
        let img = match conn.get_image(ImageFormat::Z_PIXMAP, target, 0, 0, w as u16, h as u16, !0)
        {
            Ok(cookie) => match cookie.reply() {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("get_image reply error: {e:?}");
                    break;
                }
            },
            Err(e) => {
                eprintln!("get_image request error: {e:?}");
                break;
            }
        };
        let rgb = decode(&img.data, w, h, &fmt);
        render_diff(&mut prev, w, h, &rgb, &mut out);
        write!(stdout, "{out}")?;
        stdout.flush()?;
        frames += 1;

        let elapsed = t0.elapsed();
        if elapsed < target_frametime {
            std::thread::sleep(target_frametime - elapsed);
        }
    }

    let secs = start.elapsed().as_secs_f64().max(0.001);
    write!(stdout, "\x1b[?25h\x1b[?1049l")?;
    stdout.flush()?;
    crossterm::terminal::disable_raw_mode()?;
    println!(
        "{frames} frames in {secs:.1}s ({:.1} FPS), {w}x{h} px",
        frames as f64 / secs
    );
    Ok(())
}
