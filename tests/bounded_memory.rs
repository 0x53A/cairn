use anyhow::{Result, ensure};
use std::{
    fs::{self, File},
    process::Command,
};

#[test]
fn capture_and_restore_file_larger_than_process_address_space() -> Result<()> {
    bounded_roundtrip("fixed")
}

#[test]
fn fastcdc_capture_and_restore_file_larger_than_process_address_space() -> Result<()> {
    bounded_roundtrip("fastcdc")
}

fn bounded_roundtrip(chunker: &str) -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    File::create(source.join("large"))?.set_len(128 * 1024 * 1024)?;
    let repo = temp.path().join("repo");
    let key = temp.path().join("key");
    fs::write(&key, hex::encode([5u8; 32]))?;
    let binary = env!("CARGO_BIN_EXE_cairn");
    let capture = Command::new("bash")
        .args([
            "-c",
            "ulimit -v 98304; exec \"$1\" --repo \"$2\" --key-file \"$4\" capture \"$3\" --live --chunker \"$5\"",
            "cairn-memory-test",
        ])
        .arg(binary)
        .arg(&repo)
        .arg(&source)
        .arg(&key)
        .arg(chunker)
        .output()?;
    ensure!(
        capture.status.success(),
        "capture under 96 MiB address-space cap: {}",
        String::from_utf8_lossy(&capture.stderr)
    );
    let id = String::from_utf8(capture.stdout)?.trim().to_owned();
    let destination = temp.path().join("restored");
    let restore = Command::new("bash")
        .args([
            "-c",
            "ulimit -v 98304; exec \"$1\" --repo \"$2\" --key-file \"$5\" restore \"$3\" \"$4\"",
            "cairn-memory-test",
        ])
        .arg(binary)
        .arg(&repo)
        .arg(&id)
        .arg(&destination)
        .arg(&key)
        .output()?;
    ensure!(
        restore.status.success(),
        "restore under 96 MiB address-space cap: {}",
        String::from_utf8_lossy(&restore.stderr)
    );
    assert_eq!(
        fs::metadata(destination.join("large"))?.len(),
        128 * 1024 * 1024
    );
    Ok(())
}
