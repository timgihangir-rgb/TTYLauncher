//! The game session: terminal setup, Xvfb + Minecraft lifecycle,
//! capture → render loop, input forwarding, guaranteed cleanup.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use crossterm::cursor::{Hide, Show};
use crossterm::execute;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};

use crate::config::Config;
use crate::input::{Action, Input};
use crate::mc;
use crate::render::Renderer;
use crate::x11::GameX11;
use crate::xvfb::Xvfb;

/// Restores the terminal to a sane state on drop (including panics).
struct TermGuard;

impl TermGuard {
    fn setup() -> Result<Self> {
        enable_raw_mode().context("enable raw mode")?;
        let mut out = io::stdout();
        execute!(out, EnterAlternateScreen, Hide, EnableMouseCapture)?;
        // Ask for distinct press/repeat/release events (kitty keyboard
        // protocol). The release is what lets us lift a game key the moment the
        // user lets go instead of guessing with a timeout; the repeat just
        // keeps the hold alive. REPORT_ALL_KEYS_AS_ESCAPE_CODES is required for
        // plain-text keys (WASD, space) to carry an event type at all.
        execute!(
            out,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
            )
        )?;
        out.flush()?;
        Ok(Self)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = execute!(out, PopKeyboardEnhancementFlags);
        let _ = execute!(out, DisableMouseCapture, Show, LeaveAlternateScreen);
        let _ = out.write_all(b"\x1b[0m");
        let _ = out.flush();
        let _ = disable_raw_mode();
    }
}

pub struct SessionParams {
    pub version: String,
    pub game_w: u16,
    pub game_h: u16,
    pub fps: u32,
    pub xmx_mb: u32,
    pub username: String,
}

/// Performance summary of a finished session, shown in the menu afterwards.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionStats {
    /// Frames captured by the render loop.
    pub frames: u64,
    /// Frames that actually changed on screen (the game's own redraws).
    pub changed: u64,
    /// Wall-clock seconds the render loop ran.
    pub secs: f64,
}

impl SessionStats {
    pub fn fps(&self) -> f64 {
        if self.secs > 0.0 {
            self.frames as f64 / self.secs
        } else {
            0.0
        }
    }

    pub fn game_fps(&self) -> f64 {
        if self.secs > 0.0 {
            self.changed as f64 / self.secs
        } else {
            0.0
        }
    }
}

/// Owns the Minecraft process and kills it when the session ends, no matter
/// how it ends (normal exit, `?` early return, panic). A plain
/// `std::process::Child` does *not* kill its process on drop.
struct GameProcess(Option<std::process::Child>);

impl GameProcess {
    fn new(child: std::process::Child) -> Self {
        Self(Some(child))
    }

    /// Returns the exit status if the game has already stopped.
    fn exited(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        match self.0.as_mut() {
            Some(c) => c.try_wait(),
            None => Ok(None),
        }
    }
}

