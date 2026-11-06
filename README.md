# ttycraft

A terminal Minecraft launcher: pick a version, run the **real Java Minecraft**
on a headless Xvfb display, and watch its graphics stream into your terminal
as ANSI half-block characters. Input (keyboard + mouse) is injected back into
the game via XTEST, so you play Minecraft directly in a TTY.

```
 ┌────────────────────────────────────────────────┬──────────────────────┐
 │ ttycraft  user: Steve  mem: 1024 MB  fps: 60   │                      │
 ├─────────────────────────────┬──────────────────┤ Resolution  640x360  │
 │ /1.20                       │                  │ Graphics    fast     │
 │ ─────────────────────────── │                  │ Game dir    ~/.mine… │
 │ ▸ 1.20.1        release   ✓ │                  │                      │
 │   1.20          release     │                  │                      │
 │   1.19.4        release     │                  │                      │
 └─────────────────────────────┴──────────────────┘
  ↑↓ move · Enter play · i install · / filter · n name · m memory · p fps
  · c resolution · g graphics · r reload · q quit
```

(The real menu draws a bordered dashboard; the layout above is simplified.)

## How it works

```
 ┌─────────┐  MIT-SHM capture ┌───────────────    ANSI ▀ + truecolor    ┐
 │ Minecraft│ ────────────► │ ttycraft          ────────────────────► │ terminal
 │ (Java)   │  frames       │  capture → diff   half-block renderer  │
 │ on Xvfb  │ ◄──────────── │  ← XTEST keys/buttons/pointer warp     │
 └─────────┘   fake input   └─────────────── input ◄──────────────── │ keys/mouse
```

1. **Xvfb** — a virtual X server on the first free display (`:90`–`:109`),
   sized to the game window. No real GPU or monitor is needed.
