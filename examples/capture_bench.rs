//! Capture path benchmark: plain `get_image` versus the MIT-SHM capture the
//! session uses, against a real Xvfb run at the resolution the launcher runs
//! at (default 1280x720). A pattern is drawn between grabs so every one
//! returns fresh data.
//!
//! Run:  cargo run --release --example capture_bench -- 1280 720

use std::time::Instant;

use x11rb::connection::Connection;
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xproto::{
    ConnectionExt as _, CreateGCAux, CreateWindowAux, Gcontext, ImageFormat, Rectangle,
    WindowClass,
};

struct Seg {
    seg: u32,
    addr: *mut u8,
}

impl Drop for Seg {
    fn drop(&mut self) {
        // mirrors GameX11's `Shm::drop`
        unsafe { libc::shmdt(self.addr as *mut libc::c_void) };
    }
}

fn main() -> anyhow::Result<()> {
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":99".into());
    let mut args = std::env::args().skip(1);
    let w: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(1280);
    let h: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(720);

    let (conn, screen_num) = x11rb::connect(Some(&display))?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    println!(
        "display {display}, screen {}x{}, target {w}x{h}",
        screen.width_in_pixels, screen.height_in_pixels
    );

    let win = conn.generate_id()?;
    conn.create_window(
        0,
        win,
        root,
        0,
        0,
        w as u16,
        h as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new().background_pixel(0x101c30),
    )?;
    conn.map_window(win)?;
    conn.flush()?;

    let gc: Gcontext = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new().foreground(0x7fd4ff))?;
    conn.flush()?;

    // --- attach a MIT-SHM segment (same as GameX11::attach_shm)
    let bytes = w * h * 4;
    let shmseg = conn.generate_id()?;
    let shmid = unsafe { libc::shmget(libc::IPC_PRIVATE, bytes, libc::IPC_CREAT | 0o600) };
    if shmid < 0 {
        anyhow::bail!("shmget failed (sysv shared memory unavailable)");
    }
    let addr = unsafe { libc::shmat(shmid, std::ptr::null(), 0) };
    if addr as isize == -1 {
        anyhow::bail!("shmat failed");
    }
    conn.shm_attach(shmseg, shmid as u32, false)?.check()?;
    unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
    let seg = Seg { seg: shmseg, addr: addr as *mut u8 };
    conn.flush()?;

    // warm up so page faults and first-use costs do not skew the timing
    for _ in 0..5 {
        let _ = conn
            .get_image(ImageFormat::Z_PIXMAP, win, 0, 0, w as u16, h as u16, !0)?
            .reply()?;
        let _ = conn
            .shm_get_image(win, 0, 0, w as u16, h as u16, !0, u8::from(ImageFormat::Z_PIXMAP), seg.seg, 0)?
            .reply()?;
    }

    const N: usize = 200;
    let tick = |i: usize, x_off: i16, y_off: i16| {
        let _ = conn.poly_fill_rectangle(
            win,
            gc,
            &[Rectangle {
                x: ((i * 37 + x_off as usize) % w) as i16,
                y: y_off,
                width: 9,
                height: 9,
            }],
        );
        conn.flush().ok();
    };

    let t0 = Instant::now();
    for i in 0..N {
        let _ = conn
            .get_image(ImageFormat::Z_PIXMAP, win, 0, 0, w as u16, h as u16, !0)?
            .reply()?;
        if i % 16 == 0 {
            tick(i, 0, 3);
        }
    }
    let plain_ms = t0.elapsed().as_secs_f64() * 1000.0 / N as f64;

    let t0 = Instant::now();
    for i in 0..N {
        let _ = conn
            .shm_get_image(win, 0, 0, w as u16, h as u16, !0, u8::from(ImageFormat::Z_PIXMAP), seg.seg, 0)?
            .reply()?;
        if i % 16 == 0 {
            tick(i, 5, 11);
        }
    }
    let shm_ms = t0.elapsed().as_secs_f64() * 1000.0 / N as f64;

    // sanity: the shm copy must agree with the reply bytes of the same content
    let plain_data = conn
        .get_image(ImageFormat::Z_PIXMAP, win, 0, 0, w as u16, h as u16, !0)?
        .reply()?
        .data;
    let _ = conn
        .shm_get_image(win, 0, 0, w as u16, h as u16, !0, u8::from(ImageFormat::Z_PIXMAP), seg.seg, 0)?
        .reply()?;
    let shm_view =
        unsafe { std::slice::from_raw_parts(seg.addr as *const u8, plain_data.len()) };
    let mismatches = plain_data.iter().zip(shm_view).filter(|(a, b)| a != b).count();

    println!("bytes compared {}, mismatches {}", plain_data.len(), mismatches);
    println!("avg get_image : {plain_ms:6.2} ms");
    println!("avg shm       : {shm_ms:6.2} ms");
    println!("speedup       : {:5.1}x", plain_ms / shm_ms.max(1e-9));
    Ok(())
}