impl Drop for GameProcess {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Terminal size in cells with sane fallbacks (some PTYs report 0x0).
pub fn terminal_cells() -> (usize, usize) {
    let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
    (w.max(20) as usize, h.max(6) as usize)
}

/// Run one game session. Returns Ok when the user quits (Ctrl+Q);
/// errors carry the tail of the Minecraft session log.
pub fn run(config: &Config, params: &SessionParams) -> Result<SessionStats> {
    let _guard = TermGuard::setup()?;

    let (mut cols, mut rows) = terminal_cells();
    let mut renderer = Renderer::new(cols, rows);

    let started = Instant::now();
    let mut tick = 0usize;
    launch_screen(&params.version, "starting the virtual display", started, tick)?;

    // --- virtual display sized for the game window
    let xvfb = Xvfb::spawn(params.game_w.max(640), params.game_h.max(480))
        .context("spawn Xvfb")?;
    let display = xvfb.display_string();

    let x11 = GameX11::connect_display(&display).context("connect to Xvfb")?;

    // --- launch Minecraft
    let log_path = Config::session_log_path();
    let launch = mc::LaunchArgs {
        version: params.version.clone(),
        username: params.username.clone(),
        xmx_mb: params.xmx_mb,
        width: params.game_w,
        height: params.game_h,
        session_log: log_path.clone(),
        low_end: config.low_end,
    };
    tick += 1;
    launch_screen(&params.version, "launching Java and Minecraft", started, tick)?;
    let mut game = GameProcess::new(mc::spawn(&config.game_dir, &launch, &display)?);

    // --- wait for the game window (up to 90s; cold java start can be slow)
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut win = None;
    while Instant::now() < deadline {
        if let Some(status) = game.exited()? {
            let tail = mc::tail_of_log(&log_path, 15);
            bail!("Minecraft exited during startup ({status}):\n{tail}");
        }
        if let Some(w) = x11.find_window("minecraft") {
            win = Some(w);
            break;
        }
        tick += 1;
        launch_screen(&params.version, "waiting for the game window", started, tick)?;
        std::thread::sleep(Duration::from_millis(250));
    }
    let Some(win) = win else {
        let tail = mc::tail_of_log(&log_path, 15);
        bail!("Minecraft window did not appear within 90s:\n{tail}");
    };

    std::thread::sleep(Duration::from_millis(400)); // let the GL context settle
    let mut geom = x11.geometry(win).context("window geometry")?;
    x11.focus(win);
    x11.warp(win, geom.width as i16 / 2, geom.height as i16 / 2);

    // show the controls for a moment, then hand the screen over to the game
    launch_screen(
        &params.version,
        "playing — Ctrl+Q quits · mouse looks · WASD moves",
        started,
        tick,
    )?;
    std::thread::sleep(Duration::from_millis(1200));
    renderer.invalidate();
    print!("\x1b[2J\x1b[H");
    io::stdout().flush()?;

    // --- capture / render loop
    //
    // `interval` is the *current* capture period. It starts at the configured
    // frame rate and backs off (up to IDLE_INTERVAL) while nothing changes on
    // screen: a static screen then costs almost no CPU, which weak machines
    // can spend on rendering the game instead. Any change snaps it back.
    const IDLE_INTERVAL: Duration = Duration::from_millis(200);
    let target = Duration::from_micros(1_000_000 / params.fps.max(1) as u64);
    let mut interval = target;
    let loop_start = Instant::now();
    let mut stats = SessionStats::default();
    let mut input = Input::new(config.mouse_recenter);
    let mut rgb: Vec<u8> = Vec::new();
    let mut out = String::with_capacity(cols * rows * 16);
    let mut stdout = io::stdout();
    let mut next_geom_refresh = Instant::now() + Duration::from_secs(5);

    // Optional `TTYCRAFT_TRACE=1`: per-second breakdown of where each frame's
    // time goes (capture / render / terminal write) plus bytes sent. Written to
    // `<config dir>/trace.log`. Cheap and off by default.
    let trace = std::env::var_os("TTYCRAFT_TRACE").is_some();
    let mut trace_log = String::new();
    let mut trace_t0 = Instant::now();
    let mut trace_acc = [Duration::ZERO; 3]; // capture, render, write
    let mut trace_bytes = 0usize;
    let mut trace_frames = 0usize;

    loop {
        let frame_start = Instant::now();
        let mut quit = crate::signals::shutdown_requested();

        // drain terminal events until the next frame is due
        loop {
            let elapsed = frame_start.elapsed();
            if elapsed >= interval {
                break;
            }
            match crossterm::event::poll(interval - elapsed) {
                Ok(true) => {}
                // no input pending within the budget
                Ok(false) => break,
                // the terminal went away (e.g. the pty closed): end the session
                Err(_) => {
                    quit = true;
                    break;
                }
            }
            match crossterm::event::read() {
                Ok(Event::Resize(w, h)) => {
                    cols = w as usize;
                    rows = h as usize;
                    renderer.resize(cols, rows);
                }
                Ok(other) => {
                    if matches!(input.handle(&other, &x11, win, &geom, cols, rows), Action::Quit) {
                        quit = true;
                    }
                }
                Err(_) => {
                    quit = true;
                    break;
                }
            }
            if quit {
                break;
            }
        }
        if quit {
            break;
        }

        // did the game exit on its own?
        if let Some(status) = game.exited()? {
            let tail = mc::tail_of_log(&log_path, 20);
            bail!("Minecraft exited ({status}):\n{tail}");
        }

        input.sweep(&x11);
        input.apply_mouse(&x11, win, &geom);

        // window geometry can change (resize/fullscreen); refresh occasionally
        if Instant::now() >= next_geom_refresh {
            if let Ok(g) = x11.geometry(win) {
                geom = g;
            }
            next_geom_refresh = Instant::now() + Duration::from_secs(5);
        }

        let cap_start = Instant::now();
        let captured = x11.capture(win, geom.width as usize, geom.height as usize, &mut rgb);
        let cap_time = cap_start.elapsed();

        match captured {
            Ok(frame) => {
                stats.frames += 1;
                let render_start = Instant::now();
                let written = renderer.render(&frame, &mut out);
                let render_time = render_start.elapsed();
                let mut write_time = Duration::ZERO;
                if written > 0 {
                    stats.changed += 1;
                    let write_start = Instant::now();
                    // Wrap the frame in a synchronized update: a supporting
                    // terminal (Ghostty does) buffers everything inside and
                    // only then paints the frame atomically. Without it the
                    // rows of a long frame diff get painted straight from the
                    // parse buffer and every camera move looks like rolling
                    // blinds. Terminals without support ignore both unknown
                    // private modes, so no handshake is needed.
                    out.insert_str(0, "\x1b[?2026h");
                    out.push_str("\x1b[?2026l");
                    stdout.write_all(out.as_bytes())?;
                    stdout.flush()?;
                    write_time = write_start.elapsed();
                }
                if trace {
                    trace_acc[0] += cap_time;
                    trace_acc[1] += render_time;
                    trace_acc[2] += write_time;
                    trace_bytes += out.len();
                    trace_frames += 1;
                    let window = trace_t0.elapsed();
                    if window >= Duration::from_secs(1) {
                        let n = trace_frames.max(1) as f64;
                        let ms = |d: Duration| d.as_secs_f64() * 1000.0 / n;
                        trace_log.push_str(&format!(
                            "t={:6.1}s loop {:5.1} fps | capture {:5.2}ms render {:5.2}ms \
                             write {:5.2}ms | {} B/frame\n",
                            loop_start.elapsed().as_secs_f64(),
                            trace_frames as f64 / window.as_secs_f64(),
                            ms(trace_acc[0]),
                            ms(trace_acc[1]),
                            ms(trace_acc[2]),
                            trace_bytes / trace_frames.max(1),
                        ));
                        trace_acc = [Duration::ZERO; 3];
                        trace_bytes = 0;
                        trace_frames = 0;
                        trace_t0 = Instant::now();
                    }
                }
                // nothing moved: slow the capture down and give the CPU to java
                interval = if written > 0 {
                    target
                } else {
                    (interval * 2).min(IDLE_INTERVAL)
                };
            }
            Err(_) => {
                // window may be momentarily unmapped (resize/fullscreen)
                std::thread::sleep(Duration::from_millis(100));
                if let Ok(g) = x11.geometry(win) {
                    geom = g;
                }
            }
        }

        let elapsed = frame_start.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }

    stats.secs = loop_start.elapsed().as_secs_f64();
    if trace {
        let path = Config::config_dir().join("trace.log");
        let _ = std::fs::write(&path, trace_log);
    }
    input.release_all(&x11);
    drop(game); // GameProcess::drop kills Java
    Ok(stats)
}

// helpers ------------------------------------------------------------------

/// Full-screen "launching" splash: a centred title, the current stage with a
/// spinner and the elapsed time, plus a first-run hint.
fn launch_screen(version: &str, stage: &str, started: Instant, tick: usize) -> Result<()> {
    const SPIN: [char; 4] = ['|', '/', '-', '\\'];
    let (cols, rows) = terminal_cells();
    let centre = |text: &str| -> String {
        let width = text.chars().count();
        let pad = cols.saturating_sub(width) / 2;
        format!("\x1b[{}G{}", pad + 1, text)
    };

    let mut buf = String::from("\x1b[2J\x1b[H");
    let top = rows.saturating_sub(7) / 2;
    for _ in 0..top {
        buf.push_str("\r\n");
    }
    buf.push_str(&centre("\x1b[1;35mttycraft\x1b[0m"));
    buf.push_str("\r\n\r\n");
    buf.push_str(&centre(&format!(
        "\x1b[36m{}\x1b[0m  launching Minecraft \x1b[1m{version}\x1b[0m",
        SPIN[tick % SPIN.len()]
    )));
    buf.push_str("\r\n");
    buf.push_str(&centre(&format!("\x1b[2m{stage}\x1b[0m")));
    buf.push_str("\r\n\r\n");
    buf.push_str(&centre(&format!(
        "\x1b[2m{}s elapsed · Ctrl+Q aborts\x1b[0m",
        started.elapsed().as_secs()
    )));
    buf.push_str("\r\n\r\n");
    buf.push_str(&centre(
        "\x1b[2mthe first launch downloads files and can take a few minutes\x1b[0m",
    ));

    let mut out = io::stdout();
    out.write_all(buf.as_bytes())?;
    out.flush()?;
    Ok(())
}
