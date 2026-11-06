//! Persistent launcher configuration.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Game render sizes cycled by the menu (`resolution` is `"auto"` or `"WxH"`).
pub const RESOLUTION_CYCLE: [&str; 8] = [
    "auto",
    "320x180",
    "480x270",
    "640x360",
    "854x480",
    "1280x720",
    "1600x900",
    "1920x1080",
];

// Below 320x180 Minecraft's GUI swallows the whole screen, and software GL
// is hopeless well before 4K, so every size is clamped to this range.
const MIN_W: u16 = 320;
const MIN_H: u16 = 180;
const MAX_W: u16 = 2560;
const MAX_H: u16 = 1440;

/// Parse `"640x360"` into `(640, 360)`; anything else (`"auto"`) is `None`.
fn parse_wh(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once(['x', 'X'])?;
    let w = w.trim().parse().ok()?;
    let h = h.trim().parse().ok()?;
    (w > 0 && h > 0).then_some((w, h))
}

/// The next preset after `current` (unknown values fall back to the first).
fn next_resolution(current: &str) -> &'static str {
    let pos = RESOLUTION_CYCLE
        .iter()
        .position(|v| *v == current)
        .map(|i| i + 1)
        .unwrap_or(0);
    RESOLUTION_CYCLE[pos % RESOLUTION_CYCLE.len()]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Offline username passed to Minecraft.
    pub username: String,
    /// JVM max heap in megabytes — keep low on weak machines.
    pub xmx_mb: u32,
    /// Terminal capture/render frame rate.
    pub fps: u32,
    /// Game render size: `"auto"` (follow the terminal) or `"WxH"` pixels.
    pub resolution: String,
    /// Tune Minecraft for speed on launch (short render distance, no vsync,
    /// fast graphics …). Keeps a one-time `options.txt.ttycraft-bak`.
    pub low_end: bool,
    /// Keep the camera turning at the screen edge by recentring the mouse with
    /// a virtual uinput pointer (see `uinput`). No effect where uinput is
    /// unavailable.
    pub mouse_recenter: bool,
    /// Minecraft game directory (shared with other launchers by default).
    pub game_dir: PathBuf,
    /// Last played version, preselected in the menu.
    pub last_version: Option<String>,
    /// Version manifest cache path override (defaults next to config).
    pub manifest_cache: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        Self {
            username: "Steve".to_string(),
            xmx_mb: 1024,
            fps: 60,
            resolution: "640x360".to_string(),
            low_end: true,
            mouse_recenter: true,
            game_dir: home.join(".minecraft"),
            last_version: None,
            manifest_cache: None,
        }
    }
}

impl Config {
    pub fn config_dir() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(".ttycraft")
    }

    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.json")
    }

    pub fn manifest_cache_path(&self) -> PathBuf {
        self.manifest_cache
            .clone()
            .unwrap_or_else(|| Self::config_dir().join("manifest.json"))
    }

    pub fn session_log_path() -> PathBuf {
        Self::config_dir().join("session.log")
    }

    /// Game window size in pixels for a terminal of `cols` x `rows` cells.
    ///
    /// `auto` maps one game pixel onto one terminal half-pixel (1:1, no
    /// resampling); an explicit `"WxH"` is used as given, so anything larger
    /// than the terminal gets downsampled by the renderer.
    pub fn game_size(&self, cols: usize, rows: usize) -> (u16, u16) {
        let (w, h) = parse_wh(&self.resolution).unwrap_or((cols as u32, rows as u32 * 2));
        ((w as u16).clamp(MIN_W, MAX_W), (h as u16).clamp(MIN_H, MAX_H))
    }

    /// Step to the next entry of the resolution cycle.
    pub fn cycle_resolution(&mut self) {
        self.resolution = next_resolution(&self.resolution).to_string();
        self.save();
    }

    pub fn load() -> Self {
        match std::fs::read_to_string(Self::config_path()) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let _ = std::fs::create_dir_all(Self::config_dir());
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::config_path(), text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_config_gains_new_defaults() {
        // written before `resolution`/`low_end` existed; `scale` is gone
        let legacy = r#"{
            "username": "Steve", "xmx_mb": 1024, "fps": 20, "scale": 1,
            "game_dir": "/tmp", "last_version": null, "manifest_cache": null
        }"#;
        let cfg: Config = serde_json::from_str(legacy).unwrap();
        assert_eq!(cfg.resolution, "640x360");
        assert!(cfg.low_end);
        assert!(cfg.mouse_recenter);
        assert_eq!(cfg.fps, 20);
        assert_eq!(cfg.username, "Steve");
    }

    #[test]
    fn game_size_uses_preset_or_terminal() {
        let sized = |r: &str| Config {
            resolution: r.into(),
            ..Config::default()
        };
        // an explicit size is used as given
        assert_eq!(sized("1280x720").game_size(213, 60), (1280, 720));
        // `auto` maps one game pixel onto one terminal half-pixel
        assert_eq!(sized("auto").game_size(400, 300), (400, 600));
        // tiny terminals still get a usable window
        assert_eq!(sized("auto").game_size(80, 24), (320, 180));
        // oversized requests are clamped to the hard ceiling
        assert_eq!(sized("9999x9999").game_size(80, 24), (2560, 1440));
    }

    #[test]
    fn resolution_cycle_covers_every_preset() {
        let mut cur = RESOLUTION_CYCLE[0];
        for _ in 0..RESOLUTION_CYCLE.len() {
            cur = next_resolution(cur);
        }
        // a full cycle returns to where it started
        assert_eq!(cur, RESOLUTION_CYCLE[0]);
        // an unknown value restarts the cycle
        assert_eq!(next_resolution("1000x500"), RESOLUTION_CYCLE[0]);
        assert_eq!(next_resolution("640x360"), "854x480");
    }
}
