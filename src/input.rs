//! Terminal → X11 input translation.
//!
//! - keys: crossterm events → XTEST key press/release via the server keymap,
//!   with modifier refcounting (shift/ctrl/alt injected only while needed);
//! - held keys auto-release after a timeout (covers terminals that do not
//!   report key release events);
//! - mouse: terminal cell deltas accumulate → pointer is queried and warped
//!   by the same delta (clamped to the game window);
//! - mouse recentring: while the game has grabbed the pointer the terminal
//!   only reports *absolute* cells, so at the screen edge the pointer stops and
//!   the camera with it. When it reaches the edge we push it back toward the
//!   middle with a virtual uinput mouse (see `crate::uinput`), giving endless
//!   camera look;
//! - Ctrl+Q quits the session.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};

use crate::uinput::VirtualPointer;
use crate::x11::{keysym_for_char, keysym_for_special, GameX11, Geometry};

const HELD_TIMEOUT: Duration = Duration::from_millis(900);

/// How long after a recentring injection terminal motion is treated as our own
/// and skipped (the injected move bounces back as a cell report).
const SETTLE: Duration = Duration::from_millis(100);

/// The pointer counts as "at the edge" within this many cells of the border.
const EDGE_MARGIN: i32 = 1;

/// Upper bound on consecutive recentring attempts for one edge encounter.
const MAX_RETRIES: u8 = 3;

/// Fallback cell size in pixels when neither the pty nor the screen can be
/// measured (refined by calibration after the first recentring anyway).
const DEFAULT_CELL: (f32, f32) = (8.0, 16.0);


#[derive(Clone, Copy, PartialEq)]
struct Mods {
    shift: bool,
    ctrl: bool,
    alt: bool,
}

struct HeldKey {
    keycode: u8,
    mods: Mods,
    last_seen: Instant,
}

pub enum Action {
    Continue,
    Quit,
}

/// A recentring injection waiting to be measured: where the pointer was, and
/// how many cells we asked it to move on each axis.
#[derive(Clone, Copy)]
struct Calib {
    col: u16,
    row: u16,
    dc: i32,
    dr: i32,
}

pub struct Input {
    held: HashMap<u8, HeldKey>,
    /// accumulated pointer delta in game pixels
    pending_dx: i32,
    pending_dy: i32,
    /// last mouse position in game pixels (delta source)
    last_mouse: Option<(i32, i32)>,
    /// absolute game-pixel target for the latest mouse event. Used to keep the
    /// in-game cursor aligned with the terminal cell under the mouse.
    pending_target: Option<(i32, i32)>,
    /// virtual pointer used to recenter the terminal mouse (None = disabled or
    /// uinput unavailable)
    pointer: Option<VirtualPointer>,
    /// last terminal cell the mouse was reported at (edge detection)
    last_cell: Option<(u16, u16)>,
    /// while set, terminal motion is our own injected recentring: skip the
    /// camera delta and re-anchor instead
    settle_until: Option<Instant>,
    /// pending recentring measurement (see `Calib`)
    calib: Option<Calib>,
    /// cell size in pixels, refined by calibration
    cell_w: f32,
    cell_h: f32,
    /// terminal size, kept fresh for edge detection
    cols: usize,
    rows: usize,
    /// consecutive recentring attempts for the current edge encounter
    retries: u8,
}

impl Input {
    /// `recenter` enables the virtual-pointer mouse recentring (see the module
    /// docs); it is silently ignored when uinput is unavailable.
    pub fn new(recenter: bool) -> Self {
        let pointer = recenter.then(VirtualPointer::create).flatten();
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let (cols, rows) = (cols.max(1) as usize, rows.max(1) as usize);
        let (cell_w, cell_h) = estimate_cell(cols, rows);
        Self {
            held: HashMap::new(),
            pending_dx: 0,
            pending_dy: 0,
            last_mouse: None,
            pending_target: None,
            pointer,
            last_cell: None,
            settle_until: None,
            calib: None,
            cell_w,
            cell_h,
            cols,
            rows,
            retries: 0,
        }
    }

    pub fn handle(&mut self, ev: &Event, x11: &GameX11, win: x11rb::protocol::xproto::Window, geom: &Geometry, cols: usize, rows: usize) -> Action {
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        match ev {
            Event::Key(ke) => self.on_key(ke, x11),
            Event::Mouse(me) => {
                self.on_mouse(me, x11, win, geom, cols, rows);
                Action::Continue
            }
            _ => Action::Continue,
        }
    }

