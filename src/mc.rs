//! Minecraft side: version manifest, install with progress, process spawn.

use anyhow::{bail, Context, Result};
use mc_launcher_core::account::Account;
use mc_launcher_core::command::builder::LaunchOptions;
use mc_launcher_core::install::request::InstallRequest;
use mc_launcher_core::launcher::Launcher;
use mc_launcher_core::progress::ProgressEvent;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

#[derive(Debug, Serialize, Deserialize)]
pub struct Latest {
    pub release: String,
    pub snapshot: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub latest: Latest,
    pub versions: Vec<VersionEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct VersionEntry {
    pub id: String,
    pub r#type: String,
    pub url: String,
    #[serde(default)]
    pub time: String,
    #[serde(default)]
    pub release_time: String,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub compliance_level: Option<u32>,
}

const MANIFEST_URL: &str =
    "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

pub fn fetch_manifest() -> Result<Manifest> {
    let mut resp = ureq::get(MANIFEST_URL)
        .call()
        .context("failed to fetch version manifest")?;
    let manifest: Manifest = resp.body_mut().read_json()?;
    Ok(manifest)
}

/// Local version directories inside `<game_dir>/versions`.
pub fn local_versions(game_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(game_dir.join("versions")) {
        for e in rd.flatten() {
            if e.path().is_dir() {
                let name = e.file_name().to_string_lossy().into_owned();
                if std::fs::exists(e.path().join(format!("{name}.json"))).unwrap_or(false) {
                    out.push(name);
                }
            }
        }
    }
    out.sort();
    out
}

pub fn is_local(game_dir: &Path, version: &str) -> bool {
    std::fs::exists(
        game_dir
            .join("versions")
            .join(version)
            .join(format!("{version}.json")),
    )
    .unwrap_or(false)
}

/// Install a version, forwarding progress lines to `report`.
pub fn install<F>(game_dir: &Path, version: &str, mut report: F) -> Result<()>
where
    F: FnMut(&str),
{
    let launcher = Launcher::new(game_dir);
    let req = InstallRequest::vanilla(version);
    let result = launcher.install_with_progress(req, &mut |event: ProgressEvent| {
        let line = match event {
            ProgressEvent::StageStarted { stage } => format!(">> {stage:?}"),
            ProgressEvent::TaskStarted { label, .. } => format!("  .. {label}"),
            ProgressEvent::TaskSkipped { label, reason } => {
                format!("   = {label} ({reason:?})")
            }
            ProgressEvent::TaskFinished { label } => format!("   ok {label}"),
            ProgressEvent::BytesReceived { received, total, .. } => match total {
                Some(t) if t > 0 => format!("   {}% ({received}/{t})", received * 100 / t),
                _ => return,
            },
        };
        report(&line);
    });
    result.map_err(|e| anyhow::anyhow!("install failed: {e}"))?;
    Ok(())
}

/// Which Java major version does this MC version require (best effort from
/// the version json; 8 for everything old, 21+ for recent releases).
pub fn required_java(game_dir: &Path, version: &str) -> Option<u32> {
    let path = game_dir
        .join("versions")
        .join(version)
        .join(format!("{version}.json"));
    let text = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let java = json.get("javaVersion")?.get("majorVersion")?.as_u64()?;
    Some(java as u32)
}

/// The java executable to use. The system `java` is assumed to be compatible
/// unless the version explicitly needs Java 8 (old MC + new java breaks).
pub fn java_command(game_dir: &Path, version: &str) -> String {
    if let Some(need) = required_java(game_dir, version)
        && need <= 8 {
            for cand in ["java-8-openjdk", "java-8-openjdk-amd64", "java18", "java-1.8"] {
                let p = format!("/usr/lib/jvm/{cand}/bin/java");
                if std::fs::exists(&p).unwrap_or(false) {
                    return p;
                }
            }
        }
    "java".to_string()
}

pub struct LaunchArgs {
    pub version: String,
    pub username: String,
    pub xmx_mb: u32,
    pub width: u16,
    pub height: u16,
    pub session_log: PathBuf,
    /// Tune Minecraft's own options for speed before launching.
    pub low_end: bool,
}

/// JVM flags for low-end mode, tuned for a weak CPU: a G1 with a short pause
/// budget (frequent-but-short GCs instead of rare long hitches), steadier
/// young-gen sizing, and no explicit `System.gc()` stops. Order matters:
/// the unlock flag must come before the experimental options.
const LOW_END_JVM_FLAGS: &[&str] = &[
    "-XX:+UnlockExperimentalVMOptions",
    "-XX:+UseG1GC",
    "-XX:MaxGCPauseMillis=50",
    "-XX:G1NewSizePercent=20",
    "-XX:G1ReservePercent=20",
    "-XX:+DisableExplicitGC",
];

/// Minecraft options that matter most on weak hardware. Values are merged into
/// the game's `options.txt` (only keys the file already has are rewritten, so
/// every MC version keeps parsing it as it did before).
const PERF_SETTINGS: &[(&str, &str)] = &[
    ("enableVsync", "false"),      // Xvfb has no real vsync — just caps fps
    ("maxFps", "260"),             // 260 = unlimited in MC's slider
    ("renderDistance", "4"),
    ("simulationDistance", "5"),   // 1.20.1 rejects anything below 5
    ("graphicsMode", "0"),         // 1.20.2+: 0 = fast
    ("fancyGraphics", "false"),    // pre-1.20.2 equivalent
    ("ao", "false"),
    ("particles", "2"),            // 2 = minimal
    ("renderClouds", "false"),
    ("mipmapLevels", "0"),
    ("entityShadows", "false"),
    ("biomeBlendRadius", "0"),
    ("entityDistanceScaling", "0.5"),
    ("fullscreen", "false"),       // the window must stay capturable
];

/// Keys appended when the game has never written an `options.txt` yet: plain
/// numbers and booleans, which all versions parse identically.
const PERF_APPEND: &[(&str, &str)] = &[
    ("enableVsync", "false"),
    ("maxFps", "260"),
    ("renderDistance", "4"),
    ("simulationDistance", "5"),
    ("particles", "2"),
    ("mipmapLevels", "0"),
    ("fancyGraphics", "false"),
    ("graphicsMode", "0"),
];

/// Merge [`PERF_SETTINGS`] into `<game_dir>/options.txt`.
///
/// The first rewrite keeps a copy at `options.txt.ttycraft-bak`, so vanilla
/// settings can always be restored.
pub fn apply_low_end_options(game_dir: &Path) -> Result<()> {
    let path = game_dir.join("options.txt");
    let original = std::fs::read_to_string(&path).unwrap_or_default();

    if !original.is_empty() {
        let backup = game_dir.join("options.txt.ttycraft-bak");
        if !backup.exists() {
            std::fs::write(&backup, &original)
                .with_context(|| format!("write backup {}", backup.display()))?;
        }
    }

    let mut out = String::with_capacity(original.len() + 256);
    let mut seen: Vec<&str> = Vec::new();
    for line in original.lines() {
        let tuned = line.split_once(':').and_then(|(key, value)| {
            PERF_SETTINGS
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (key, value, *v))
        });
        match tuned {
            Some((key, value, new)) => {
                seen.push(key);
                out.push_str(key);
                out.push(':');
                // some versions quote enum values ("true"), others do not
                if value.starts_with('"') {
                    out.push('"');
                    out.push_str(new);
                    out.push('"');
                } else {
                    out.push_str(new);
                }
                out.push('\n');
            }
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    for (key, value) in PERF_APPEND {
        if !seen.contains(key) {
            out.push_str(&format!("{key}:{value}\n"));
        }
    }

    std::fs::write(&path, out).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Build the launch command for an installed version.
pub fn build_command(game_dir: &Path, args: &LaunchArgs) -> Result<Command> {
    let launcher = Launcher::new(game_dir);
    let version = launcher
        .load_version(&args.version)
        .map_err(|e| anyhow::anyhow!("load version json: {e}"))?;

    let java = java_command(game_dir, &args.version);
    let options = LaunchOptions {
        account: Account::offline(args.username.clone()),
        java_executable: Some(PathBuf::from(&java)),
        custom_resolution: Some((args.width as u32, args.height as u32)),
        ..Default::default()
    };
    let built = launcher
        .build_launch_command_from_version(&version, options)
        .map_err(|e| anyhow::anyhow!("build launch command: {e}"))?;

    if args.low_end {
        let dir = data_dir(&built.working_dir, &built.args);
        apply_low_end_options(&dir).context("tune Minecraft graphics settings")?;
    }

    // LaunchOptions has no memory flag; prepend -Xmx before everything else.
    let mut cmd = Command::new(&built.executable);
    cmd.arg(format!("-Xmx{}M", args.xmx_mb));
    // JVM flags must precede `-cp`, so this goes before the launcher's args.
    if args.low_end {
        for flag in LOW_END_JVM_FLAGS {
            cmd.arg(flag);
        }
    }
    cmd.args(&built.args);
    cmd.current_dir(&built.working_dir);
    for (k, v) in &built.env {
        cmd.env(k, v);
    }
    if args.low_end {
        // the software rasterizer spends a slice of every frame servicing
        // per-call GL error checks; Minecraft handles GL errors itself, so
        // skip the driver's bookkeeping
        cmd.env("MESA_NO_ERROR", "1");
    }
    Ok(cmd)
}

/// The directory Minecraft actually stores its data in: the `--gameDir`
/// argument the launcher generated, or the launcher's working directory.
fn data_dir(working_dir: &Path, args: &[String]) -> PathBuf {
    let dir = args
        .windows(2)
        .find(|w| w[0] == "--gameDir")
        .map(|w| PathBuf::from(&w[1]))
        .unwrap_or_else(|| working_dir.to_path_buf());
    if dir.is_relative() {
        working_dir.join(dir)
    } else {
        dir
    }
}

/// Spawn MC with terminal-friendly environment.
pub fn spawn(game_dir: &Path, args: &LaunchArgs, display: &str) -> Result<Child> {
    let mut cmd = build_command(game_dir, args)?;
    let log = std::fs::File::create(&args.session_log)
        .with_context(|| format!("create log {}", args.session_log.display()))?;
    let log_err = log.try_clone()?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .env("DISPLAY", display)
        .env("ALSOFT_DRIVERS", "null") // no audio device in the TTY session
        .env("LIBGL_ALWAYS_SOFTWARE", "1")
        .current_dir(game_dir);
    let mut child = cmd
        .spawn()
        .context("failed to spawn Minecraft (java not found?)")?;
    // Give java a moment to fail fast (bad version, missing files).
    std::thread::sleep(std::time::Duration::from_millis(400));
    if let Some(status) = child.try_wait()? {
        let tail = tail_of_log(&args.session_log, 12);
        bail!("Minecraft exited immediately ({status}):\n{tail}");
    }
    Ok(child)
}

pub fn tail_of_log(path: &Path, lines: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return "  (no log)".into();
    };
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_end_options_merge_in_place() {
        let dir = std::env::temp_dir().join(format!("ttycraft-mc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("options.txt");
        std::fs::write(
            &path,
            "version:3465\nrenderDistance:12\nrenderClouds:\"true\"\nfov:0.0\n\
             key_key.attack:key.mouse.left\n",
        )
        .unwrap();

        apply_low_end_options(&dir).unwrap();

        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains("renderDistance:4")); // rewritten in place
        assert!(out.contains("renderClouds:\"false\"")); // quoting preserved
        assert!(out.contains("fov:0.0")); // untouched
        assert!(out.contains("key_key.attack:key.mouse.left"));
        assert!(out.contains("maxFps:260")); // appended when missing
        let backup = std::fs::read_to_string(dir.join("options.txt.ttycraft-bak")).unwrap();
        assert!(backup.contains("renderDistance:12"));

        // a second launch must be idempotent and must not clobber the backup
        apply_low_end_options(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), out);
        assert_eq!(out.matches("maxFps:").count(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn data_dir_prefers_the_game_dir_argument() {
        let args = vec![
            "-cp".to_string(),
            "a.jar".to_string(),
            "--gameDir".to_string(),
            "/games/mc".to_string(),
        ];
        assert_eq!(data_dir(Path::new("/fallback"), &args), PathBuf::from("/games/mc"));
        assert_eq!(
            data_dir(Path::new("/fallback"), &["-cp".to_string()]),
            PathBuf::from("/fallback")
        );
    }
}
