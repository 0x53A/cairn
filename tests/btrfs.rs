//! Opt-in real filesystem tests. All writes stay in a newly created test directory.
use anyhow::{Context, Result, ensure};
use cairn::{
    capture::Source,
    objects::Keys,
    snapshot::{self, CaptureOptions},
    store::Store,
};
use std::{
    fs,
    net::TcpListener,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Fixture {
    directory: tempfile::TempDir,
    subvolumes: Vec<PathBuf>,
}
impl Fixture {
    fn new(root: &Path) -> Result<Self> {
        let output = Command::new("stat")
            .args(["-f", "-c", "%T"])
            .arg(root)
            .output()?;
        ensure!(
            output.status.success() && output.stdout == b"btrfs\n",
            "test root must be Btrfs"
        );
        Ok(Self {
            directory: tempfile::Builder::new()
                .prefix("cairn-btrfs-test-")
                .tempdir_in(root)?,
            subvolumes: vec![],
        })
    }
    fn subvolume(&mut self, name: &str) -> Result<PathBuf> {
        let path = self.directory.path().join(name);
        let output = Command::new("btrfs")
            .args(["subvolume", "create"])
            .arg(&path)
            .output()?;
        ensure!(
            output.status.success(),
            "create subvolume: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        self.subvolumes.push(path.clone());
        Ok(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for path in self.subvolumes.iter().rev() {
            match Command::new("btrfs")
                .args(["subvolume", "delete"])
                .arg(path)
                .output()
            {
                Ok(output) if output.status.success() => {}
                result => eprintln!(
                    "Btrfs fixture cleanup failed for {}: {result:?}",
                    path.display()
                ),
            }
        }
    }
}

fn assert_no_snapshots(parent: &Path) -> Result<()> {
    for entry in fs::read_dir(parent)? {
        ensure!(
            !entry?
                .file_name()
                .to_string_lossy()
                .starts_with(".cairn-capture-"),
            "temporary snapshot leaked"
        );
    }
    Ok(())
}

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn http_roundtrip(selected: &Path, snapshots: &Path, scratch: &Path) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    drop(listener);
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_cairn"))
            .arg("--repo")
            .arg(scratch.join("http-repository"))
            .args(["serve", "--listen", &address.to_string()])
            .env_remove("CAIRN_TOKEN")
            .stdout(Stdio::null())
            .spawn()?,
    );
    let mut ready = false;
    for _ in 0..200 {
        if std::net::TcpStream::connect(address).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    ensure!(ready, "HTTP server did not start");
    let key_file = scratch.join("http.key");
    fs::write(&key_file, hex::encode([23; 32]))?;
    let url = format!("http://{address}");
    let output = Command::new(env!("CARGO_BIN_EXE_cairn"))
        .args(["--repo", &url])
        .arg("--key-file")
        .arg(&key_file)
        .arg("capture")
        .arg(selected)
        .args(["--chunker", "fastcdc"])
        .arg("--snapshot-dir")
        .arg(snapshots)
        .env_remove("CAIRN_TOKEN")
        .current_dir(scratch)
        .output()?;
    ensure!(
        output.status.success(),
        "HTTP Btrfs capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_no_snapshots(snapshots)?;
    ensure!(
        !scratch.join("cairn-store").exists(),
        "unexpected local repository"
    );
    let id = String::from_utf8(output.stdout)?.trim().to_owned();
    let keys = Keys::from_bytes([23; 32]);
    let store = Store::connect(&url, &keys.domain(), None)?;
    let restored = scratch.join("http-restored");
    snapshot::restore(&store, &keys, &id, &restored)?;
    assert_eq!(
        fs::read(restored.join("file"))?,
        fs::read(selected.join("file"))?
    );
    let output = Command::new(env!("CARGO_BIN_EXE_cairn"))
        .args(["--repo", &url])
        .arg("--key-file")
        .arg(key_file)
        .args(["diff-source", &id])
        .arg(selected)
        .arg("--snapshot-dir")
        .arg(snapshots)
        .env_remove("CAIRN_TOKEN")
        .output()?;
    ensure!(
        output.status.success(),
        "HTTP Btrfs diff failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert_no_snapshots(snapshots)?;
    Ok(())
}

fn killed_capture_cleanup(selected: &Path, snapshots: &Path) -> Result<()> {
    // Hold the first HTTP request open so the child is killed while owning its
    // snapshot, rather than relying on a race against a short capture.
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let url = format!("http://{}", listener.local_addr()?);
    let mut child = Server(
        Command::new(env!("CARGO_BIN_EXE_cairn"))
            .args(["--repo", &url, "capture"])
            .arg(selected)
            .arg("--snapshot-dir")
            .arg(snapshots)
            .env_remove("CAIRN_TOKEN")
            .stdout(Stdio::null())
            .spawn()?,
    );
    let mut held = None;
    for _ in 0..500 {
        match listener.accept() {
            Ok((stream, _)) => {
                held = Some(stream);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(e) => return Err(e.into()),
        }
    }
    ensure!(held.is_some(), "capture did not reach HTTP upload");
    let active = cairn::capture::cleanup_abandoned(snapshots)?;
    assert_eq!((active.removed, active.active), (0, 1));
    child.0.kill()?;
    child.0.wait()?;
    drop(held);
    let output = Command::new(env!("CARGO_BIN_EXE_cairn"))
        .arg("cleanup-snapshots")
        .arg("--snapshot-dir")
        .arg(snapshots)
        .output()?;
    ensure!(
        output.status.success(),
        "recover abandoned snapshot: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout)?.trim(),
        "removed 1, active 0"
    );
    assert_no_snapshots(snapshots)?;
    assert!(selected.join("file").is_file());
    assert_eq!(cairn::capture::cleanup_abandoned(snapshots)?.removed, 0);
    Ok(())
}

#[test]
#[ignore = "requires CAIRN_BTRFS_TEST_ROOT and Btrfs snapshot create/delete permissions"]
fn real_btrfs_capture_isolation_restore_diff_and_cleanup() -> Result<()> {
    let root = std::env::var_os("CAIRN_BTRFS_TEST_ROOT")
        .context("set CAIRN_BTRFS_TEST_ROOT to the mounted test filesystem")?;
    let mut fixture = Fixture::new(Path::new(&root))?;
    let volume = fixture.subvolume("source")?;
    let snapshots = fixture.subvolume("snapshots")?;
    let selected = volume.join("selected");
    fs::create_dir(&selected)?;
    fs::write(volume.join("outside"), "must not be captured")?;
    fs::write(selected.join("file"), "before snapshot")?;
    fs::write(selected.join("ignored.tmp"), "ignored")?;
    let ignore = fixture.directory.path().join("ignore");
    fs::write(&ignore, "*.tmp\n")?;
    let options = CaptureOptions {
        ignore_file: Some(ignore),
        ..CaptureOptions::default()
    };
    let keys = Keys::from_bytes([17; 32]);
    let store = Store::Local(
        fixture
            .directory
            .path()
            .join("repository")
            .join(keys.domain()),
    );

    // Explicit destination is a different subvolume on the same filesystem.
    let source = Source::open(&selected, false, Some(&snapshots))?;
    assert!(fs::write(source.path.join("forbidden"), "read-only").is_err());
    fs::write(selected.join("file"), "after snapshot")?;
    let id = snapshot::capture(&store, &keys, &source.path, &options)?;
    let index = snapshot::snapshot_index(&store, &keys, &id)?;
    assert!(!index.contains_key("ignored.tmp"));
    assert!(!index.contains_key("outside"));
    let restored = fixture.directory.path().join("restored");
    snapshot::restore(&store, &keys, &id, &restored)?;
    assert_eq!(
        fs::read_to_string(restored.join("file"))?,
        "before snapshot"
    );
    source.finish()?;
    assert_no_snapshots(&snapshots)?;

    // Default snapshot destination, diff without ingesting, and Drop cleanup.
    let object_count = || -> Result<usize> {
        let Store::Local(root) = &store else {
            unreachable!()
        };
        Ok(fs::read_dir(root.join("objects"))?.count())
    };
    let objects_before = object_count()?;
    {
        let source = Source::open(&selected, false, None)?;
        let current = snapshot::source_index(&keys, &source.path, &options)?;
        assert_eq!(
            snapshot::differences(&index, &current),
            vec![('M', "file".into())]
        );
    }
    assert_eq!(object_count()?, objects_before);
    assert_no_snapshots(fixture.directory.path())?;

    // Snapshotting the mount's top-level subvolume also requires owning that
    // subvolume, not just the selected directory. Check the permission failure
    // on a root-owned mount, or the successful path when the test owns it.
    let ordinary = fixture.directory.path().join("ordinary");
    fs::create_dir(&ordinary)?;
    fs::write(ordinary.join("file"), "top-level subvolume")?;
    let mut containing = ordinary.clone();
    while fs::metadata(&containing)?.ino() != 256 {
        ensure!(containing.pop(), "missing containing subvolume");
    }
    if fs::metadata(&containing)?.uid() == fs::metadata(&ordinary)?.uid() {
        let source = Source::open(&ordinary, false, Some(&snapshots))?;
        assert_eq!(
            fs::read_to_string(source.path.join("file"))?,
            "top-level subvolume"
        );
        source.finish()?;
    } else {
        let error = match Source::open(&ordinary, false, Some(&snapshots)) {
            Ok(source) => {
                source.finish()?;
                anyhow::bail!("unexpected access to another user's subvolume");
            }
            Err(error) => error,
        };
        ensure!(
            format!("{error:#}").contains("Operation not permitted"),
            "unexpected error: {error:#}"
        );
        eprintln!("verified permission rejection for root-owned containing subvolume");
    }
    // Direct reads work even when the containing subvolume cannot be snapshotted.
    let direct = Source::open(&ordinary, true, None)?;
    let direct_index = snapshot::source_index(&keys, &direct.path, &CaptureOptions::default())?;
    assert!(direct_index.contains_key("file"));
    direct.finish()?;
    assert_no_snapshots(&snapshots)?;

    http_roundtrip(&selected, &snapshots, fixture.directory.path())?;
    killed_capture_cleanup(&selected, &snapshots)?;

    // Nested subvolumes become empty stubs in a snapshot: reject silent data loss.
    fixture.subvolume("source/selected/nested")?;
    fs::write(selected.join("nested/file"), "nested content")?;
    {
        let source = Source::open(&selected, false, Some(&snapshots))?;
        let error = snapshot::capture(&store, &keys, &source.path, &options).unwrap_err();
        ensure!(
            format!("{error:#}").contains("needs a separate capture"),
            "unexpected failure: {error:#}"
        );
    }
    assert_no_snapshots(&snapshots)?;
    assert_eq!(store.list("snapshots")?, vec![id]);
    let fixture_path = fixture.directory.path().to_owned();
    drop(fixture);
    ensure!(
        !fixture_path.exists(),
        "fixture cleanup left {}",
        fixture_path.display()
    );
    Ok(())
}