2. **Launch** — `mc-launcher-core` installs/loads the version json, builds the
   Java command with a custom resolution, offline username, `-Xmx` cap and
   `ALSOFT_DRIVERS=null` (no audio device in a TTY). If the *fast* graphics
   profile is on, Minecraft's own `options.txt` is tuned for speed first
   (see [Performance](#performance)).
3. **Capture** — the frame is copied by the server itself through **MIT-SHM**
   (a System V segment attached at connect time): no multi-megabyte reply
   travels through the X socket, and the renderer samples the framebuffer
   words directly, without a conversion pass. Where MIT-SHM is unavailable the
   classic `XGetImage` + decode path is used unchanged.
4. **Render** — the frame is **box-averaged** down to the terminal grid and
   drawn with the `▀` half-block glyph, 24-bit color each. Box filtering (not
   nearest-neighbour) keeps text and edges readable when the game renders
   larger than the terminal. Each frame is sent inside a **synchronized
   update** (`CSI ? 2026`): supporting terminals buffer everything inside and
   paint it atomically, so camera movement doesn't redraw row by row straight
   from the parse stream. Only changed cells are rewritten (frame diff), and
   while nothing changes on screen the capture rate backs off, leaving the CPU
   to Java. The whole frame is flushed as a **single write**, and the byte
   stream is kept minimal: one cursor move per row, colours re-sent only when
   they change.
5. **Input** — crossterm events are translated back to the game:
   - keys → XTEST fake press/release using the server keymap, with
     shift/ctrl/alt refcounting. The terminal is asked for distinct
     press/repeat/release events (kitty keyboard protocol), so a game key is
     held down exactly as long as the key is held and lifted the moment it is
     released; terminals without that reporting still get an auto-release of
     held keys after a timeout. The Xvfb keyboard auto-repeat is switched off,
     because a held XTEST key would otherwise repeat as synthetic
     release/press pairs that games read as extra key presses;
   - mouse → terminal cell deltas accumulate and go out as XTEST **relative**
     motion, which is what turns the in-game camera. While the game has grabbed
     the pointer and hidden the cursor (gameplay) that is all that is sent; when
     the cursor is visible (menus, chat) the pointer is also warped to the
     terminal cell so clicks land where you point. Buttons via XTEST, wheel as
     button 4/5 taps;
   - **mouse recentring** — terminals only report the mouse as absolute cell
     coordinates (no terminal has a relative-motion protocol), so once the real
     pointer reaches the edge of the screen no further events arrive and the
     camera would stop turning with it. Instead ttycraft pushes the pointer
     back toward the middle of the terminal with a virtual mouse created
     through `/dev/uinput`. That is a real input device to the compositor, so it
     works on Wayland too, where an ordinary client may not warp the pointer.
     The camera then keeps turning no matter how far you push. Controlled by the
     `mouse_recenter` option (`u` in the menu); without uinput the behaviour is
     unchanged;
   - **Ctrl+Q** (or Ctrl+C) quits the session.

## Requirements

- Linux with X11 utilities: `Xvfb` (`apt install xvfb`)
- Java (the version Minecraft needs; old versions fall back to Java 8 if
  installed side-by-side)
- A **truecolor**, **fullscreen** terminal (`COLORTERM=truecolor`) — one cell
  = two game pixels, so a windowed terminal would waste most of the screen.
  The launcher waits on a "make it fullscreen" screen until the terminal fills
  the display (a maximized window counts); if the pixel size cannot be
  measured it proceeds with a warning.
- Internet for the version manifest and first-time installs
- Optional: write access to `/dev/uinput` (be in the `uinput` group, or a
  logind/udev ACL) for the endless edge mouse. Without it the launcher still
  runs, the camera just stops at the screen edge.

## Usage

```sh
cargo run --                # interactive menu: versions, settings, install
cargo run -- play 1.20.1    # install if needed, then launch
cargo run -- install 1.8.9  # install only (with progress)
cargo run -- list           # remote + local versions
```

In the menu: `↑↓/j/k` move, `Enter` play/install, `/` filter, `n` username,
`m` memory (MB), `p` FPS limit, `c` cycle resolution, `g` toggle the fast
graphics profile, `u` toggle endless edge mouse, `r` reload manifest, `q` quit.
The mouse works too: click a version to select it, click it again (or press
`Enter`) to play or install it, and the wheel scrolls the list.

In game: mouse aims, WASD moves, `Ctrl+Q` quits. The session log lands in
`~/.ttycraft/session.log`; when you come back the menu status line reports the
measured frame rate.

Config: `~/.ttycraft/config.json`

| key            | default        | meaning                                    |
|----------------|----------------|--------------------------------------------|
| username       | `Steve`        | offline account name                       |
| xmx_mb         | `1024`         | JVM max heap — keep low on weak machines   |
| fps            | `60`           | capture/render frame-rate ceiling          |
| resolution     | `640x360`      | game window size: `auto` or `WxH` pixels   |
| low_end        | `true`         | tune Minecraft for speed before launching  |
| mouse_recenter | `true`         | endless camera turn at the screen edge     |
| game_dir       | `~/.minecraft` | shared with other launchers                |

`resolution` accepts `auto` (one game pixel per terminal half-pixel, no
resampling — the most detail the terminal can actually show) or any `WxH`
size; the menu's `c` key cycles a ladder from `320x180` up to `1920x1080`.

Defaults are deliberately frugal: a small game window, software GL
(`LIBGL_ALWAYS_SOFTWARE=1`), 60 FPS ceiling, 1 GB heap — suitable for a weak
machine with no graphics acceleration.

## Performance

The game runs on **software OpenGL** (llvmpipe) — there is no GPU on a headless
Xvfb display — and ttycraft runs on the *same* CPU cores as the JVM, so the two
compete. Two things matter, in this order:

### 1. Build the launcher optimized (biggest win)

An **unoptimized** build is CPU-bound: its per-frame work (decoding the captured
image and rebuilding the ANSI frame) is slow enough to both cap the display and
steal a core from the game. Measured on the same world, same settings (640x360):

| per frame            | `cargo build` (unoptimized) | optimized |
|----------------------|-----------------------------|-----------|
| render (ANSI)        | ~7 ms                       | ~2 ms     |
| terminal write       | ~1 ms                       | ~1 ms     |
| **Minecraft fps**    | **8–9**                     | **20–21** |

Capture is no longer a factor: the optimized build reads frames through
**MIT-SHM** — the server copies the frame straight into a shared segment and
the renderer samples its words directly, so the whole capture costs ~0.5 ms at
1280x720 on an idle server (`capture_bench` measures ~9x cheaper than the old
`XGetImage`-through-the-socket path, and skips the decode pass entirely).

The dev profile is therefore optimized in `Cargo.toml` (`[profile.dev]
opt-level = 2`, debug symbols kept), so `cargo run` is fast out of the box. The
first build after this setting takes a few minutes (dependencies rebuild);
later builds are incremental. `cargo run --release` is equivalent.

### 2. Resolution

Rendering is **fill-rate bound**, so fewer pixels means more frames, almost
linearly (measured with the optimized build):

| resolution | pixels  | Minecraft fps |
|------------|---------|---------------|
| 320x180    | 58 k    | ~26           |
| 640x360    | 230 k   | ~20           |

`capture_bench` compares the two capture paths side by side on a running Xvfb:
the classic `XGetImage` reply costs ~9 ms at 640x360 (scaling linearly: ~35 ms
at 1280x720, ~80 ms at 1080p — plus a decode pass), while the **MIT-SHM** read
the launcher uses copies the same bytes for ~1/9th of that with zero
mismatches. Capture is comfortably outside the frame budget at every size. Rendering itself is near the byte
floor for truecolor: one `▀` (3 bytes) plus the colour digits per changed cell,
with a single cursor move per row and colours only re-sent when they change
(~65–80 KB per frame at a 100x30 terminal).

Set `TTYCRAFT_TRACE=1` to write a per-second breakdown of capture / render /
write and bytes-per-frame to `~/.ttycraft/trace.log`.

### Fast graphics profile

`low_end` (on by default) rewrites Minecraft's own `options.txt` before launch:
render distance 12 → 4, simulation distance 12 → 4, vsync off, max fps 260,
fast graphics, no clouds/entity shadows/ao, mipmaps off, particles minimal.
It also launches Java with a pause-tuned G1 (`MaxGCPauseMillis=50`, experimental
young-gen sizing, no explicit-GC stops — fewer visible hitches) and sets
`MESA_NO_ERROR=1` so the software rasterizer skips per-call GL error checks.
The original file is copied once to `options.txt.ttycraft-bak` next to it, and the
file is only ever edited in place (values keep their quoting), so any Minecraft
version keeps parsing it. Turn it off with `g` to use your own settings.

Want more frames? Press `c` in the menu and step down (e.g. `480x270` or
`320x180`) — the difference is large and matches the pixel counts above.

## Examples

Step-by-step proofs that each piece of the pipeline works, from the simplest
terminal trick to the full virtual-display loop:

| example          | what it demonstrates                                                  |
|------------------|-----------------------------------------------------------------------|
| `halfblock`      | RGB frame → `▀` diff rendering + resize handling in a truecolor term  |
| `term_events`    | crossterm keyboard/mouse/resize events, raw mode, mouse capture       |
| `manifest`       | piston-meta `version_manifest_v2`: 919 versions, java requirements    |
| `install_probe`  | mc-launcher-core end-to-end: install with progress, build the command |
| `launch_probe`   | the exact java command for a version, without installing or running  |
| `x11_capture`    | XGetImage → RGB decode (depth/bpp/byte order) of a live X window      |
| `xtest_input`    | XTEST fake keys: keycode↔keysym server keymap, modifier injection     |
| `xvfb_demo`      | the whole loop without Minecraft: Xvfb → window → capture → terminal, |
|                  | plus XTEST self-tests (keys DELIVERED, warp OK, relative motion OK)   |
| `xi_probe`       | XInput2 raw motion (`deviceid=2`): the reference for mouse look        |
| `capture_bench`  | classic `XGetImage` vs the MIT-SHM read the launcher uses             |
| `capture_probe`  | XGetImage cost for root vs the Minecraft window on a *running* display |
| `gamedir_probe`  | where Minecraft keeps `options.txt` (the `--gameDir` the launcher sets)|

Run any of them with, for example:

```sh
cargo run --example xvfb_demo
cargo run --example install_probe -- 1.20.1
```

## Design notes & limitations

- Frame capture uses MIT-SHM when the server offers it (Xvfb does), falling
  back to a plain `XGetImage` reply otherwise; no compositor needed either
  way, which is why Xvfb (not the host Wayland/X session) hosts the game:
  capturing the root window of Xwayland fails with `BadMatch`.
- Sound is disabled (`ALSOFT_DRIVERS=null`); a TTY has no audio device anyway.
- Offline auth only: modern versions show a harmless 401 in the log and fall
  back to the local profile.
- The pointer is clamped to the game window; when Minecraft runs windowed its
  own cursor logic keeps re-centering, which bounds the clamping.
- If the launcher is killed with `SIGKILL`, Xvfb/Java may be left behind;
  `SIGHUP`/`SIGTERM`/`SIGINT` are handled and clean everything up.

## Prior art

- **ansicraft** — the half-block (`▀` fg/bg) truecolor rendering trick and
  terminal mouse capture for game input.
- **mineclift** — running the real client headlessly and bridging X11 input.
- The XTEST self-tests in `xvfb_demo` were used to prove, on the target
  machine, that fake keys (`deviceid=2`), absolute warps and relative motion
  all work before wiring them into the launcher.