    // ---------------------------------------------------------------- keys

    fn on_key(&mut self, ke: &KeyEvent, x11: &GameX11) -> Action {
        // Quit before injecting anything.
        if ke.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(ke.code, KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Char('c'))
        {
            return Action::Quit;
        }

        let mut mods = Mods {
            shift: ke.modifiers.contains(KeyModifiers::SHIFT),
            ctrl: ke.modifiers.contains(KeyModifiers::CONTROL),
            alt: ke.modifiers.contains(KeyModifiers::ALT),
        };

        let keysym: Option<(u32, bool)> = match ke.code {
            KeyCode::Char(c) => keysym_for_char(c),
            KeyCode::BackTab => {
                mods.shift = true;
                Some((0xff09, false))
            }
            other => keysym_for_special(other).map(|s| (s, false)),
        };
        let Some((keysym, needs_shift)) = keysym else {
            return Action::Continue; // unmapped (e.g. modifier-only event)
        };
        if needs_shift {
            mods.shift = true;
        }

        let Some(keycode) = x11.keycode_for_keysym(keysym) else {
            return Action::Continue;
        };

        match ke.kind {
            KeyEventKind::Release => {
                self.release_key(x11, keycode);
                Action::Continue
            }
            KeyEventKind::Press | KeyEventKind::Repeat => {
                self.press_key(x11, keycode, mods);
                Action::Continue
            }
        }
    }

    fn press_key(&mut self, x11: &GameX11, keycode: u8, mods: Mods) {
        if let Some(held) = self.held.get_mut(&keycode) {
            held.last_seen = Instant::now();
            return; // autorepeat or legacy re-press of a held key
        }
        // modifiers first, so the key arrives with the right state
        if mods.shift {
            self.ensure_mod(x11, x11.shift_keycode(), |m| m.shift);
        }
        if mods.ctrl {
            self.ensure_mod(x11, x11.control_keycode(), |m| m.ctrl);
        }
        if mods.alt {
            self.ensure_mod(x11, x11.alt_keycode(), |m| m.alt);
        }
        x11.key_press(keycode);
        self.held.insert(
            keycode,
            HeldKey { keycode, mods, last_seen: Instant::now() },
        );
    }

    fn release_key(&mut self, x11: &GameX11, keycode: u8) {
        if let Some(held) = self.held.remove(&keycode) {
            x11.key_release(held.keycode);
            self.release_unused_mods(x11, &held.mods);
        }
    }

    /// Press a modifier keycode if no currently held key already uses it.
    fn ensure_mod(&mut self, x11: &GameX11, mod_keycode: Option<u8>, which: fn(&Mods) -> bool) {
        let already = self.held.values().any(|h| which(&h.mods));
        if !already
            && let Some(kc) = mod_keycode {
                x11.key_press(kc);
            }
    }

    /// Release modifiers that no held key needs anymore.
    fn release_unused_mods(&mut self, x11: &GameX11, used: &Mods) {
        let shift_still = self.held.values().any(|h| h.mods.shift);
        let ctrl_still = self.held.values().any(|h| h.mods.ctrl);
        let alt_still = self.held.values().any(|h| h.mods.alt);
        if used.shift && !shift_still
            && let Some(kc) = x11.shift_keycode() {
                x11.key_release(kc);
            }
        if used.ctrl && !ctrl_still
            && let Some(kc) = x11.control_keycode() {
                x11.key_release(kc);
            }
        if used.alt && !alt_still
            && let Some(kc) = x11.alt_keycode() {
                x11.key_release(kc);
            }
    }

    /// Auto-release held keys with no fresh terminal events (terminals that
    /// do not report releases; autorepeat keeps `last_seen` fresh).
    pub fn sweep(&mut self, x11: &GameX11) {
        let stale: Vec<u8> = self
            .held
            .values()
            .filter(|h| h.last_seen.elapsed() > HELD_TIMEOUT)
            .map(|h| h.keycode)
            .collect();
        for kc in stale {
            self.release_key(x11, kc);
        }
    }

