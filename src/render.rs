//! ANSI half-block frame renderer with frame diffing.
//!
//! One terminal cell = two vertically stacked pixels: glyph '▀',
//! foreground = top pixel, bottom = bottom pixel (24-bit color each).
//! Only cells that changed since the previous frame are rewritten.

use std::fmt::Write as _;

/// A captured game frame the renderer can sample.
///
/// Two byte layouts are supported so the capture can hand us whatever the
/// server produced without running an intermediate byte-swap: packed RGB (the
/// classic get-image decode) and the X framebuffer layout (one 4-byte word per
/// pixel, byte 0 = blue, stride = width*4), which is both what `get_image`
/// returns and what MIT-SHM puts into a shared segment. Sampling the X layout
/// directly removes one full-frame copy per frame.
pub struct Frame<'a> {
    pub w: usize,
    pub h: usize,
    data: &'a [u8],
    bgra: bool,
}

impl<'a> Frame<'a> {
    /// Tightly packed RGB, 3 bytes per pixel, row-major.
    pub fn rgb(data: &'a [u8], w: usize, h: usize) -> Self {
        Self { w, h, data, bgra: false }
    }

    /// X framebuffer words (BGRA8888 = one 32-bit word per pixel, byte 0 =
    /// blue on little-endian Xvfb, row stride `w * 4`).
    pub fn bgra(data: &'a [u8], w: usize, h: usize) -> Self {
        Self { w, h, data, bgra: true }
    }

