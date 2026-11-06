//! Xvfb virtual display management: spawn on a free display, wait for the
//! socket, kill on drop.

use anyhow::{bail, Result};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct Xvfb {
    child: Child,
    display: u8,
}

fn socket_path(display: u8) -> PathBuf {
    PathBuf::from(format!("/tmp/.X11-unix/X{display}"))
}

fn display_busy(display: u8) -> bool {
    socket_path(display).exists()
}

impl Xvfb {
    /// Spawn Xvfb on the first free display in 90..110.
    pub fn spawn(screen_w: u16, screen_h: u16) -> Result<Self> {
        let mut last_err = None;
        for display in 90u8..110 {
            if display_busy(display) {
                continue;
            }
            let spawned = Command::new("Xvfb")
                .arg(format!(":{display}"))
                .args(["-screen", "0", &format!("{screen_w}x{screen_h}x24")])
                .args(["-nolisten", "tcp"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            match spawned {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    bail!("Xvfb not found in PATH — install it (e.g. `apt install xvfb`)")
                }
                Err(e) => last_err = Some(e.to_string()),
                Ok(mut child) => {
                    match Self::wait_socket(display, Duration::from_secs(5)) {
                        Ok(()) => return Ok(Xvfb { child, display }),
                        Err(e) => {
                            let _ = child.kill();
                            let _ = child.wait();
                            last_err = Some(e.to_string());
                        }
                    }
                }
            }
        }
        bail!(
            "failed to start Xvfb on displays 90..110: {}",
            last_err.unwrap_or_else(|| "no free display".into())
        )
    }

    fn wait_socket(display: u8, timeout: Duration) -> Result<()> {
        let path = socket_path(display);
        let start = Instant::now();
        while start.elapsed() < timeout {
            if path.exists() {
                // Give the server a moment to start accepting connections.
                std::thread::sleep(Duration::from_millis(100));
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        bail!("Xvfb :{display} socket did not appear")
    }

    pub fn display_string(&self) -> String {
        format!(":{}", self.display)
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Xvfb removes its own socket, but be safe.
        let _ = std::fs::remove_file(socket_path(self.display));
    }
}