    /// Release everything (session teardown).
    pub fn release_all(&mut self, x11: &GameX11) {
        let all: Vec<u8> = self.held.keys().copied().collect();
        for kc in all {
            self.release_key(x11, kc);
        }
        // modifiers may remain if their refcount bookkeeping raced
        for kc in [x11.shift_keycode(), x11.control_keycode(), x11.alt_keycode()]
            .into_iter()
            .flatten()
        {
            x11.key_release(kc);
        }
    }

    // --------------------------------------------------------------- mouse

    fn on_mouse(&mut self, me: &MouseEvent, x11: &GameX11, win: x11rb::protocol::xproto::Window, geom: &Geometry, cols: usize, rows: usize) {
        let gw = geom.width as i32;
        let gh = geom.height as i32;
        if gw == 0 || gh == 0 || cols == 0 || rows == 0 {
            return;
        }
        // terminal cell → game pixel (proportional, robust to resampling)
        let gx = (me.column as i64 * gw as i64 / cols as i64) as i32;
        let gy = (me.row as i64 * gh as i64 / rows as i64) as i32;

        match me.kind {
            MouseEventKind::Down(btn) => {
                self.track_mouse_pos(x11, win, geom, gx, gy);
                if let Some(b) = map_button(btn) {
                    x11.button(b, true);
                }
            }
            MouseEventKind::Up(btn) => {
                self.track_mouse_pos(x11, win, geom, gx, gy);
                if let Some(b) = map_button(btn) {
                    x11.button(b, false);
                }
            }
            MouseEventKind::ScrollUp => tap_button(x11, 4),
            MouseEventKind::ScrollDown => tap_button(x11, 5),
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {}
            MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                self.on_motion(me.column, me.row, gx, gy, x11);
            }
        }
    }

    fn track_mouse_pos(&mut self, x11: &GameX11, win: x11rb::protocol::xproto::Window, geom: &Geometry, gx: i32, gy: i32) {
        // clicks also re-anchor the delta source to avoid a jump on the next move
        self.last_mouse = Some((gx, gy));
        // While the game has grabbed the pointer (gameplay) it drives the
        // crosshair from the deltas; warping would yank the camera around, so
        // only menus — where the cursor is visible — get an absolute pointer.
        if !x11.cursor_hidden() {
            let x = gx.clamp(0, geom.width as i32 - 1) as i16;
            let y = gy.clamp(0, geom.height as i32 - 1) as i16;
            x11.warp(win, x, y);
        }
    }

    // ---------------------------------------------------- mouse recentring

    /// A pointer move: accumulate the camera delta, and when the pointer has
    /// reached the terminal edge recenter it so the user can keep turning.
    fn on_motion(&mut self, col: u16, row: u16, gx: i32, gy: i32, x11: &GameX11) {
        if self.settling() {
            // this is our own recentring bounce (or input that raced it):
            // measure it and re-anchor without moving the camera
            self.measure(col, row);
            self.last_mouse = Some((gx, gy));
            self.last_cell = Some((col, row));
            self.pending_target = Some((gx, gy));
            return;
        }
        if let Some((lx, ly)) = self.last_mouse {
            self.pending_dx += gx - lx;
            self.pending_dy += gy - ly;
        }
        self.last_mouse = Some((gx, gy));
        self.last_cell = Some((col, row));
        self.pending_target = Some((gx, gy));
        self.recenter_if_edge(col, row, x11);
    }

    fn settling(&self) -> bool {
        matches!(self.settle_until, Some(t) if Instant::now() < t)
    }

    fn recenter_if_edge(&mut self, col: u16, row: u16, x11: &GameX11) {
        if self.pointer.is_none() || !x11.cursor_hidden() {
            self.retries = 0;
            return;
        }
        match recenter_target(col, row, self.cols, self.rows) {
            Some((tc, tr)) => self.inject_recenter(col, row, tc, tr),
            None => self.retries = 0,
        }
    }

    /// Push the pointer from `col`/`row` back to `tc`/`tr` with the virtual
    /// mouse and start the settle window during which the resulting report is
    /// treated as our own.
    fn inject_recenter(&mut self, col: u16, row: u16, tc: u16, tr: u16) {
        let (dc, dr) = (tc as i32 - col as i32, tr as i32 - row as i32);
        if dc == 0 && dr == 0 {
            return;
        }
        let dx = (dc as f32 * self.cell_w).round() as i32;
        let dy = (dr as f32 * self.cell_h).round() as i32;
        if let Some(vp) = self.pointer.as_ref() {
            vp.move_rel(dx, dy);
        }
        self.calib = Some(Calib { col, row, dc, dr });
        // Optimistically assume it lands where we aimed so a lost report cannot
        // leave a stale delta source behind.
        self.last_cell = Some((tc, tr));
        self.last_mouse = None;
        self.settle_until = Some(Instant::now() + SETTLE);
    }

    /// Turn the pending recentring into a fresh cell-size estimate. The move we
    /// asked for is measured in pixels, the observed one in cells, so their
    /// ratio is pixels-per-cell; one or two of these converge even from a rough
    /// initial guess.
    fn measure(&mut self, col: u16, row: u16) {
        let Some(c) = self.calib.take() else { return };
        if c.dc != 0 {
            let moved = col as i32 - c.col as i32;
            if moved.abs() >= 2 {
                let est = (c.dc as f32 * self.cell_w) / moved as f32;
                if (3.0..=60.0).contains(&est) {
                    self.cell_w = self.cell_w * 0.5 + est * 0.5;
                }
            }
        }
        if c.dr != 0 {
            let moved = row as i32 - c.row as i32;
            if moved.abs() >= 2 {
                let est = (c.dr as f32 * self.cell_h) / moved as f32;
                if (6.0..=120.0).contains(&est) {
                    self.cell_h = self.cell_h * 0.5 + est * 0.5;
                }
            }
        }
    }

    /// Close the settle window: if the pointer is still stuck at the edge (the
    /// estimate was too small, or it never crossed a cell) try once more, up to
    /// `MAX_RETRIES` times per edge encounter, with a grown estimate.
    fn finish_settle(&mut self, x11: &GameX11) {
        let Some(until) = self.settle_until else { return };
        if Instant::now() < until {
            return;
        }
        self.settle_until = None;
        // a leftover measurement means no report arrived: the move never
        // crossed a cell, so assume we are still on the border
        let missing = self.calib.take();
        let cell = match (missing, self.last_cell) {
            (Some(c), _) => (c.col, c.row),
            (None, Some(cell)) => cell,
            (None, None) => {
                self.retries = 0;
                return;
            }
        };
        if self.pointer.is_none() || !x11.cursor_hidden() || self.retries >= MAX_RETRIES {
            self.retries = 0;
            return;
        }
        match recenter_target(cell.0, cell.1, self.cols, self.rows) {
            Some((tc, tr)) => {
                self.retries += 1;
                self.cell_w = (self.cell_w * 1.3).min(60.0);
                self.cell_h = (self.cell_h * 1.3).min(120.0);
                self.inject_recenter(cell.0, cell.1, tc, tr);
            }
            None => self.retries = 0,
        }
    }

    /// Inject the accumulated mouse movement into the game.
    ///
    /// In menus the pointer is warped to the absolute position that corresponds
    /// to the terminal cell under the mouse, so the cursor keeps matching the
    /// terminal and clicks land where the user points.
    ///
    /// In gameplay the game has grabbed the pointer and hidden the cursor: only
    /// XTEST *relative* motion is sent (the one path that produces XInput2 raw
    /// events, see examples/xi_probe.rs), and warping is suppressed entirely —
    /// otherwise the forced absolute pointer would fight the game's own cursor
    /// recentring and the camera would be uncontrollable.
    pub fn apply_mouse(
        &mut self,
        x11: &GameX11,
        win: x11rb::protocol::xproto::Window,
        geom: &Geometry,
    ) {
        self.finish_settle(x11);
        let camera = x11.cursor_hidden();
        if self.pending_dx != 0 || self.pending_dy != 0 {
            x11.relative_motion(
                self.pending_dx.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                self.pending_dy.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            );
        }
        let target = self.pending_target.take();
        if !camera && let Some((tx, ty)) = target {
            let x = tx.clamp(0, geom.width as i32 - 1) as i16;
            let y = ty.clamp(0, geom.height as i32 - 1) as i16;
            x11.warp(win, x, y);
        }
        self.pending_dx = 0;
        self.pending_dy = 0;
    }
}

