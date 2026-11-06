//! Launch command probe: builds the launch command for a version *without*
//! running it (no --run) and prints the executable + JVM/game arguments, so
//! one can verify that the classpath/labels actually match the version asked
//! for — the reproduction for "second version launches the first one".
//!
//! Run:  cargo run --example launch_probe -- 1.16.5
//!       cargo run --example launch_probe -- 1.20.1

use mc_launcher_core::prelude::*;

fn main() -> mc_launcher_core::Result<()> {
    let version = std::env::args().nth(1).unwrap_or_else(|| "1.16.5".into());
    let minecraft_dir = std::path::PathBuf::from(
        std::env::var_os("GAME_DIR").unwrap_or_else(|| std::env::var_os("HOME").unwrap())
            .to_string_lossy()
            .into_owned(),
    )
    .join(".minecraft");

    println!("minecraft dir : {}", minecraft_dir.display());
    println!("asked version : {version}\n");

    let launcher = Launcher::new(&minecraft_dir);
    let version_json = launcher.load_version(&version)?;
    println!("json id       : {:?}", version_json.id);
    println!("inherits_from : {:?}\n", version_json.inherits_from);

    let options = LaunchOptions {
        account: Account::offline("Probe".to_string()),
        custom_resolution: Some((640, 360)),
        ..Default::default()
    };
    let built = launcher.build_launch_command_from_version(&version_json, options)?;

    println!("executable    : {}", built.executable.display());
    println!("working dir   : {}", built.working_dir.display());
    println!("env keys      : {:?}\n", built.env);
    println!("--- args");
    for a in &built.args {
        println!("{a}");
    }
    Ok(())
}
