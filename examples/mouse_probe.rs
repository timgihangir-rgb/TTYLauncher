//! Inspect the pointer/cursor/focus state of a *running* ttycraft display.
//!
//! Usage: cargo run --example mouse_probe -- :91 [screenshot.ppm]
//!        cargo run --example mouse_probe -- :91 motion DX DY COUNT
//!
//! `motion` sends COUNT XTEST *relative* pointer moves of (DX, DY) — the same
//! path ttycraft uses for camera look — so the effect on the game is testable.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, GrabStatus, ImageFormat, Window};
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xtest::ConnectionExt as _;

fn intern(conn: &impl Connection, name: &[u8]) -> u32 {
    conn.intern_atom(false, name).unwrap().reply().unwrap().atom
}

fn find_window(conn: &impl Connection, parent: Window, needle: &str) -> Option<Window> {
    let children = conn.query_tree(parent).ok()?.reply().ok()?.children;
    let utf8 = intern(conn, b"UTF8_STRING");
    let plain = intern(conn, b"STRING");
    let net_name = intern(conn, b"_NET_WM_NAME");
    let wm_name = intern(conn, b"WM_NAME");
    for win in children {
        for (prop, ty) in [(net_name, utf8), (wm_name, plain)] {
            if let Ok(r) = conn.get_property(false, win, prop, ty, 0, 1024)
                && let Ok(r) = r.reply()
                && !r.value.is_empty()
                && String::from_utf8_lossy(&r.value).to_lowercase().contains(needle)
            {
                return Some(win);
            }
        }
        if let Some(found) = find_window(conn, win, needle) {
            return Some(found);
        }
    }
    None
}

fn main() {
    let mut args = std::env::args().skip(1);
    let display = args.next().unwrap_or_else(|| ":91".into());
    let rest: Vec<String> = args.collect();
    let shot = rest.iter().find(|s| s.ends_with(".ppm")).cloned();
    let (conn, screen_num) = x11rb::connect(Some(&display)).expect("connect");
    let root = conn.setup().roots[screen_num].root;
    let mc = find_window(&conn, root, "minecraft");

    if let Some(i) = rest.iter().position(|s| s == "motion") {
        let dx: i16 = rest[i + 1].parse().unwrap();
        let dy: i16 = rest[i + 2].parse().unwrap();
        let n: u32 = rest[i + 3].parse().unwrap();
        for _ in 0..n {
            let _ = conn.xtest_fake_input(6, 1, 0, root, dx, dy, 2);
        }
        let _ = conn.flush();
        println!("motion      : {n} x ({dx},{dy})");
    }

    if let Ok(cookie) = conn.xfixes_query_version(2, 0)
        && let Ok(v) = cookie.reply()
    {
        println!("xfixes      : {}.{}", v.major_version, v.minor_version);
    }

    let focus = conn.get_input_focus().unwrap().reply().unwrap();
    let who = if focus.focus == root {
        "root".to_string()
    } else if focus.focus == x11rb::NONE {
        "None".to_string()
    } else {
        format!("{:#x}", focus.focus)
    };
    println!("input focus : {who}   (minecraft={:?})", mc.map(|w| format!("{w:#x}")));

    if let Some(win) = mc {
        let g = conn.get_geometry(win).unwrap().reply().unwrap();
        println!("geometry    : {}x{}+{}+{}", g.width, g.height, g.x, g.y);

        match conn.grab_pointer(
            false, win, x11rb::protocol::xproto::EventMask::NO_EVENT, GrabMode::ASYNC, GrabMode::ASYNC,
            x11rb::NONE, x11rb::NONE, x11rb::CURRENT_TIME,
        ) {
            Ok(cookie) => match cookie.reply() {
                Ok(r) => {
                    let s = match r.status {
                        GrabStatus::SUCCESS => "FREE (game has no grab)",
                        GrabStatus::ALREADY_GRABBED => "GRABBED by the game",
                        _ => "other",
                    };
                    println!("pointer grab: {s}  ({:?})", r.status);
                    if r.status == GrabStatus::SUCCESS {
                        let _ = conn.ungrab_pointer(x11rb::CURRENT_TIME);
                    }
                }
                Err(e) => println!("pointer grab: reply error {e}"),
            },
            Err(e) => println!("pointer grab: request error {e}"),
        }

        if let Some(path) = &shot {
            let (w, h) = (g.width as usize, g.height as usize);
            let img = conn
                .get_image(ImageFormat::Z_PIXMAP, win, 0, 0, g.width, g.height, !0)
                .unwrap()
                .reply()
                .unwrap();
            let stride = w * 4;
            let mut rgb = vec![0u8; w * h * 3];
            for y in 0..h {
                let row = &img.data[y * stride..y * stride + w * 4];
                let dst = &mut rgb[y * w * 3..y * w * 3 + w * 3];
                for (px, d) in row.chunks_exact(4).zip(dst.chunks_exact_mut(3)) {
                    d[0] = px[2];
                    d[1] = px[1];
                    d[2] = px[0];
                }
            }
            let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
            out.extend_from_slice(&rgb);
            std::fs::write(path, out).unwrap();
            println!("screenshot  : {w}x{h} -> {path}");
        }
    }

    match conn.xfixes_get_cursor_image() {
        Ok(cookie) => match cookie.reply() {
            Ok(r) => {
                let opaque = r.cursor_image.iter().filter(|&&px| px >> 24 != 0).count();
                println!(
                    "cursor image: {}x{} opaque={}/{} -> {}",
                    r.width, r.height, opaque, r.cursor_image.len(),
                    if opaque == 0 { "HIDDEN (captured / in-game)" } else { "visible (menu)" },
                );
            }
            Err(e) => println!("cursor image: reply error {e}"),
        },
        Err(e) => println!("cursor image: request error {e}"),
    }
}
