//! Example 7: inject keyboard & mouse input into an X11 window via XTEST —
//! the mechanism the launcher uses to control Minecraft from terminal input.
//!
//! Run:  cargo run --example xtest_input -- <window name substring>
//!
//! Demo: focuses the window, types a string, moves the pointer in a circle,
//! clicks and scrolls. Watch the target window.
//!
//! Requires: X11 session ($DISPLAY). Use `xterm` as the target to try it out:
//!   xterm &   then:   cargo run --example xtest_input -- xterm

use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, InputFocus, MapState, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

// ---------------------------------------------------------------- find window

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

// ---------------------------------------------------------------- keysym table

/// Map a printable char to (X keysym, needs_shift) — US layout.
fn keysym_for_char(c: char) -> Option<(u32, bool)> {
    match c {
        'a'..='z' => Some((c as u32, false)),
        'A'..='Z' => Some((c as u32 - 32, true)), // shift + lowercase keysym
        '1'..='9' => Some((c as u32, false)),
        '0' => Some((0x30, false)),
        ' ' => Some((0x20, false)),
        '\t' => Some((0xff09, false)),
        '\n' | '\r' => Some((0xff0d, false)), // Return
        '~' => Some((0x60, true)),
        '!' => Some((0x31, true)),
        '@' => Some((0x32, true)),
        '#' => Some((0x33, true)),
        '$' => Some((0x34, true)),
        '%' => Some((0x35, true)),
        '^' => Some((0x36, true)),
        '&' => Some((0x37, true)),
        '*' => Some((0x38, true)),
        '(' => Some((0x39, true)),
        ')' => Some((0x30, true)),
        '_' => Some((0x2d, true)),
        '+' => Some((0x3d, true)),
        '-' => Some((0x2d, false)),
        '=' => Some((0x3d, false)),
        '[' => Some((0x5b, false)),
        ']' => Some((0x5d, false)),
        '\\' => Some((0x5c, false)),
        '{' => Some((0x5b, true)),
        '}' => Some((0x5d, true)),
        '|' => Some((0x5c, true)),
        ';' => Some((0x3b, false)),
        ':' => Some((0x3b, true)),
        '\'' => Some((0x27, false)),
        '"' => Some((0x27, true)),
        ',' => Some((0x2c, false)),
        '<' => Some((0x2c, true)),
        '.' => Some((0x2e, false)),
        '>' => Some((0x2e, true)),
        '/' => Some((0x2f, false)),
        '?' => Some((0x2f, true)),
        '`' => Some((0x60, false)),
        _ => None,
    }
}

struct KeyMap {
    /// (keycode, keysym) pairs from the server
    table: Vec<(u8, u32)>,
    shift_keycode: Option<u8>,
}

impl KeyMap {
    fn build(conn: &RustConnection) -> Self {
        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let reply = conn
            .get_keyboard_mapping(min, max - min + 1)
            .expect("get_keyboard_mapping")
            .reply()
            .expect("keyboard mapping reply");
        let per = reply.keysyms_per_keycode as usize;
        let mut table = Vec::new();
        for (i, chunk) in reply.keysyms.chunks(per).enumerate() {
            for &ks in chunk.iter().take(2) {
                if ks != 0 {
                    table.push((min + i as u8, ks));
                }
            }
        }
        let shift_keycode = table
            .iter()
            .find(|(_, ks)| *ks == 0xffe1) // XK_Shift_L
            .map(|(kc, _)| *kc);
        Self { table, shift_keycode }
    }

    fn keycode_for(&self, keysym: u32) -> Option<u8> {
        // prefer the "unshifted" slot (column 0): lowercase letters live there
        self.table
            .iter()
            .find(|(_, ks)| *ks == keysym)
            .map(|(kc, _)| *kc)
    }
}

// ---------------------------------------------------------------- XTEST

const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const DEVICE_ID: u8 = 2; // confirmed by example 6 self-test

