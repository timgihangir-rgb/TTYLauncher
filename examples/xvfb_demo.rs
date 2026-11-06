//! Example 6: the full virtual-display pipeline WITHOUT Minecraft.
//!
//! 1. spawn Xvfb (headless X server on a free display)
//! 2. connect via x11rb, create a window, draw an animated gradient
//! 3. capture frames with GetImage, render as half-block ANSI in the terminal
//! 4. self-test XTEST input (fake keypress must arrive to our window;
//!    pointer warp + relative motion must work) — this is exactly how
//!    the launcher will control Minecraft
//!
//! Run:  cargo run --example xvfb_demo
//!
//! Requires: Xvfb binary in PATH (apt install xvfb) and a truecolor terminal.

use std::io::{self, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, ImageFormat, ImageOrder,
    InputFocus, Window, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

// ---------------------------------------------------------------- rendering

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

fn decode(data: &[u8], w: usize, h: usize, bpp: u8, lsb: bool) -> Vec<u8> {
    let mut rgb = vec![0u8; w * h * 3];
    let stride = w * bpp as usize / 8;
    for y in 0..h {
        for x in 0..w {
            let off = y * stride + x * bpp as usize / 8;
            let px = if bpp == 32 {
                if lsb {
                    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                } else {
                    u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                }
            } else if lsb {
                u32::from_le_bytes([0, data[off + 2], data[off + 1], data[off]])
            } else {
                u32::from_be_bytes([0, data[off], data[off + 1], data[off + 2]])
            };
            let i = (y * w + x) * 3;
            rgb[i] = channel(px, 0x00ff0000);
            rgb[i + 1] = channel(px, 0x0000ff00);
            rgb[i + 2] = channel(px, 0x000000ff);
        }
    }
    rgb
}

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

// ---------------------------------------------------------------- Xvfb

fn spawn_xvfb() -> (Child, u16) {
    let display = (90..110u16)
        .find(|n| !std::path::Path::new(&format!("/tmp/.X11-unix/X{n}")).exists())
        .expect("no free X display numbers");
    let child = Command::new("Xvfb")
        .arg(format!(":{display}"))
        .args(["-screen", "0", "800x600x24", "-nolisten", "tcp"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to run Xvfb — is it installed? (apt install xvfb)");
    for _ in 0..100 {
        if std::path::Path::new(&format!("/tmp/.X11-unix/X{display}")).exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    (child, display)
}

fn connect_xvfb(display: u16) -> (RustConnection, usize) {
    for _ in 0..100 {
        if let Ok(pair) = x11rb::connect(Some(&format!(":{display}"))) {
            return pair;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("cannot connect to Xvfb :{display}");
}

// ---------------------------------------------------------------- X helpers

fn keycode_for(conn: &RustConnection, keysym: u32) -> Option<u8> {
    let setup = conn.setup();
    let min = setup.min_keycode;
    let max = setup.max_keycode;
    let reply = conn.get_keyboard_mapping(min, max - min + 1).ok()?.reply().ok()?;
    let per = reply.keysyms_per_keycode as usize;
    for (i, chunk) in reply.keysyms.chunks(per).enumerate() {
        if chunk.contains(&keysym) {
            return Some(min + i as u8);
        }
    }
    None
}

/// Drain all pending X events (non-blocking, with a short settle time).
fn drain_events(conn: &RustConnection, settle_ms: u64) -> Vec<Event> {
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(settle_ms);
    loop {
        match conn.poll_for_event() {
            Ok(Some(ev)) => events.push(ev),
            Ok(None) => {
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
    events
}

/// Send one XTEST event and wait for the server to confirm it.
fn fake(
    conn: &RustConnection,
    type_: u8,
    detail: u8,
    root: Window,
    x: i16,
    y: i16,
    deviceid: u8,
) -> Result<(), String> {
    match conn.xtest_fake_input(type_, detail, 0, root, x, y, deviceid) {
        Ok(cookie) => cookie.check().map_err(|e| format!("{e:?}")),
        Err(e) => Err(format!("{e:?}")),
    }
}

// ---------------------------------------------------------------- main

fn main() {
    eprintln!("spawning Xvfb...");
    let (mut xvfb, display) = spawn_xvfb();
    let (conn, screen_num) = connect_xvfb(display);
    eprintln!("connected to :{display}\n");

    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let lsb = conn.setup().image_byte_order == ImageOrder::LSB_FIRST;

    // ---- window
    let (ww, wh) = (320u16, 200u16);
    let win = conn.generate_id().unwrap();
    conn.create_window(
        0, // depth 0 = copy from parent
        win,
        root,
        0,
        0,
        ww,
        wh,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new().background_pixel(screen.white_pixel),
    )
    .unwrap()
    .check()
    .unwrap();
    conn.map_window(win).unwrap().check().unwrap();
    conn.change_window_attributes(
        win,
        &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
            .event_mask(EventMask::KEY_PRESS | EventMask::KEY_RELEASE | EventMask::POINTER_MOTION),
    )
    .unwrap()
    .check()
    .unwrap();
    let gc = conn.generate_id().unwrap();
    conn.create_gc(gc, win, &CreateGCAux::new().graphics_exposures(0))
        .unwrap()
        .check()
        .unwrap();
    conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(300));

    // ---- input self-tests
    eprintln!("== XTEST self-tests ==");
    conn.set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME)
        .unwrap()
        .check()
        .unwrap();

    // keysym 'a' = 0x61
    let ka = keycode_for(&conn, 0x61).expect("no keycode for keysym 'a'");
    eprintln!("keysym 0x61 ('a') -> keycode {ka}");

    let mut key_ok = false;
    for deviceid in [2u8, 0, 1, 4] {
        drain_events(&conn, 5);
        let r = fake(&conn, 2 /* KeyPress */, ka, root, 0, 0, deviceid);
        let r2 = fake(&conn, 3 /* KeyRelease */, ka, root, 0, 0, deviceid);
        if r.is_err() || r2.is_err() {
            eprintln!("  deviceid={deviceid}: request error ({r:?} / {r2:?})");
            continue;
        }
        let got = drain_events(&conn, 120)
            .into_iter()
            .any(|e| matches!(e, Event::KeyPress(k) if k.detail == ka));
        eprintln!("  deviceid={deviceid}: fake key -> {}", if got { "DELIVERED ✓" } else { "nothing" });
        if got {
            key_ok = true;
            break;
        }
    }
    assert!(key_ok, "XTEST key delivery failed for all device ids");

    // pointer: absolute warp to (100, 80)
    conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, 100, 80)
        .unwrap()
        .check()
        .unwrap();
    let p = conn.query_pointer(root).unwrap().reply().unwrap();
    eprintln!("absolute warp to (100,80) -> pointer at ({},{})", p.root_x, p.root_y);

    // relative fake motion (XTEST MotionNotify, detail=1 means relative)
    let mut rel_ok = false;
    for deviceid in [2u8, 0, 1, 4] {
        let r = fake(&conn, 6 /* MotionNotify */, 1 /* relative */, root, 7, 5, deviceid);
        if r.is_err() {
            eprintln!("  relative motion deviceid={deviceid}: request error");
            continue;
        }
        std::thread::sleep(Duration::from_millis(50));
        let p = conn.query_pointer(root).unwrap().reply().unwrap();
        let ok = p.root_x == 107 && p.root_y == 85;
        eprintln!(
            "  relative motion deviceid={deviceid}: now ({},{}) {}",
            p.root_x,
            p.root_y,
            if ok { "DELTA OK ✓" } else { "no delta" }
        );
        if ok {
            rel_ok = true;
            // warp back for the next test
            conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, 100, 80)
                .unwrap()
                .check()
                .unwrap();
            break;
        }
    }
    if !rel_ok {
        eprintln!("  (relative motion not confirmed — launcher will use absolute warp instead)");
    }
    eprintln!();

    // ---- animated gradient: put -> get -> render
    let cells = (ww as usize) * (wh as usize / 2);
    let mut prev = vec![Cell { fg: [0; 3], bg: [0; 3] }; cells];
    let mut out = String::with_capacity(ww as usize * wh as usize);

    let mut stdout = io::stdout();
    crossterm::terminal::enable_raw_mode().ok();
    write!(stdout, "\x1b[?1049h\x1b[?25l\x1b[2J").ok();
    stdout.flush().ok();

    let start = Instant::now();
    let mut frames = 0u64;
    let mut framebuf = vec![0u8; ww as usize * wh as usize * 4];

    while start.elapsed() < Duration::from_secs(5) {
        let t0 = Instant::now();
        let phase = start.elapsed().as_secs_f32();

        // encode moving vertical bars + radial pulse (BGRX on LE)
        for y in 0..wh as usize {
            for x in 0..ww as usize {
                let r = (((x as f32 * 0.15 + phase * 2.0).sin() * 0.5 + 0.5) * 255.0) as u32;
                let g = (((y as f32 * 0.20 - phase * 1.3).cos() * 0.5 + 0.5) * 200.0) as u32;
                let b = 90u32;
                let px = (r << 16) | (g << 8) | b;
                let i = (y * ww as usize + x) * 4;
                if lsb {
                    framebuf[i..i + 4].copy_from_slice(&px.to_le_bytes());
                } else {
                    framebuf[i..i + 4].copy_from_slice(&px.to_be_bytes());
                }
            }
        }
        conn.put_image(ImageFormat::Z_PIXMAP, win, gc, ww, wh, 0, 0, 0, 24, &framebuf)
            .unwrap()
            .check()
            .unwrap();

        let img = conn
            .get_image(ImageFormat::Z_PIXMAP, win, 0, 0, ww, wh, !0)
            .unwrap()
            .reply()
            .unwrap();
        let rgb = decode(&img.data, ww as usize, wh as usize, 32, lsb);
        render_diff(&mut prev, ww as usize, wh as usize, &rgb, &mut out);
        write!(stdout, "{out}").ok();
        stdout.flush().ok();
        frames += 1;

        let elapsed = t0.elapsed();
        if elapsed < Duration::from_millis(66) {
            std::thread::sleep(Duration::from_millis(66) - elapsed);
        }
    }

    write!(stdout, "\x1b[?25h\x1b[?1049l").ok();
    stdout.flush().ok();
    crossterm::terminal::disable_raw_mode().ok();

    println!(
        "pipeline OK: {frames} frames in 5s ({:.1} FPS) over Xvfb :{display}",
        frames as f64 / 5.0
    );

    conn.flush().ok();
    drop(conn);
    let _ = xvfb.kill();
    let _ = xvfb.wait();
    eprintln!("Xvfb stopped");
}