/// Where to recenter to, given a 0-based terminal cell (the convention
/// crossterm parses mouse reports into: the wire coordinates are 1-based and
/// crossterm subtracts 1). `Some` only when the cell sits on the border of a
/// `cols` x `rows` terminal; the target is the centre cell, which leaves the
/// most room to keep moving in any direction.
fn recenter_target(col: u16, row: u16, cols: usize, rows: usize) -> Option<(u16, u16)> {
    let (c, r) = (col as i32, row as i32);
    let (cols, rows) = (cols.max(1) as i32, rows.max(1) as i32);
    let at_edge = c < EDGE_MARGIN
        || c >= cols - EDGE_MARGIN
        || r < EDGE_MARGIN
        || r >= rows - EDGE_MARGIN;
    at_edge.then_some(((cols / 2) as u16, (rows / 2) as u16))
}

/// Cell size in pixels, from the pty size if it reports pixels, else the screen
/// size (equal for the fullscreen terminals the launcher requires). Refined at
/// runtime by `Input::measure`.
fn estimate_cell(cols: usize, rows: usize) -> (f32, f32) {
    let (cols, rows) = (cols.max(1) as f32, rows.max(1) as f32);
    let pixels = crate::display::terminal_pixels().or_else(crate::display::screen_pixels);
    match pixels {
        Some((w, h)) if w > 0 && h > 0 => (w as f32 / cols, h as f32 / rows),
        _ => DEFAULT_CELL,
    }
}

