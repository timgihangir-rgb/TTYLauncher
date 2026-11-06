//! Example 4: install a Minecraft version and build its launch command
//! using the mc-launcher-core crate (no game start unless --run is passed).
//!
//! Run:  cargo run --example install_probe -- 1.20.1
//!       cargo run --example install_probe -- 1.20.1 --run

use mc_launcher_core::prelude::*;
use mc_launcher_core::progress::ProgressEvent;
use std::process::Command;

fn main() -> mc_launcher_core::Result<()> {
    let mut args = std::env::args().skip(1);
    let version = args.next().unwrap_or_else(|| "1.20.1".to_string());
    let run = args.any(|a| a == "--run");

    let minecraft_dir = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join(".minecraft");
    println!("minecraft dir : {}", minecraft_dir.display());
    println!("installing    : {version}\n");

    let launcher = Launcher::new(minecraft_dir);
    let mut done_tasks = 0u64;
    let install = launcher.install_with_progress(
        InstallRequest {
            minecraft_version: version.clone(),
            loader: None,
            java: JavaInstallPolicy::Auto,
        },
        &mut |event: ProgressEvent| match event {
            ProgressEvent::StageStarted { stage } => println!("  stage: {stage:?}"),
            ProgressEvent::TaskStarted { label, .. } => {
                print!("\r  [{done_tasks}] {label}                ");
                use std::io::Write;
                std::io::stdout().flush().ok();
            }
            ProgressEvent::TaskFinished { .. } => done_tasks += 1,
            ProgressEvent::TaskSkipped { label, reason } => {
                println!("\n  skipped {label}: {reason:?}")
            }
            ProgressEvent::BytesReceived { received, total, label } => {
                if let Some(t) = total
                    && t > 0
                {
                    let pct = received * 100 / t;
                    print!("\r  [{label}] {pct}% ({received}/{t} bytes)   ");
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                }
            }
        },
    )?;
    println!("\ninstalled profile: {}", install.version_id);

    let version_json = launcher.load_version(&install.version_id)?;
    let command = launcher.build_launch_command_from_version(
        &version_json,
        LaunchOptions {
            account: Account::offline("Steve"),
            ..Default::default()
        },
    )?;

    println!("\nlaunch command ({} args):", command.args.len());
    println!("  exe: {}", command.executable.display());
    println!("  dir: {}", command.working_dir.display());
    // print first few args so the command shape is visible
    for a in command.args.iter().take(6) {
        println!("  arg: {a}");
    }
    println!("  ...");

    if run {
        println!("\nstarting the game...");
        let status = Command::new(&command.executable)
            .args(&command.args)
            .current_dir(&command.working_dir)
            .status()?;
        println!("game exited with {status}");
    } else {
        println!("\n(dry run — pass --run to actually start Minecraft)");
    }
    Ok(())
}
