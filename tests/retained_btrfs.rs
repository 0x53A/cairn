//! Real Btrfs backing tests; cleanup only visits our disposable fixture directory.
use anyhow::{Context, Result, ensure};
use cairn::{
    chunking::Chunker,
    objects::Keys,
    snapshot::{self, CaptureOptions},
    store::Store,
};
use std::{
    fs,
    os::unix::fs::{MetadataExt, symlink},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Fixture(tempfile::TempDir);
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn server(repo: &Path) -> Result<(Server, String)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    drop(listener);
    let process = Server(
        Command::new(env!("CARGO_BIN_EXE_cairn"))
            .arg("--repo")
            .arg(repo)
            .args(["serve", "--listen", &address.to_string()])
            .env_remove("CAIRN_TOKEN")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    for _ in 0..200 {
        if std::net::TcpStream::connect(address).is_ok() {
            return Ok((process, format!("http://{address}")));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    anyhow::bail!("server did not start")
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fn unlock(path: &Path) {
            let Ok(meta) = fs::symlink_metadata(path) else {
                return;
            };
            if !meta.is_dir() {
                return;
            }
            if meta.ino() == 256 {
                let _ = Command::new("btrfs")
                    .args(["property", "set", "-ts"])
                    .arg(path)
                    .args(["ro", "false"])
                    .output();
            }
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    unlock(&entry.path());
                }
            }
        }
        // On mounts without user_subvol_rm_allowed, empty subvolumes can still
        // be rmdir'ed. Only tests empty their own files to use this fallback.
        unlock(self.0.path());
    }
}

