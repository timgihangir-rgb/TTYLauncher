//! X11 side: connection, window discovery, frame capture, XTEST input.
//!
//! The game runs on a headless Xvfb display; this module talks to it.
//! Input injection uses XTEST (fake keys/buttons) and pointer warps —
//! proven by examples/xvfb_demo.rs self-tests.

use std::cell::Cell;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{
    AutoRepeatMode, ChangeKeyboardControlAux, ConnectionExt as _, ImageFormat, ImageOrder,
    InputFocus, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::render::Frame;
use x11rb::protocol::shm::ConnectionExt as _;

const DEVICE_ID: u8 = 2; // XTEST keyboard+pointer device, confirmed empirically
const KEY_PRESS: u8 = 2;const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6;

/// Map a printable char to (X keysym, needs shift) — US layout.
pub fn keysym_for_char(c: char) -> Option<(u32, bool)> {
    match c {
        'a'..='z' => Some((c as u32, false)),
        'A'..='Z' => Some((c as u32 - 32, true)),
        '1'..='9' => Some((c as u32, false)),
        '0' => Some((0x30, false)),
        ' ' => Some((0x20, false)),
        '\t' => Some((0xff09, false)),
        '\n' | '\r' => Some((0xff0d, false)),
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

/// X keysym values for non-printable keys.
pub fn keysym_for_special(key: crossterm::event::KeyCode) -> Option<u32> {
    use crossterm::event::KeyCode;
    Some(match key {
        KeyCode::Enter => 0xff0d,
        KeyCode::Esc => 0xff1b,
        KeyCode::Backspace => 0xff08,
        KeyCode::Tab => 0xff09,
        KeyCode::Up => 0xff52,
        KeyCode::Down => 0xff54,
        KeyCode::Left => 0xff51,
        KeyCode::Right => 0xff53,
        KeyCode::Home => 0xff50,
        KeyCode::End => 0xff57,
        KeyCode::PageUp => 0xff55,
        KeyCode::PageDown => 0xff56,
        KeyCode::Insert => 0xff63,
        KeyCode::Delete => 0xffff,
        KeyCode::F(n) if (1..=12).contains(&n) => 0xffbe + (n as u32 - 1),
        KeyCode::BackTab => 0xff09, // caller adds shift
        _ => return None,
    })
}

pub struct Geometry {
    pub width: u16,
    pub height: u16,
}

/// Is a framebuffer the plain 32-bit layout (LSB, 0x00RRGGBB) that Xvfb and
/// most driver layouts use? Used both to decide which capture path pays off
/// and whether its words can be sampled without a conversion pass.
fn is_standard_layout(lsb: bool, red: u32, green: u32, blue: u32, bpp: u8) -> bool {
    bpp == 32
        && lsb
        && red == 0x00ff_0000
        && green == 0x0000_ff00
        && blue == 0x0000_00ff
}

pub struct GameX11 {
    pub conn: RustConnection,
    pub root: Window,
    lsb: bool,
    /// colour channel masks of the root visual (for the capture fast path)
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
    /// (keycode, keysym) pairs, column 0-1 of the server keymap
    key_table: Vec<(u8, u32)>,
    shift_keycode: Option<u8>,
    control_keycode: Option<u8>,
    alt_keycode: Option<u8>,
    /// XFixes available (needed to read the cursor image)
    xfixes_ok: bool,
    /// cached `cursor_hidden()` result: (when it was read, the value)
    cursor_hidden_cache: Cell<Option<(Instant, bool)>>,
    /// MIT-SHM segment attached at connect time; `None` → the extension is
    /// unusable and captures go through the classic socket reply
    shm: Option<Shm>,
}

/// A System V shared segment the server is allowed to write frames into.
///
/// Created once, sized for the whole Xvfb screen, and kept for the whole
/// session, so `capture` can return a slice into it without any copying.
struct Shm {
    /// X-side segment id (the `Seg` the server knows this memory by)
    seg: u32,
    /// our own mapping of the segment in this process
    addr: *mut u8,
    bytes: usize,
}

impl Drop for Shm {
    fn drop(&mut self) {
        // Releasing our mapping lets the kernel reap the segment once the
        // server has detached too (it does when the connection closes; we
        // already stripped the segment name at attach time).
        unsafe { libc::shmdt(self.addr as *mut libc::c_void) };
    }
}

impl GameX11 {
    pub fn connect_display(display: &str) -> anyhow::Result<Self> {
        let (conn, screen_num) = x11rb::connect(Some(display))?;
        let root = conn.setup().roots[screen_num].root;
        let lsb = conn.setup().image_byte_order == ImageOrder::LSB_FIRST;

        // colour masks of the root visual: the common BGRX layout gets a fast
        // decoder (see `capture`), anything unusual falls back to the generic one
        let screen = &conn.setup().roots[screen_num];
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|d| &d.visuals)
            .find(|v| v.visual_id == screen.root_visual);
        let (red_mask, green_mask, blue_mask) = visual
            .map(|v| (v.red_mask, v.green_mask, v.blue_mask))
            .unwrap_or((0, 0, 0));

        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let reply = conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        let per = reply.keysyms_per_keycode as usize;
        let mut key_table = Vec::new();
        for (i, chunk) in reply.keysyms.chunks(per).enumerate() {
            for &ks in chunk.iter().take(2) {
                if ks != 0 {
                    key_table.push((min + i as u8, ks));
                }
            }
        }
        let find = |ks: u32| key_table.iter().find(|(_, k)| *k == ks).map(|(kc, _)| *kc);
        let shift_keycode = find(0xffe1);
        let control_keycode = find(0xffe3);
        let alt_keycode = find(0xffe9);

        // XFixes lets us read the cursor the server currently draws. GLFW swaps
        // in a 1x1 fully transparent cursor when a game grabs the pointer, so
        // this is how we tell "camera look" apart from "menu pointer".
        let xfixes_ok = match conn.xfixes_query_version(2, 0) {
            Ok(cookie) => matches!(cookie.reply(), Ok(v) if v.major_version >= 2),
            Err(_) => false,
        };

        // A key held down through XTEST would otherwise trip the server's
        // auto-repeat and reach the game as a stream of synthetic
        // release/press pairs. Games read the release as "key up", so holding
        // a movement key stutters and toggles (inventory, jump) fire twice.
        // ttycraft keeps the X key down exactly as long as the terminal
        // reports it held, so the server's own repeat is pure noise here.
        let aux = ChangeKeyboardControlAux {
            auto_repeat_mode: Some(AutoRepeatMode::OFF),
            ..Default::default()
        };
        let _ = conn
            .change_keyboard_control(&aux)
            .map(|cookie| cookie.check());

        // One MIT-SHM segment covering the whole screen, attached up front:
        // captures then read the frame from our own memory instead of pushing
        // a full-screen reply (several MB at 720p) through the X socket every
        // frame. Only worth it for the standard 24-bit layout, whose words we
        // can sample without a conversion pass; anything else (and any attach
        // failure) keeps the classic capture path.
        let (screen_w, screen_h, bpp24) = {
            let s = conn.setup();
            let bpp = s
                .pixmap_formats
                .iter()
                .find(|f| f.depth == 24)
                .map(|f| f.bits_per_pixel)
                .unwrap_or(0);
            let r = &s.roots[screen_num];
            (r.width_in_pixels, r.height_in_pixels, bpp)
        };
        let standard = is_standard_layout(
            lsb, red_mask, green_mask, blue_mask, bpp24,
        );
        let shm = standard
            .then(|| Self::attach_shm(&conn, screen_w as usize * screen_h as usize * 4 + 64))
            .flatten();

        Ok(Self {
            conn,
            root,
            lsb,
            red_mask,
            green_mask,
            blue_mask,
            key_table,
            shift_keycode,
            control_keycode,
            alt_keycode,
            xfixes_ok,
            cursor_hidden_cache: Cell::new(None),
            shm,
        })
    }

    /// Whether the game has grabbed the pointer and hidden the cursor, i.e. it
    /// is in gameplay and expects relative motion for camera look. Cached for a
    /// short while so the per-frame loop does not hammer the X server.
    pub fn cursor_hidden(&self) -> bool {
        const TTL: Duration = Duration::from_millis(120);
        if let Some((at, value)) = self.cursor_hidden_cache.get()
            && at.elapsed() < TTL
        {
            return value;
        }
        let value = self.query_cursor_hidden();
        self.cursor_hidden_cache.set(Some((Instant::now(), value)));
        value
    }

    fn query_cursor_hidden(&self) -> bool {
        if !self.xfixes_ok {
            return false;
        }
        match self
            .conn
            .xfixes_get_cursor_image()
            .ok()
            .and_then(|cookie| cookie.reply().ok())
        {
            // GLFW hides the cursor with a 1x1 bitmap, so a tiny or fully
            // transparent image means "grabbed / in-game".
            Some(image) => {
                (image.width <= 1 && image.height <= 1)
                    || image.cursor_image.iter().all(|px| px >> 24 == 0)
            }
            None => false,
        }
    }

    // ------------------------------------------------------------- windows

    /// Walk the window tree looking for a window whose name or class
    /// contains `needle` (case-insensitive).
    pub fn find_window(&self, needle: &str) -> Option<Window> {
        self.find_window_in(self.root, &needle.to_lowercase())
    }

    fn find_window_in(&self, parent: Window, needle: &str) -> Option<Window> {
        let conn = &self.conn;
        let children = conn.query_tree(parent).ok()?.reply().ok()?.children;
        for win in children {
            let attrs = match conn.get_window_attributes(win) {
                Ok(cookie) => match cookie.reply() {
                    Ok(a) => a,
                    Err(_) => continue,
                },
                Err(_) => continue,
            };
            if attrs.map_state != x11rb::protocol::xproto::MapState::VIEWABLE {
                continue;
            }
            if let Some(name) = self.window_identity(win)
                && name.contains(needle) {
                    return Some(win);
                }
            if let Some(found) = self.find_window_in(win, needle) {
                return Some(found);
            }
        }
        None
    }

    /// Lower-cased "name WM_CLASS" identity string of a window.
    fn window_identity(&self, win: Window) -> Option<String> {
        use x11rb::protocol::xproto::Atom;
        let conn = &self.conn;
        let intern = |name: &[u8]| -> Option<u32> {
            conn.intern_atom(false, name).ok()?.reply().ok().map(|r| r.atom)
        };
        let utf8 = intern(b"UTF8_STRING")?;
        let plain = intern(b"STRING")?;
        let mut identity = String::new();
        for (prop, ty) in [(intern(b"_NET_WM_NAME")?, utf8), (intern(b"WM_NAME")?, plain)] {
            if let Ok(reply) = conn.get_property(false, win, prop, ty, 0, 1024)
                && let Ok(reply) = reply.reply()
                    && !reply.value.is_empty() {
                        identity.push_str(&String::from_utf8_lossy(&reply.value));
                        identity.push(' ');
                        break;
                    }
        }
        if let Ok(reply) = conn.get_property(false, win, Atom::from(0u32), Atom::from(0u32), 0, 0) {
            let _ = reply; // placeholder, WM_CLASS handled below
        }
        // WM_CLASS: two NUL-terminated strings (instance, class)
        if let (Some(prop), Some(ty)) = (intern(b"WM_CLASS"), intern(b"STRING"))
            && let Ok(reply) = conn.get_property(false, win, prop, ty, 0, 256)
                && let Ok(reply) = reply.reply() {
                    identity.push_str(&String::from_utf8_lossy(&reply.value));
                }
        if identity.is_empty() { None } else { Some(identity.to_lowercase()) }
    }

    pub fn geometry(&self, win: Window) -> anyhow::Result<Geometry> {
        let geo = self.conn.get_geometry(win)?.reply()?;
        Ok(Geometry {
            width: geo.width,
            height: geo.height,
        })
    }

    // ------------------------------------------------------------- capture

    /// Capture `win` (w x h) for the renderer to sample.
    ///
    /// The fast path is MIT-SHM: the server copies the frame straight into a
    /// System V segment we attached at connect time, so no multi-megabyte
    /// reply has to travel through the X socket (that transfer dominated the
    /// per-frame time — ~40 ms for 1280x720 with a 33 ms budget). Without shm
    /// the classic `get_image` + decode path is used; `out` is its scratch
    /// buffer (only touched on that path), kept alive for the renderer.
    pub fn capture<'a>(
        &'a self,
        win: Window,
        w: usize,
        h: usize,
        out: &'a mut Vec<u8>,
    ) -> anyhow::Result<Frame<'a>> {
        if w == 0 || h == 0 {
            anyhow::bail!("capture with zero size");
        }
        if let Some(frame) = self.capture_shm(win, w, h)? {
            return Ok(frame);
        }

        let img = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, win, 0, 0, w as u16, h as u16, !0)?
            .reply()?;
        let bpp = self.bits_per_pixel(img.depth);
        let data = &img.data;
        let stride = w * bpp as usize / 8;
        if data.len() < stride * h {
            anyhow::bail!("short image data: {} < {}", data.len(), stride * h);
        }
        out.resize(w * h * 3, 0);

        // Fast path: 32bpp words in the usual 0x00RRGGBB order (Xvfb on x86).
        // Three byte loads per pixel, no per-pixel branching — this loop runs
        // `w*h` times on every captured frame.
        if self.standard_layout(img.depth) {
            for y in 0..h {
                let row = &data[y * stride..y * stride + w * 4];
                let dst = &mut out[y * w * 3..y * w * 3 + w * 3];
                for (p, d) in row.chunks_exact(4).zip(dst.chunks_exact_mut(3)) {
                    d[0] = p[2];
                    d[1] = p[1];
                    d[2] = p[0];
                }
            }
        } else {
            for y in 0..h {
                for x in 0..w {
                    let off = y * stride + x * bpp as usize / 8;
                    let px = if bpp == 32 {
                        if self.lsb {
                            u32::from_le_bytes([
                                data[off],
                                data[off + 1],
                                data[off + 2],
                                data[off + 3],
                            ])
                        } else {
                            u32::from_be_bytes([
                                data[off],
                                data[off + 1],
                                data[off + 2],
                                data[off + 3],
                            ])
                        }
                    } else if self.lsb {
                        u32::from_le_bytes([0, data[off + 2], data[off + 1], data[off]])
                    } else {
                        u32::from_be_bytes([0, data[off], data[off + 1], data[off + 2]])
                    };
                    let i = (y * w + x) * 3;
                    out[i] = Self::channel(px, self.red_mask);
                    out[i + 1] = Self::channel(px, self.green_mask);
                    out[i + 2] = Self::channel(px, self.blue_mask);
                }
            }
        }
        Ok(Frame::rgb(out, w, h))
    }

    /// MIT-SHM capture: the server copies the frame straight into our shared
    /// segment and we sample it there. `Ok(None)` → shm is unusable (or the
    /// layout is exotic) and the caller falls back to a plain `get_image`.
    fn capture_shm<'a>(
        &'a self,
        win: Window,
        w: usize,
        h: usize,
    ) -> anyhow::Result<Option<Frame<'a>>> {
        let Some(shm) = self.shm.as_ref() else {
            return Ok(None);
        };
        let bytes = w * h * 4;
        if shm.bytes < bytes {
            // cannot happen for the screen-sized segment; fall back anyway
            return Ok(None);
        }

        // A roundtrip is still required so the server actually copies; by the
        // time the reply arrives every one of its writes to the segment is
        // visible to us. A bad match (geometry changed mid-frame) surfaces as
        // a reply error, which the session already treats as a failed frame.
        let reply = self
            .conn
            .shm_get_image(
                win,
                0,
                0,
                w as u16,
                h as u16,
                !0,
                u8::from(ImageFormat::Z_PIXMAP),
                shm.seg,
                0,
            )?;
        let copied = reply.reply()?.size as usize;
        if copied < bytes {
            anyhow::bail!("shm capture too short: {copied} < {bytes}");
        }
        // SAFETY: `addr` is a live mapping of `bytes` (at least) writable
        // memory, filled by the server for this frame before the reply above
        // was produced; nothing else reads or writes it.
        unsafe {
            let view = std::slice::from_raw_parts(shm.addr as *const u8, bytes);
            Ok(Some(Frame::bgra(view, w, h)))
        }
    }

    /// Attach a System V segment of `bytes` the X server can write frames
    /// into. `None` when MIT-SHM is missing, sysv shared memory is not
    /// available, or anything else went wrong — the classic capture keeps
    /// working either way.
    fn attach_shm(conn: &RustConnection, bytes: usize) -> Option<Shm> {
        // `None` on an answerless (missing) extension, `Some(reply)` otherwise
        conn.shm_query_version().ok()?.reply().ok()?;
        let seg = conn.generate_id().ok()?;

        let shmid = unsafe { libc::shmget(libc::IPC_PRIVATE, bytes, libc::IPC_CREAT | 0o600) };
        if shmid < 0 {
            return None;
        }
        let addr = unsafe { libc::shmat(shmid, std::ptr::null(), 0) };
        if addr as isize == -1 {
            // the kernel rejected the mapping: do not leave the id behind
            unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
            return None;
        }

        // Attach writable: the whole point is that the server writes into it.
        let mut ok = false;
        if let Ok(cookie) = conn.shm_attach(seg, shmid as u32, false) {
            ok = cookie.check().is_ok();
        }
        if !ok {
            unsafe {
                libc::shmdt(addr);
                libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
            }
            return None;
        }
        // The segment already lives through both mappings; stripping the SysV
        // name right away means a crashed session cannot leak it.
        unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
        let _ = conn.flush();
        Some(Shm {
            seg,
            addr: addr as *mut u8,
            bytes,
        })
    }

    /// Is the framebuffer the plain 32-bit layout (see
    /// [`is_standard_layout`])?
    fn standard_layout(&self, depth: u8) -> bool {
        is_standard_layout(
            self.lsb,
            self.red_mask,
            self.green_mask,
            self.blue_mask,
            self.bits_per_pixel(depth),
        )
    }

    fn bits_per_pixel(&self, depth: u8) -> u8 {
        self.conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|f| f.depth == depth)
            .map(|f| f.bits_per_pixel)
            .unwrap_or(32)
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

    // ------------------------------------------------------------- input

    pub fn focus(&self, win: Window) {
        let _ = self.conn.set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME);
        let _ = self.conn.flush();
    }

    pub fn warp(&self, win: Window, x: i16, y: i16) {
        let _ = self.conn.warp_pointer(x11rb::NONE, win, 0, 0, 0, 0, x, y);
        let _ = self.conn.flush();
    }

    /// Relative pointer motion through XTEST. Unlike `warp`, this goes through
    /// the device pipeline and produces XInput2 *raw* events, which is what
    /// games (GLFW/Minecraft) use for camera look while the cursor is grabbed.
    /// Verified by examples/xi_probe.rs.
    pub fn relative_motion(&self, dx: i16, dy: i16) {
        if dx == 0 && dy == 0 {
            return;
        }
        if let Ok(cookie) =
            self.conn
                .xtest_fake_input(MOTION_NOTIFY, 1 /* detail=1 -> relative */, 0, self.root, dx, dy, DEVICE_ID)
        {
            let _ = cookie.check();
        }
        let _ = self.conn.flush();
    }

    fn key_event(&self, keycode: u8, down: bool) {
        let type_ = if down { KEY_PRESS } else { KEY_RELEASE };
        if let Ok(cookie) = self
            .conn
            .xtest_fake_input(type_, keycode, 0, self.root, 0, 0, DEVICE_ID)
        {
            let _ = cookie.check();
        }
    }

    pub fn key_press(&self, keycode: u8) {
        self.key_event(keycode, true);
        let _ = self.conn.flush();
    }

    pub fn key_release(&self, keycode: u8) {
        self.key_event(keycode, false);
        let _ = self.conn.flush();
    }

    pub fn button(&self, button: u8, down: bool) {
        let type_ = if down { BUTTON_PRESS } else { BUTTON_RELEASE };
        if let Ok(cookie) = self
            .conn
            .xtest_fake_input(type_, button, 0, self.root, 0, 0, DEVICE_ID)
        {
            let _ = cookie.check();
        }
        let _ = self.conn.flush();
    }

    pub fn keycode_for_keysym(&self, keysym: u32) -> Option<u8> {
        self.key_table.iter().find(|(_, ks)| *ks == keysym).map(|(kc, _)| *kc)
    }

    pub fn shift_keycode(&self) -> Option<u8> {
        self.shift_keycode
    }

    pub fn control_keycode(&self) -> Option<u8> {
        self.control_keycode
    }

    pub fn alt_keycode(&self) -> Option<u8> {
        self.alt_keycode
    }
}
