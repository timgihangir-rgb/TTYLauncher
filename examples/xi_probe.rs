//! Probe: which injection method does an XI2 *raw* consumer see?
//!
//! Minecraft's GLFW uses XI2 raw events for the camera when the cursor is
//! grabbed (in-game). Pointer warps are invisible to that path, XTEST fake
//! input is not. Also checks raw buttons/keys, which games may read too.
//!
//! Run: cargo run --example xi_probe

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xinput;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

const XI_ALL_MASTER_DEVICES: u16 = 1;
const DEVICE_ID: u8 = 2; // XTEST device, from earlier experiments

fn spawn_xvfb() -> (Child, u16) {
    let display = (90..110u16)
        .find(|n| !std::path::Path::new(&format!("/tmp/.X11-unix/X{n}")).exists())
        .expect("no free display");
    let child = Command::new("Xvfb")
        .arg(format!(":{display}"))
        .args(["-screen", "0", "640x480x24", "-nolisten", "tcp"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Xvfb");
    for _ in 0..100 {
        if std::path::Path::new(&format!("/tmp/.X11-unix/X{display}")).exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    (child, display)
}

fn drain(conn: &RustConnection, ms: u64) -> Vec<Event> {
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        match conn.poll_for_event() {
            Ok(Some(ev)) => out.push(ev),
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(_) => break,
        }
    }
    out
}

fn count(events: &[Event], needle: &str) -> usize {
    events
        .iter()
        .filter(|ev| {
            let name = match ev {
                Event::XinputRawMotion(_) => "raw_motion",
                Event::XinputRawButtonPress(_) => "raw_button_press",
                Event::XinputRawButtonRelease(_) => "raw_button_release",
                Event::XinputRawKeyPress(_) => "raw_key_press",
                Event::XinputRawKeyRelease(_) => "raw_key_release",
                Event::MotionNotify(_) => "core_motion",
                Event::ButtonPress(_) => "core_button_press",
                Event::KeyPress(_) => "core_key_press",
                _ => "other",
            };
            name == needle
        })
        .count()
}

fn select_mask(conn: &RustConnection, root: u32, masks: Vec<xinput::XIEventMask>) -> String {
    let sel = [xinput::EventMask { deviceid: XI_ALL_MASTER_DEVICES, mask: masks }];
    let _ = conn.flush();
    match xinput::xi_select_events(conn, root, &sel).unwrap().check() {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("{e:?}"),
    }
}

fn fake(conn: &RustConnection, type_: u8, detail: u8, root: u32, x: i16, y: i16) {
    if let Ok(cookie) = conn.xtest_fake_input(type_, detail, 0, root, x, y, DEVICE_ID) {
        let _ = cookie.check();
    }
    let _ = conn.flush();
}

fn main() {
    let (mut xvfb, display) = spawn_xvfb();
    let (conn, screen_num) = x11rb::connect(Some(&format!(":{display}"))).expect("connect");
    let root = conn.setup().roots[screen_num].root;
    println!("Xvfb :{display} up, root=0x{root:x}");
    let ver = xinput::xi_query_version(&conn, 2, 0).unwrap().reply().unwrap();
    println!("XI version {}.{}\n", ver.major_version, ver.minor_version);

    // --- camera: relative motion vs warp, seen as raw motion
    let sel = select_mask(&conn, root, vec![xinput::XIEventMask::RAW_MOTION]);
    println!("select [RAW_MOTION]            -> {sel}");
    drain(&conn, 50);
    fake(&conn, 6 /* MotionNotify */, 1 /* relative */, root, 5, 3);
    let ev = drain(&conn, 120);
    println!(
        "  XTEST move_relative(5,3)     -> raw_motion={} core_motion={}",
        count(&ev, "raw_motion"),
        count(&ev, "core_motion")
    );
    drain(&conn, 30);
    conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, 200, 150).unwrap().check().unwrap();
    conn.flush().unwrap();
    let ev = drain(&conn, 120);
    println!(
        "  warp_pointer(200,150)        -> raw_motion={} core_motion={}",
        count(&ev, "raw_motion"),
        count(&ev, "core_motion")
    );

    // --- buttons, raw
    for (name, mask) in [
        ("RAW_BUTTON_PRESS", xinput::XIEventMask::RAW_BUTTON_PRESS),
        ("RAW_BUTTON_RELEASE", xinput::XIEventMask::RAW_BUTTON_RELEASE),
    ] {
        let sel = select_mask(&conn, root, vec![mask]);
        drain(&conn, 40);
        fake(&conn, 4, 1, root, 0, 0);
        fake(&conn, 5, 1, root, 0, 0);
        let ev = drain(&conn, 120);
        println!(
            "select [{name}] -> {sel}; XTEST click -> raw_button_press={} raw_button_release={}",
            count(&ev, "raw_button_press"),
            count(&ev, "raw_button_release")
        );
    }

    // --- keys, raw
    let sel = select_mask(&conn, root, vec![xinput::XIEventMask::RAW_KEY_PRESS]);
    drain(&conn, 40);
    fake(&conn, 2 /* KeyPress */, 38 /* a */, root, 0, 0);
    fake(&conn, 3 /* KeyRelease */, 38, root, 0, 0);
    let ev = drain(&conn, 120);
    println!(
        "select [RAW_KEY_PRESS] -> {sel}; XTEST key 'a' -> raw_key_press={} core_key_press={}",
        count(&ev, "raw_key_press"),
        count(&ev, "core_key_press")
    );

    // --- can we combine several raw masks at once?
    let sel = select_mask(
        &conn,
        root,
        vec![
            xinput::XIEventMask::RAW_MOTION,
            xinput::XIEventMask::RAW_BUTTON_PRESS,
            xinput::XIEventMask::RAW_BUTTON_RELEASE,
            xinput::XIEventMask::RAW_KEY_PRESS,
            xinput::XIEventMask::RAW_KEY_RELEASE,
        ],
    );
    println!("\nselect [all raw masks together] -> {sel}");

    conn.flush().ok();
    drop(conn);
    let _ = xvfb.kill();
    let _ = xvfb.wait();
}
