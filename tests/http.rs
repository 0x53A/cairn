use anyhow::{Context, Result, ensure};
use cairn::{
    objects::Keys,
    snapshot::{self, CaptureOptions},
    store::{Import, Store},
};
use std::{
    fs,
    net::TcpListener,
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Server {
    child: Child,
    url: String,
}
impl Server {
    fn start(root: &Path, token: &str, unix: bool) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        drop(listener);
        let socket = root.with_extension("sock");
        let mut command = Command::new(env!("CARGO_BIN_EXE_cairn"));
        command.arg("--repo").arg(root).arg("serve");
        if unix {
            command.arg("--unix-socket").arg(&socket);
        } else {
            command.args(["--listen", &addr.to_string()]);
        }
        let child = command
            .env("CAIRN_TOKEN", token)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let server = Self {
            child,
            url: if unix {
                format!("unix://{}", socket.display())
            } else {
                format!("http://{addr}")
            },
        };
        for _ in 0..200 {
            let ready = if unix {
                std::os::unix::net::UnixStream::connect(&socket).is_ok()
            } else {
                std::net::TcpStream::connect(addr).is_ok()
            };
            if ready {
                return Ok(server);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        anyhow::bail!("server failed to start")
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn live_cli_streams_encrypted_files_to_http_without_local_repository() -> Result<()> {
    live_roundtrip(false)?;
    live_roundtrip(true)
}

fn live_roundtrip(unix: bool) -> Result<()> {
    let temp = tempfile::tempdir()?;
    let server = Server::start(&temp.path().join("server"), "live-test-token", unix)?;
    let client = temp.path().join("client");
    let source = client.join("source");
    fs::create_dir_all(&source)?;
    fs::write(source.join("file"), vec![43u8; 3 * 1024 * 1024 + 19])?;
    let key = client.join("key");
    fs::write(&key, hex::encode([29; 32]))?;
    let output = Command::new(env!("CARGO_BIN_EXE_cairn"))
        .args(["--repo", &server.url])
        .arg("--key-file")
        .arg(&key)
        .arg("capture")
        .arg(&source)
        .args(["--live", "--tag", "direct"])
        .args(["--chunker", "fastcdc"])
        .env("CAIRN_TOKEN", "live-test-token")
        .env("PATH", "")
        .current_dir(&client)
        .output()?;
    ensure!(
        output.status.success(),
        "live HTTP capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let id = String::from_utf8(output.stdout)?.trim().to_owned();
    let keys = Keys::from_bytes([29; 32]);
    let store = Store::connect(&server.url, &keys.domain(), Some("live-test-token".into()))?;
    assert_eq!(store.resolve("direct")?, id);
    snapshot::verify(&store, &keys, &id)?;
    if unix {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(temp.path().join("server.sock"))?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(
            Store::connect(&server.url, &keys.domain(), None)?
                .list("snapshots")
                .is_err()
        );
        // The CLI must relay a Unix source, not ask a remote server to open it.
        let destination = Server::start(&temp.path().join("copy"), "destination-token", false)?;
        for _ in 0..2 {
            let copied = Command::new(env!("CARGO_BIN_EXE_cairn"))
                .args([
                    "--repo",
                    &server.url,
                    "copy",
                    "direct",
                    "--domain",
                    &keys.domain(),
                    "--to",
                    &destination.url,
                ])
                .env("CAIRN_TOKEN", "live-test-token")
                .env("CAIRN_DEST_TOKEN", "destination-token")
                .output()?;
            ensure!(
                copied.status.success(),
                "Unix copy failed: {}",
                String::from_utf8_lossy(&copied.stderr)
            );
        }
        let copied = Store::connect(
            &destination.url,
            &keys.domain(),
            Some("destination-token".into()),
        )?;
        snapshot::verify(&copied, &keys, &id)?;
    }
    let restored = temp.path().join("restored");
    snapshot::restore(&store, &keys, &id, &restored)?;
    assert_eq!(
        fs::read(restored.join("file"))?,
        fs::read(source.join("file"))?
    );
    let mut entries = fs::read_dir(&client)?
        .map(|entry| entry.map(|e| e.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort();
    assert_eq!(
        entries,
        vec![
            std::ffi::OsString::from("key"),
            std::ffi::OsString::from("source")
        ]
    );
    Ok(())
}

#[test]
fn unix_listener_preserves_existing_paths_and_rejects_conflicting_options() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("occupied");
    fs::write(&path, "leave this alone")?;
    let run = |extra: &[&str]| -> Result<std::process::Output> {
        Ok(Command::new(env!("CARGO_BIN_EXE_cairn"))
            .arg("--repo")
            .arg(temp.path().join("store"))
            .args(["serve", "--unix-socket"])
            .arg(&path)
            .args(extra)
            .env_remove("CAIRN_TOKEN")
            .output()?)
    };
    assert!(!run(&[])?.status.success());
    assert_eq!(fs::read_to_string(&path)?, "leave this alone");
    assert!(!run(&["--listen", "127.0.0.1:0"])?.status.success());
    assert!(Store::connect("unix://relative.sock", "plain", None).is_err());
    assert!(Store::connect("sftp://host/path", "plain", None).is_err());
    Ok(())
}

#[test]
fn unix_listener_recovers_crashes_and_cleans_up_on_shutdown() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("server");
    let socket = root.with_extension("sock");
    let mut first = Server::start(&root, "token", true)?;
    let duplicate = Command::new(env!("CARGO_BIN_EXE_cairn"))
        .arg("--repo")
        .arg(&root)
        .args(["serve", "--unix-socket"])
        .arg(&socket)
        .output()?;
    assert!(!duplicate.status.success());
    assert!(std::os::unix::net::UnixStream::connect(&socket).is_ok());
    first.child.kill()?;
    first.child.wait()?;
    assert!(socket.exists());
    let mut second = Server::start(&root, "token", true)?;
    ensure!(
        Command::new("kill")
            .args(["-TERM", &second.child.id().to_string()])
            .status()?
            .success(),
        "send SIGTERM"
    );
    for _ in 0..200 {
        if second.child.try_wait()?.is_some() {
            assert!(!socket.exists());
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    anyhow::bail!("server did not stop after SIGTERM")
}

#[test]
fn encrypted_http_capture_server_pull_and_restore_without_server_keys() -> Result<()> {
    server_pull(false)?;
    server_pull(true)
}

fn server_pull(unix_destination: bool) -> Result<()> {
    let temp = tempfile::tempdir()?;
    let a = Server::start(&temp.path().join("a"), "source-secret-token", false)?;
    let b = Server::start(
        &temp.path().join("b"),
        "destination-secret-token",
        unix_destination,
    )?;
    let keys = Keys::from_bytes([4; 32]);
    let source_store = Store::connect(&a.url, &keys.domain(), Some("source-secret-token".into()))?;
    let dest_store = Store::connect(
        &b.url,
        &keys.domain(),
        Some("destination-secret-token".into()),
    )?;
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("hello"), "hello from A")?;
    let id = snapshot::capture(&source_store, &keys, &source, &CaptureOptions::default())?;
    source_store.tag("first", &id)?;
    assert_eq!(source_store.resolve("first")?, id);
    let request = Import {
        source: a.url.clone(),
        snapshot: id.clone(),
        source_token: Some("source-secret-token".into()),
    };
    let stats = dest_store
        .remote_import(&request)
        .context("destination pulls from source")?;
    assert!(stats.objects_copied > 0);
    assert_eq!(dest_store.remote_import(&request)?.bytes_copied, 0);
    let output = temp.path().join("restored");
    snapshot::restore(&dest_store, &keys, &id, &output)?;
    assert_eq!(fs::read_to_string(output.join("hello"))?, "hello from A");
    assert!(
        Store::connect(&a.url, &keys.domain(), None)?
            .list("snapshots")
            .is_err()
    );
    let bad = keys.seal(b"incomplete", vec!["f".repeat(64)])?;
    assert!(
        source_store
            .put(&format!("snapshots/{}", "a".repeat(64)), &bad)
            .is_err()
    );
    assert!(!source_store.exists(&format!("snapshots/{}", "a".repeat(64)))?);
    Ok(())
}
