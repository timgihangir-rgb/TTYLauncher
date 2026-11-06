//! Print where Minecraft keeps its data, by inspecting the launch command
//! mc-launcher-core generates. `options.txt` (and saves) live in `--gameDir`,
//! which is what the launcher tunes for speed.
//!
//! Run:  cargo run --example gamedir_probe -- 1.20.1

use mc_launcher_core::prelude::*;

fn main() -> mc_launcher_core::Result<()> {
    let version = std::env::args().nth(1).unwrap_or_else(|| "1.20.1".into());
    let mc = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join(".minecraft");
    let launcher = Launcher::new(mc);
    let json = launcher.load_version(&version)?;
    let cmd = launcher.build_launch_command_from_version(
        &json,
        LaunchOptions {
            account: Account::offline("Steve"),
            custom_resolution: Some((1280, 720)),
            ..Default::default()
        },
    )?;

    println!("working_dir: {}", cmd.working_dir.display());
    for (i, a) in cmd.args.iter().enumerate() {
        if a == "--gameDir" {
            println!("--gameDir   : {}", cmd.args[i + 1]);
        }
    }
    let opts = cmd.working_dir.join("options.txt");
    println!("options.txt : {} (exists: {})", opts.display(), opts.exists());
    Ok(())
}
