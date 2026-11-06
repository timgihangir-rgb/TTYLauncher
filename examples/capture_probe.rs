//! Time `XGetImage` against an already-running display: the root window versus
//! the Minecraft window. Run it while `ttycraft` is in a game session to see
//! whether capture is slow because the *window* is expensive to read (GLX
//! double buffering) or because the whole X server is busy.
//!
//! Usage: cargo run --example capture_probe -- :90

use std::time::Instant;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, Window};

fn intern(conn: &impl Connection, name: &[u8]) -> u32 {
    conn.intern_atom(false, name).unwrap().reply().unwrap().atom
}

fn find_window(conn: &impl Connection, parent: Window, needle: &str) -> Option<Window> {
    let children = conn.query_tree(parent).ok()?.reply().ok()?.children;
    for win in children {
        let utf8 = intern(conn, b"UTF8_STRING");
        let plain = intern(conn, b"STRING");
        let net_name = intern(conn, b"_NET_WM_NAME");
        let wm_name = intern(conn, b"WM_NAME");
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

fn time_grab(conn: &impl Connection, win: Window, w: u16, h: u16) -> std::time::Duration {
    let mut times = Vec::new();
    for _ in 0..30 {
        let t = Instant::now();
        let img = conn
            .get_image(ImageFormat::Z_PIXMAP, win, 0, 0, w, h, !0)
            .unwrap()
            .reply()
            .unwrap();
        times.push(t.elapsed());
        std::hint::black_box(&img.data);
    }
    times.sort();
    times[times.len() / 2]
}

fn main() {
    let display = std::env::args().nth(1).unwrap_or_else(|| ":90".into());
    let (conn, screen_num) = x11rb::connect(Some(&display)).expect("connect");
    let root = conn.setup().roots[screen_num].root;
    let mc = find_window(&conn, root, "minecraft");
    println!("display {display}, minecraft window: {mc:?}");
    let root_ms = time_grab(&conn, root, 640, 360);
    println!("XGetImage root      640x360: {root_ms:?}");
    if let Some(win) = mc {
        let ms = time_grab(&conn, win, 640, 360);
        println!("XGetImage minecraft 640x360: {ms:?}");
    }
}
