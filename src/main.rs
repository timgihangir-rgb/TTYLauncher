//! ttycraft — a terminal Minecraft launcher.
//!
//! Runs the real Java Minecraft on a headless Xvfb display, captures its
//! frames and draws them in the terminal as ANSI half-blocks; input goes
//! back to the game via XTEST.
//!
//! Usage:
//!   ttycraft                 interactive menu (version list, settings)
//!   ttycraft play <version>  install (if needed) and play directly
//!   ttycraft install <ver>   install a version with progress
//!   ttycraft list            list remote + local versions
//!   ttycraft --help

mod config;
mod display;
mod input;
mod menu;
mod mc;
mod render;
mod session;
mod signals;
mod uinput;
mod x11;
mod xvfb;

use std::io::Write;

use anyhow::{bail, Result};

use config::Config;

fn usage() -> ! {
    println!(
        "ttycraft — terminal Minecraft launcher\n\
         \n\
         Usage:\n\
         \x20 ttycraft                 interactive menu\n\
         \x20 ttycraft play <version>  launch a version (installs it if missing)\n\
         \x20 ttycraft install <ver>   install a version\n\
         \x20 ttycraft list             list versions (remote + local)\n\
         \n\
         Config: {} \n\
         In game: Ctrl+Q quits, mouse aims, WASD moves.",
        Config::config_path().display()
    );
    std::process::exit(0)
}

fn main() {
    if let Err(e) = real_main() {
        // make sure the terminal is usable before printing
        let _ = crossterm::terminal::disable_raw_mode();
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = Config::load();
    signals::init();

    match args.first().map(String::as_str) {
        None => menu::run(config),
        Some("--help") | Some("-h") | Some("help") => usage(),
        Some("list") => list(&config),
        Some("install") => {
            let Some(ver) = args.get(1) else {
                bail!("usage: ttycraft install <version>")
            };
            mc::install(&config.game_dir, ver, |line| {
                println!("{line}");
                let _ = std::io::stdout().flush();
            })?;
            println!("installed {ver}");
            Ok(())
        }
        Some("play") => {
            let Some(ver) = args.get(1) else {
                bail!("usage: ttycraft play <version>")
            };
            play(config, ver)
        }
        Some(other) if !other.starts_with('-') => play(config, other),
        Some(other) => bail!("unknown command `{other}` — try --help"),
    }
}

fn play(mut config: Config, version: &str) -> Result<()> {
    display::require_fullscreen()?;

    // install on demand
    if !mc::is_local(&config.game_dir, version) {
        println!("installing {version} …");
        mc::install(&config.game_dir, version, |line| {
            println!("{line}");
            let _ = std::io::stdout().flush();
        })?;
    }

    let (cols, rows) = session::terminal_cells();
    let (game_w, game_h) = config.game_size(cols, rows);
    let params = session::SessionParams {
        version: version.to_string(),
        game_w,
        game_h,
        fps: config.fps.clamp(1, 120),
        xmx_mb: config.xmx_mb,
        username: config.username.clone(),
    };
    config.last_version = Some(version.to_string());
    config.save();

    let res = session::run(&config, &params);
    match &res {
        Ok(s) => println!(
            "session ended — {:.0} fps shown ({:.0} fps from the game) — thanks for playing!",
            s.fps(),
            s.game_fps()
        ),
        Err(e) => eprintln!("session failed: {e:#}"),
    }
    res.map(|_| ())
}

fn list(config: &Config) -> Result<()> {
    let local = mc::local_versions(&config.game_dir);
    let remote = match mc::fetch_manifest() {
        Ok(m) => m.versions,
        Err(e) => {
            eprintln!("manifest fetch failed: {e:#}");
            Vec::new()
        }
    };
    let mut printed = 0usize;
    for v in &remote {
        let installed = local.contains(&v.id);
        println!(
            "{:<18} {:<9} {}",
            v.id,
            v.r#type,
            if installed { "installed" } else { "" }
        );
        printed += 1;
    }
    for name in &local {
        if !remote.iter().any(|v| &v.id == name) {
            println!("{name:<18} local     installed");
            printed += 1;
        }
    }
    if printed == 0 {
        bail!("no versions found (offline and no cache?)");
    }
    Ok(())
}