    #[inline]
    fn pixel(&self, i: usize) -> [u8; 3] {
        if self.bgra {
            let o = i * 4;
            [self.data[o + 2], self.data[o + 1], self.data[o]]
        } else {
            let o = i * 3;
            [self.data[o], self.data[o + 1], self.data[o + 2]]
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub struct Cell {
    pub fg: [u8; 3],
    pub bg: [u8; 3],
}

pub struct Renderer {
    prev: Vec<Cell>,
    cols: usize,
    rows: usize,
    /// when set, the next render repaints every cell regardless of `prev`
    force_full: bool,
}

/// Game pixels covered by half-pixel `p` of `total` half-pixels along an axis
/// `g` pixels long (end exclusive, never empty).
fn span(p: usize, total: usize, g: usize) -> (usize, usize) {
    let lo = (p * g / total).min(g - 1);
    let hi = ((p + 1) * g / total).max(lo + 1).min(g);
    (lo, hi)
}

impl Renderer {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            prev: vec![Cell { fg: [0; 3], bg: [0; 3] }; cols * rows],
            cols,
            rows,
            force_full: true,
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.cols = cols;
        self.rows = rows;
        self.prev.clear();
        self.prev.resize(cols * rows, Cell { fg: [0; 3], bg: [0; 3] });
        self.force_full = true;
    }

    /// Forget the previous frame: the next `render` repaints everything.
    /// Needed whenever something else (a launch screen, a hint) has touched
    /// the terminal outside of the frame diff.
    pub fn invalidate(&mut self) {
        self.force_full = true;
    }

    /// Average the game pixels that fall inside one terminal pixel (box
    /// filter). Nearest-neighbour would alias badly whenever the game renders
    /// larger than the terminal, which is now the normal case: averaging keeps
    /// text and edges readable.
    fn sample(&self, frame: &Frame, x0: usize, x1: usize, y0: usize, y1: usize) -> [u8; 3] {
        let gw = frame.w;
        let x0 = x0.min(gw - 1);
        let x1 = x1.clamp(x0 + 1, gw);
        let y0 = y0.min(frame.h - 1);
        let y1 = y1.clamp(y0 + 1, frame.h);
        // bound the work: at most 4 samples per axis
        let step_x = (x1 - x0).div_ceil(4);
        let step_y = (y1 - y0).div_ceil(4);
        let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
        let mut y = y0;
        while y < y1 {
            let mut x = x0;
            while x < x1 {
                let [cr, cg, cb] = frame.pixel(y * gw + x);
                r += cr as u32;
                g += cg as u32;
                b += cb as u32;
                n += 1;
                x += step_x;
            }
            y += step_y;
        }
        [(r / n) as u8, (g / n) as u8, (b / n) as u8]
    }

    /// Convert a game frame into a diffed ANSI string laid out for the current
    /// terminal size. Box-filter sampling covers any mismatch between game
    /// resolution and terminal size.
    pub fn render(&mut self, frame: &Frame, out: &mut String) -> usize {
        out.clear();
        let (cols, rows) = (self.cols, self.rows);
        let (gw, gh) = (frame.w, frame.h);
        if cols == 0 || rows == 0 || gw == 0 || gh == 0 {
            return 0;
        }

        let mut cur_fg: Option<[u8; 3]> = None;
        let mut cur_bg: Option<[u8; 3]> = None;
        let mut cursor_ok = false;
        let mut written = 0usize;
        let full = std::mem::take(&mut self.force_full);

        for row in 0..rows {
            for col in 0..cols {
                // map cell pixel grid (cols x rows*2) onto the game frame
                let x0 = col * gw / cols;
                let x1 = ((col + 1) * gw / cols).max(x0 + 1);
                let (yt0, yt1) = span(row * 2, rows * 2, gh);
                let (yb0, yb1) = span(row * 2 + 1, rows * 2, gh);
                let cell = Cell {
                    fg: self.sample(frame, x0, x1, yt0, yt1),
                    bg: self.sample(frame, x0, x1, yb0, yb1),
                };
                let idx = row * cols + col;
                if !full && self.prev[idx] == cell {
                    cursor_ok = false;
                    continue;
                }
                if !cursor_ok {
                    // reposition; SGR colour state survives a cursor move, so
                    // it is NOT reset here (that only costs bytes)
                    write!(out, "\x1b[{};{}H", row + 1, col + 1).ok();
                }
                // one escape when both colours change, otherwise only the one
                // that actually changed — keeps the byte stream minimal
                let new_fg = cur_fg != Some(cell.fg);
                let new_bg = cur_bg != Some(cell.bg);
                if new_fg && new_bg {
                    write!(
                        out,
                        "\x1b[38;2;{};{};{};48;2;{};{};{}m",
                        cell.fg[0], cell.fg[1], cell.fg[2], cell.bg[0], cell.bg[1], cell.bg[2]
                    )
                    .ok();
                    cur_fg = Some(cell.fg);
                    cur_bg = Some(cell.bg);
                } else if new_fg {
                    write!(out, "\x1b[38;2;{};{};{}m", cell.fg[0], cell.fg[1], cell.fg[2]).ok();
                    cur_fg = Some(cell.fg);
                } else if new_bg {
                    write!(out, "\x1b[48;2;{};{};{}m", cell.bg[0], cell.bg[1], cell.bg[2]).ok();
                    cur_bg = Some(cell.bg);
                }
                out.push('▀');
                self.prev[idx] = cell;
                cursor_ok = true;
                written += 1;
            }
            cursor_ok = false;
        }
        if written > 0 {
            out.push_str("\x1b[0m");
        }
        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_filter_averages_the_covered_pixels() {
        // 2x2 frame, black left column and white right column
        let rgb = [0, 0, 0, 255, 255, 255, 0, 0, 0, 255, 255, 255];
        let frame = Frame::rgb(&rgb, 2, 2);
        // one cell covering the whole frame becomes mid grey
        assert_eq!(Renderer::sample(&Renderer::new(1, 1), &frame, 0, 2, 0, 2), [127, 127, 127]);
        // the left half stays black, the right half stays white
        assert_eq!(Renderer::sample(&Renderer::new(1, 1), &frame, 0, 1, 0, 2), [0, 0, 0]);
        assert_eq!(Renderer::sample(&Renderer::new(1, 1), &frame, 1, 2, 0, 2), [255, 255, 255]);
    }

    #[test]
    fn bgra_and_rgb_sample_the_same_pixels() {
        // pixel 0 = black, pixel 1 = white — the X layout is blue, green, red
        let rgb = [0, 0, 0, 255, 255, 255];
        let bgra = [0, 0, 0, 0, 255, 255, 255, 0];
        let frgb = Frame::rgb(&rgb, 2, 1);
        let fbgra = Frame::bgra(&bgra, 2, 1);
        assert_eq!(frgb.pixel(0), fbgra.pixel(0));
        assert_eq!(frgb.pixel(1), fbgra.pixel(1));
        assert_eq!(fbgra.pixel(1), [255, 255, 255]);
    }

    #[test]
    fn half_pixel_spans_cover_every_pixel() {
        // half-pixel ranges must tile the axis with no gaps, for any game size
        for g in 1..=40usize {
            for half in 1..=12usize {
                let mut covered = vec![0u32; g];
                for p in 0..half {
                    let (lo, hi) = span(p, half, g);
                    assert!(lo < hi && hi <= g, "bad span for g={g} half={half}");
                    for c in covered[lo..hi].iter_mut() {
                        *c += 1;
                    }
                }
                assert!(covered.iter().all(|&c| c >= 1), "gap at g={g} half={half}");
            }
        }
    }

    #[test]
    fn full_repaint_moves_the_cursor_once_per_row() {
        let (w, h) = (16, 6);
        let mut r = Renderer::new(w, h);
        // a noisy frame larger than the cell grid: every cell differs
        let (gw, gh) = (w * 4, h * 8);
        let rgb: Vec<u8> = (0..gw * gh * 3).map(|i| (i % 251) as u8).collect();
        let frame = Frame::rgb(&rgb, gw, gh);
        let mut out = String::new();
        assert_eq!(r.render(&frame, &mut out), w * h);
        // exactly one absolute cursor move per row, not one per cell
        assert_eq!(out.matches('H').count(), h, "cursor churn");
    }

    #[test]
    fn flat_frames_reuse_the_colour_state() {
        let (w, h) = (20, 5);
        let mut r = Renderer::new(w, h);
        // a larger game frame of one single colour
        let (gw, gh) = (w * 2, h * 4);
        let rgb = vec![90u8; gw * gh * 3];
        let frame = Frame::rgb(&rgb, gw, gh);
        let mut out = String::new();
        assert_eq!(r.render(&frame, &mut out), w * h);
        // colour is set once; every other cell costs only the 3-byte glyph
        assert!(out.len() < w * h * 6, "{} bytes for {} cells", out.len(), w * h);
    }

    #[test]
    fn both_colours_share_a_single_escape() {
        let (w, h) = (4, 1);
        let mut r = Renderer::new(w, h);
        // 4x2 game frame (one cell row), left black / right white
        let rgb = [
            0, 0, 0, 0, 0, 0, 255, 255, 255, 255, 255, 255, // top row
            0, 0, 0, 0, 0, 0, 255, 255, 255, 255, 255, 255, // bottom row
        ];
        let frame = Frame::rgb(&rgb, 4, 2);
        let mut out = String::new();
        r.render(&frame, &mut out);
        // two cells written, each with a combined 38;2;…;48;2;…m escape
        assert_eq!(out.matches("\x1b[38;2;").count(), 2);
        assert_eq!(out.matches("\x1b[48;2;").count(), 0);
    }

    #[test]
    fn unchanged_frames_render_nothing() {
        let mut r = Renderer::new(4, 2);
        let rgb = vec![10u8; 4 * 4 * 3]; // gw=4, gh=4
        let frame = Frame::rgb(&rgb, 4, 4);
        let mut out = String::new();
        assert!(r.render(&frame, &mut out) > 0);
        // the same frame twice: the second pass must produce no output at all
        assert_eq!(r.render(&frame, &mut out), 0);
        // a different frame is written again
        let rgb2 = vec![200u8; 4 * 4 * 3];
        let frame2 = Frame::rgb(&rgb2, 4, 4);
        assert!(r.render(&frame2, &mut out) > 0);
        // resizing forces a full repaint of the next frame
        r.resize(2, 2);
        assert!(r.render(&frame2, &mut out) > 0);
    }
}