fn btrfs(args: &[&str], path: &Path) -> Result<()> {
    let output = Command::new("btrfs").args(args).arg(path).output()?;
    ensure!(
        output.status.success(),
        "Btrfs failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
#[ignore = "requires CAIRN_BTRFS_TEST_ROOT and Btrfs snapshot creation permissions"]
fn retained_identity_restore_export_and_materialize() -> Result<()> {
    let base = std::env::var_os("CAIRN_BTRFS_TEST_ROOT").context("set CAIRN_BTRFS_TEST_ROOT")?;
    let fixture = Fixture(
        tempfile::Builder::new()
            .prefix("cairn-retained-test-")
            .tempdir_in(base)?,
    );
    let scratch = fixture.0.path();
    let source = scratch.join("source");
    btrfs(&["subvolume", "create"], &source)?;
    let probe = scratch.join("delete-probe");
    btrfs(&["subvolume", "create"], &probe)?;
    let can_delete = Command::new("btrfs")
        .args(["subvolume", "delete"])
        .arg(&probe)
        .output()?
        .status
        .success();
    if !can_delete {
        fs::remove_dir(&probe)?;
    }
    let selected = source.join("selected");
    fs::create_dir(&selected)?;
    fs::write(source.join("outside"), "outside selected subtree")?;
    let data: Vec<u8> = (0..1024 * 1024).map(|i| (i * 17 % 251) as u8).collect();
    fs::write(selected.join("large"), &data)?;
    fs::write(selected.join("empty"), [])?;
    fs::write(selected.join("ignored"), "filtered")?;
    fs::create_dir(selected.join("empty-dir"))?;
    symlink("large", selected.join("link"))?;
    let ignore = scratch.join("ignore");
    fs::write(&ignore, "ignored\n")?;
    let options = CaptureOptions {
        ignore_file: Some(ignore),
        ..Default::default()
    };
    for secret in [[0; 32], [31; 32]] {
        let keys = Keys::from_bytes(secret);
        let repo = scratch.join(format!("repo-{}", keys.domain()));
        let store = Store::connect(repo.to_str().unwrap(), &keys.domain(), None)?;
        let key_path = scratch.join(format!("{}.key", keys.domain()));
        fs::write(&key_path, hex::encode(secret))?;
        let output = Command::new(env!("CARGO_BIN_EXE_cairn"))
            .arg("--repo")
            .arg(&repo)
            .arg("--key-file")
            .arg(&key_path)
            .arg("capture")
            .arg(&selected)
            .args(["--backend", "btrfs", "--tag", "kept"])
            .arg("--ignore-file")
            .arg(options.ignore_file.as_ref().unwrap())
            .output()?;
        ensure!(
            output.status.success(),
            "capture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let id = String::from_utf8(output.stdout)?.trim().to_owned();
        let snap = snapshot::load_snapshot(&store, &keys, &id)?;
        let backing = snap.backing.clone().context("missing Btrfs backing")?;
        drop(snap);
        assert!(backing.path.is_dir());
        assert!(fs::write(backing.path.join("forbidden"), "write").is_err());
        assert_eq!(
            cairn::capture::cleanup_abandoned(backing.path.parent().unwrap())?.removed,
            0
        );
        let ordinary = Store::Local(scratch.join(format!("ordinary-{}", keys.domain())));
        assert_eq!(
            snapshot::capture(&ordinary, &keys, &selected, &options)?,
            id
        );
        if can_delete {
            assert_eq!(
                snapshot::capture_btrfs(&store, &keys, &selected, None, &options)?,
                id
            );
            assert_eq!(fs::read_dir(backing.path.parent().unwrap())?.count(), 1);
        }
        fs::write(selected.join("large"), "source changed")?;
        snapshot::verify(&store, &keys, &id)?;
        let restored = scratch.join(format!("restored-{}", keys.domain()));
        snapshot::restore(&store, &keys, &id, &restored)?;
        assert_eq!(fs::read(restored.join("large"))?, data);
        assert!(!restored.join("ignored").exists());
        assert!(!restored.join("outside").exists());
        assert_eq!(fs::read_link(restored.join("link"))?, Path::new("large"));

        // A different read-only subvolume at the same pathname must fail closed.
        let moved = backing.path.with_extension("moved");
        fs::rename(&backing.path, &moved)?;
        let output = Command::new("btrfs")
            .args(["subvolume", "snapshot", "-r"])
            .arg(&source)
            .arg(&backing.path)
            .output()?;
        ensure!(output.status.success(), "replacement fixture failed");
        let error = snapshot::verify(&store, &keys, &id).unwrap_err();
        assert!(format!("{error:#}").contains("identity mismatch"));
        let replacement = backing.path.with_extension("replacement");
        fs::rename(&backing.path, &replacement)?;
        fs::rename(&moved, &backing.path)?;
        snapshot::verify(&store, &keys, &id)?;

        let copy = Store::Local(scratch.join(format!("copy-{}", keys.domain())));
        let conversion = CaptureOptions {
            chunker: Chunker::FastCdc,
            chunk_size: 4096,
            ignore_file: None,
        };
        snapshot::copy_btrfs(&store, &copy, &keys, &id, &conversion)?;
        assert!(
            snapshot::load_snapshot(&copy, &keys, &id)?
                .backing
                .is_none()
        );
        snapshot::verify(&copy, &keys, &id)?;
        assert!(backing.path.exists(), "export must retain source backing");

        // Keyless HTTP cannot expose/accept local backing records. The local
        // key-holding exporter sends ordinary encrypted objects to another server.
        let (_source_server, source_url) = server(&repo)?;
        let http_source = Store::connect(&source_url, &keys.domain(), None)?;
        assert!(http_source.get(&format!("snapshots/{id}")).is_err());
        assert!(http_source.exists(&format!("snapshots/{id}")).is_err());
        let http_repo = scratch.join(format!("http-{}", keys.domain()));
        let (_destination_server, destination_url) = server(&http_repo)?;
        let http_destination = Store::connect(&destination_url, &keys.domain(), None)?;
        let raw = store.get(&format!("snapshots/{id}"))?;
        let response = reqwest::blocking::Client::new()
            .put(format!(
                "{destination_url}/v1/{}/snapshots/{id}",
                keys.domain()
            ))
            .body(raw)
            .send()?;
        assert!(!response.status().is_success());
        let output = Command::new(env!("CARGO_BIN_EXE_cairn"))
            .arg("--repo")
            .arg(&repo)
            .arg("--key-file")
            .arg(&key_path)
            .args([
                "copy",
                "kept",
                "--to",
                &destination_url,
                "--chunker",
                "fastcdc",
                "--chunk-size",
                "4096",
                "--tag",
                "exported",
            ])
            .env_remove("CAIRN_DEST_TOKEN")
            .output()?;
        ensure!(
            output.status.success(),
            "copy failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(http_destination.resolve("exported")?, id);
        snapshot::verify(&http_destination, &keys, &id)?;
        let http_restored = scratch.join(format!("http-restored-{}", keys.domain()));
        snapshot::restore(&http_destination, &keys, &id, &http_restored)?;
        assert_eq!(fs::read(http_restored.join("large"))?, data);
        if secret != [0; 32] {
            assert!(snapshot::verify(&http_destination, &Keys::default(), &id).is_err());
        }

        // An interrupted export/conversion cannot publish after changed bytes.
        let set_ro = |value: &str| -> Result<()> {
            let output = Command::new("btrfs")
                .args(["property", "set", "-ts"])
                .arg(&backing.path)
                .args(["ro", value])
                .output()?;
            ensure!(output.status.success(), "set read-only failed");
            Ok(())
        };
        set_ro("false")?;
        assert!(snapshot::verify(&store, &keys, &id).is_err());
        fs::write(backing.path.join("selected/large"), vec![9u8; data.len()])?;
        set_ro("true")?;
        assert!(snapshot::verify(&store, &keys, &id).is_err());
        assert!(snapshot::materialize(&store, &keys, &id, &conversion).is_err());
        assert!(
            snapshot::load_snapshot(&store, &keys, &id)?
                .backing
                .is_some()
        );
        set_ro("false")?;
        fs::write(backing.path.join("selected/large"), &data)?;
        set_ro("true")?;

        // Hold a reader lease across a child conversion. It must wait until the
        // reader drops, then atomically replace the record under the same ID.
        let held = snapshot::load_snapshot(&store, &keys, &id)?;
        let mut child = Command::new(env!("CARGO_BIN_EXE_cairn"))
            .arg("--repo")
            .arg(&repo)
            .arg("--key-file")
            .arg(key_path)
            .args([
                "materialize",
                "kept",
                "--chunker",
                "fastcdc",
                "--chunk-size",
                "4096",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            child.try_wait()?.is_none(),
            "materialize did not wait for its reader"
        );
        drop(held);
        let output = child.wait_with_output()?;
        if can_delete {
            ensure!(
                output.status.success(),
                "materialize failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!backing.path.exists());
        } else {
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("is materialized; old backing remains")
            );
            eprintln!(
                "verified conversion with explicit post-publication deletion-permission failure"
            );
        }
        assert_eq!(store.resolve("kept")?, id);
        assert!(
            snapshot::load_snapshot(&store, &keys, &id)?
                .backing
                .is_none()
        );
        snapshot::verify(&store, &keys, &id)?;
        snapshot::materialize(&store, &keys, &id, &conversion)?;
        let restored = scratch.join(format!("converted-{}", keys.domain()));
        snapshot::restore(&store, &keys, &id, &restored)?;
        assert_eq!(fs::read(restored.join("large"))?, data);
        fs::write(selected.join("large"), &data)?;
    }
    let path: PathBuf = scratch.into();
    drop(fixture);
    ensure!(!path.exists(), "fixture cleanup failed: {}", path.display());
    Ok(())
}
