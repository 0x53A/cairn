//! End-to-end encrypted capture/restore with a 96 MiB address-space limit.
//! Requires GNU time (provided by shell.nix). Uses only generated temporary data.
//! cargo build --release
//! cargo run --release --example capture_bench -- target/release/cairn
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    fs::{self, File},
    io::Write,
    path::Path,
    process::Command,
};

#[derive(Serialize)]
struct Measurement {
    chunker: String,
    operation: String,
    source_bytes: u64,
    address_space_cap_kib: usize,
    peak_rss_kib: usize,
    elapsed_seconds: f64,
    user_seconds: f64,
    system_seconds: f64,
}
fn measure(
    binary: &Path,
    root: &Path,
    mode: &str,
    operation: &str,
    args: &[&str],
) -> Result<String> {
    let metrics = root.join("metrics");
    let output = Command::new("time")
        .args(["-f", "%M %e %U %S", "-o"])
        .arg(&metrics)
        .args([
            "bash",
            "-c",
            "ulimit -v 98304; exec \"$@\"",
            "cairn-benchmark",
        ])
        .arg(binary)
        .args(args)
        .env_remove("CAIRN_TOKEN")
        .output()?;
    ensure!(
        output.status.success(),
        "{mode} {operation}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let values = fs::read_to_string(metrics)?;
    let fields: Vec<_> = values.split_whitespace().collect();
    ensure!(fields.len() == 4, "unexpected GNU time output: {values}");
    println!(
        "{}",
        serde_json::to_string(&Measurement {
            chunker: mode.into(),
            operation: operation.into(),
            source_bytes: 128 * 1024 * 1024,
            address_space_cap_kib: 98304,
            peak_rss_kib: fields[0].parse()?,
            elapsed_seconds: fields[1].parse()?,
            user_seconds: fields[2].parse()?,
            system_seconds: fields[3].parse()?,
        })?
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
fn main() -> Result<()> {
    let binary = fs::canonicalize(
        std::env::args_os()
            .nth(1)
            .context("pass path to cairn release binary")?,
    )?;
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    let mut file = File::create(source.join("large"))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cairn capture benchmark v1");
    let mut xof = hasher.finalize_xof();
    let mut buffer = vec![0; 1024 * 1024];
    for _ in 0..128 {
        xof.fill(&mut buffer);
        file.write_all(&buffer)?;
    }
    file.sync_all()?;
    drop(file);
    let key = temp.path().join("key");
    fs::write(&key, hex::encode([93; 32]))?;
    for mode in ["fixed", "fastcdc"] {
        let repo = temp.path().join(format!("repo-{mode}"));
        let restored = temp.path().join(format!("restored-{mode}"));
        let id = measure(
            &binary,
            temp.path(),
            mode,
            "capture",
            &[
                "--repo",
                repo.to_str().unwrap(),
                "--key-file",
                key.to_str().unwrap(),
                "capture",
                source.to_str().unwrap(),
                "--live",
                "--chunker",
                mode,
            ],
        )?;
        measure(
            &binary,
            temp.path(),
            mode,
            "restore",
            &[
                "--repo",
                repo.to_str().unwrap(),
                "--key-file",
                key.to_str().unwrap(),
                "restore",
                &id,
                restored.to_str().unwrap(),
            ],
        )?;
    }
    Ok(())
}