fn map_button(btn: MouseButton) -> Option<u8> {
    Some(match btn {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
    })
}

fn tap_button(x11: &GameX11, b: u8) {
    x11.button(b, true);
    x11.button(b, false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recenter_only_at_the_border() {
        // 80x24 cells are numbered 0..79 and 0..23; the border cells are
        // col 0/79 and row 0/23.
        assert!(recenter_target(40, 12, 80, 24).is_none());
        assert_eq!(recenter_target(0, 12, 80, 24), Some((40, 12)));
        assert_eq!(recenter_target(79, 12, 80, 24), Some((40, 12)));
        assert_eq!(recenter_target(40, 0, 80, 24), Some((40, 12)));
        assert_eq!(recenter_target(40, 23, 80, 24), Some((40, 12)));
        // corners recenter too
        assert_eq!(recenter_target(79, 23, 80, 24), Some((40, 12)));
    }

    #[test]
    fn recenter_target_is_inside_the_terminal() {
        for &(cols, rows) in &[(80usize, 24usize), (3, 3), (200, 60)] {
            let (tc, tr) = recenter_target(0, 0, cols, rows).unwrap();
            assert!((tc as usize) < cols);
            assert!((tr as usize) < rows);
        }
    }

    #[test]
    fn calibration_recovers_cell_size_from_movement() {
        let mut input = Input {
            held: HashMap::new(),
            pending_dx: 0,
            pending_dy: 0,
            last_mouse: None,
            pending_target: None,
            pointer: None,
            last_cell: None,
            settle_until: None,
            calib: None,
            cell_w: 8.0,
            cell_h: 16.0,
            cols: 80,
            rows: 24,
            retries: 0,
        };
        // the real cell is 12px, we start believing 8px; each round we inject
        // `dc * cell_w` pixels and observe how many whole cells that moved
        let truth = 12.0f32;
        let dc = -39i32;
        for _ in 0..6 {
            let dx = (dc as f32 * input.cell_w).round();
            let moved = (dx / truth).round();
            input.calib = Some(Calib { col: 80, row: 12, dc, dr: 0 });
            input.measure((80.0 + moved) as u16, 12);
        }
        assert!((input.cell_w - truth).abs() < 1.0, "got {}", input.cell_w);
    }

    #[test]
    fn measure_ignores_tiny_or_absurd_moves() {
        let mut input = Input::new(false);
        // moved less than a cell: no signal, estimate untouched
        input.cell_w = 8.0;
        input.calib = Some(Calib { col: 80, row: 12, dc: -39, dr: 0 });
        input.measure(79, 12);
        assert_eq!(input.cell_w, 8.0);
        // absurd ratio (pointer only moved one cell over 300px) is rejected
        input.cell_w = 8.0;
        input.calib = Some(Calib { col: 80, row: 12, dc: -39, dr: 0 });
        input.measure(78, 12);
        assert_eq!(input.cell_w, 8.0);
    }
}

