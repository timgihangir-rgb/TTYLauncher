//! Graceful shutdown on SIGHUP/SIGTERM/SIGINT.
//!
//! The session loops poll [`shutdown_requested`] every frame, break out and
//! run normal cleanup (kill Minecraft, stop Xvfb, restore the terminal)
//! instead of dying with the default signal handlers and leaking processes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

static FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();

pub fn init() {
    let flag = Arc::new(AtomicBool::new(false));
    let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, flag.clone());
    let _ = signal_hook::flag::register(signal_hook::consts::SIGHUP, flag.clone());
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, flag.clone());
    let _ = FLAG.set(flag);
}

pub fn shutdown_requested() -> bool {
    FLAG.get().map(|f| f.load(Ordering::SeqCst)).unwrap_or(false)
}