fn fake_key(conn: &RustConnection, root: Window, keycode: u8, down: bool) -> anyhow::Result<()> {
    let type_ = if down { KEY_PRESS } else { KEY_RELEASE };
    conn.xtest_fake_input(type_, keycode, 0, root, 0, 0, DEVICE_ID)?
        .check()?;
    Ok(())
}

fn type_char(conn: &RustConnection, root: Window, km: &KeyMap, c: char) -> anyhow::Result<bool> {
    let Some((keysym, shift)) = keysym_for_char(c) else {
        return Ok(false);
    };
    let Some(keycode) = km.keycode_for(keysym) else {
        eprintln!("  no keycode for keysym {keysym:#x} ('{c}')");
        return Ok(false);
    };
    if shift
        && let Some(sk) = km.shift_keycode
    {
        fake_key(conn, root, sk, true)?;
    }
    fake_key(conn, root, keycode, true)?;
    conn.flush()?;
    std::thread::sleep(Duration::from_millis(15));
    fake_key(conn, root, keycode, false)?;
    if shift
        && let Some(sk) = km.shift_keycode
    {
        fake_key(conn, root, sk, false)?;
    }
    conn.flush()?;
    std::thread::sleep(Duration::from_millis(15));
    Ok(true)
}

fn main() -> anyhow::Result<()> {
    let needle = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "xterm".to_string())
        .to_lowercase();

    let (conn, screen_num) = x11rb::connect(None).expect("no X11 display — set $DISPLAY");
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    let win = find_window(&conn, root, &needle)
        .ok_or_else(|| anyhow::anyhow!("no visible window matching {needle:?}"))?;
    let geo = conn.get_geometry(win)?.reply()?;
    eprintln!("target window 0x{win:x} ({}x{})", geo.width, geo.height);

    // focus + pointer into the window
    conn.set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME)?
        .check()?;
    let (cx, cy) = (geo.width as i16 / 2, geo.height as i16 / 2);
    conn.warp_pointer(x11rb::NONE, win, 0, 0, 0, 0, cx, cy)?.check()?;
    conn.flush()?;

    let km = KeyMap::build(&conn);
    eprintln!("keyboard: {} (keycode, keysym) pairs, shift keycode {:?}", km.table.len(), km.shift_keycode);

    // 1. type a string
    let text = "Hello from the terminal, Minecraft!";
    let mut typed = 0;
    for c in text.chars() {
        if type_char(&conn, root, &km, c)? {
            typed += 1;
        }
    }
    // 2. Return
    type_char(&conn, root, &km, '\n')?;
    eprintln!("typed {typed}/{} chars", text.chars().count());

    // 3. pointer circle (mouse-look simulation)
    for i in 0..36 {
        let angle = i as f32 * 10.0_f32.to_radians();
        let x = cx + (angle.cos() * 40.0) as i16;
        let y = cy + (angle.sin() * 25.0) as i16;
        conn.warp_pointer(x11rb::NONE, win, 0, 0, 0, 0, x, y)?
            .check()?;
        conn.flush()?;
        std::thread::sleep(Duration::from_millis(20));
    }

    // 4. left click
    for button in [1u8, 1u8] {
        conn.xtest_fake_input(BUTTON_PRESS, button, 0, root, 0, 0, DEVICE_ID)?.check()?;
        conn.flush()?;
        std::thread::sleep(Duration::from_millis(80));
        conn.xtest_fake_input(BUTTON_RELEASE, button, 0, root, 0, 0, DEVICE_ID)?.check()?;
        conn.flush()?;
        std::thread::sleep(Duration::from_millis(80));
    }

    // 5. wheel down + up (X buttons 5 and 4)
    for button in [5u8, 4u8] {
        conn.xtest_fake_input(BUTTON_PRESS, button, 0, root, 0, 0, DEVICE_ID)?.check()?;
        conn.flush()?;
        std::thread::sleep(Duration::from_millis(40));
        conn.xtest_fake_input(BUTTON_RELEASE, button, 0, root, 0, 0, DEVICE_ID)?.check()?;
        conn.flush()?;
        std::thread::sleep(Duration::from_millis(40));
    }

    eprintln!("done: typed text, circled the pointer, clicked, scrolled");
    Ok(())
}
