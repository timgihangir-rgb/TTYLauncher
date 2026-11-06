//! Example 3: fetch the official Mojang version manifest and inspect a version.
//!
//! This is where the launcher's version list comes from.
//!
//! Run:  cargo run --example manifest              # list releases
//!       cargo run --example manifest -- --all     # snapshots/beta/alpha too
//!       cargo run --example manifest -- 1.8.9     # details of one version

use serde::Deserialize;

const MANIFEST_URL: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

#[derive(Deserialize)]
struct Manifest {
    latest: Latest,
    versions: Vec<VersionEntry>,
}

#[derive(Deserialize)]
struct Latest {
    release: String,
    snapshot: String,
}

#[derive(Deserialize)]
struct VersionEntry {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    url: String,
}

#[derive(Deserialize)]
struct VersionJson {
    #[serde(rename = "mainClass")]
    main_class: String,
    #[serde(rename = "javaVersion")]
    java_version: Option<JavaVersion>,
    downloads: Downloads,
}

#[derive(Deserialize)]
struct JavaVersion {
    #[serde(rename = "majorVersion")]
    major: u32,
}

#[derive(Deserialize)]
struct Downloads {
    client: ClientDownload,
}

#[derive(Deserialize)]
struct ClientDownload {
    size: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    println!("Fetching version manifest...");
    let manifest: Manifest = ureq::get(MANIFEST_URL).call()?.body_mut().read_json()?;
    println!(
        "OK: {} versions. Latest release = {}, latest snapshot = {}\n",
        manifest.versions.len(),
        manifest.latest.release,
        manifest.latest.snapshot
    );

    // detail mode for a single version
    if let Some(want) = args.iter().find(|a| !a.starts_with("--")) {
        let entry = manifest
            .versions
            .iter()
            .find(|v| &v.id == want)
            .ok_or_else(|| format!("version {want} not found"))?;
        println!("Fetching {} version json...", entry.id);
        let json: VersionJson = ureq::get(&entry.url).call()?.body_mut().read_json()?;
        println!("  main class : {}", json.main_class);
        if let Some(j) = json.java_version {
            println!("  java       : needs Java {}", j.major);
        }
        println!("  client jar : {:.1} MB", json.downloads.client.size as f64 / 1e6);
        return Ok(());
    }

    let show_all = args.iter().any(|a| a == "--all");
    println!("{:<14} {:<9} RELEASE DATE", "VERSION", "TYPE");
    for v in &manifest.versions {
        if !show_all && v.kind != "release" {
            continue;
        }
        println!("{:<14} {:<9}", v.id, v.kind);
    }
    Ok(())
}
