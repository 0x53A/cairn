use anyhow::Result;
use cairn::{
    objects::Keys,
    restore::{self, RestoreMode, Reuse},
    snapshot::{self, CaptureOptions},
    store::Store,
};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    process::Command,
};

#[test]
fn swap_hash_reuses_content_reconciles_modes_and_retains_dirty_tree() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    let live = temp.path().join("live");
    fs::create_dir(&source)?;
    fs::write(source.join("unchanged"), vec![17; 2 * 1024 * 1024 + 7])?;
    fs::write(source.join("changed"), b"wanted")?;
    fs::write(source.join("was-directory"), b"now a file")?;
    fs::create_dir(source.join("was-file"))?;
    fs::write(source.join("was-file/child"), b"child")?;
    fs::create_dir(source.join("was-link"))?;
    fs::write(source.join("was-link/child"), b"new child")?;
    symlink("unchanged", source.join("link"))?;
    fs::write(source.join("empty"), b"")?;
    fs::set_permissions(source.join("unchanged"), fs::Permissions::from_mode(0o640))?;
    let keys = Keys::from_bytes([93; 32]);
    let repository = temp.path().join("repo");
    let store = Store::Local(repository.clone());
    let id = snapshot::capture(&store, &keys, &source, &CaptureOptions::default())?;
    fs::create_dir(&live)?;
    fs::copy(source.join("unchanged"), live.join("unchanged"))?;
    fs::set_permissions(live.join("unchanged"), fs::Permissions::from_mode(0o600))?;
    let outside = temp.path().join("outside-link");
    fs::hard_link(live.join("unchanged"), &outside)?;
    fs::write(live.join("changed"), b"dirty!")?; // Same size, different bytes.
    fs::create_dir(live.join("was-directory"))?;
    fs::write(live.join("was-directory/extra"), b"keep old")?;
    fs::write(live.join("was-file"), b"old file")?;
    let foreign = temp.path().join("foreign");
    fs::create_dir(&foreign)?;
    // Matching content must still not be reused through the directory symlink.
    fs::write(foreign.join("child"), b"new child")?;
    symlink(&foreign, live.join("was-link"))?;
    fs::write(live.join("extra"), b"extra retained")?;
    fs::write(live.join("empty"), b"")?;
    let old_inode = live.metadata()?.ino();
    // Reused files must need no repository payload download.
    for entry in fs::read_dir(repository.join("objects"))? {
        let path = entry?.path();
        let (bytes, _) = keys.open(&fs::read(&path)?)?;
        if bytes.first() == Some(&b'C') && bytes[1..].iter().all(|b| *b == 17) {
            fs::remove_file(path)?;
        }
    }
    let stats = restore::run(&store, &keys, &id, &live, RestoreMode::Swap, None)?;
    assert_eq!(stats.hash_reused, 2);
    assert_eq!(stats.downloaded, 4);
    let displaced = stats.displaced_tree.unwrap();
    assert_eq!(displaced.metadata()?.ino(), old_inode);
    assert_ne!(live.metadata()?.ino(), old_inode);
    assert_eq!(fs::read(displaced.join("changed"))?, b"dirty!");
    assert_eq!(fs::read(displaced.join("extra"))?, b"extra retained");
    assert_eq!(
        fs::read(displaced.join("was-directory/extra"))?,
        b"keep old"
    );
    assert_eq!(fs::read(live.join("changed"))?, b"wanted");
    assert_eq!(fs::read(live.join("was-directory"))?, b"now a file");
    assert_eq!(fs::read(live.join("was-file/child"))?, b"child");
    assert_eq!(fs::read(live.join("was-link/child"))?, b"new child");
    assert_eq!(fs::read(foreign.join("child"))?, b"new child");
    assert!(!live.join("extra").exists());
    assert_eq!(live.join("unchanged").metadata()?.mode() & 0o777, 0o640);
    assert_eq!(outside.metadata()?.mode() & 0o777, 0o600);
    assert_ne!(
        live.join("unchanged").metadata()?.ino(),
        outside.metadata()?.ino()
    );
    assert_eq!(
        fs::read_link(live.join("link"))?,
        std::path::Path::new("unchanged")
    );
    let before = snapshot::source_index(&keys, &live, &CaptureOptions::default())?;
    assert_eq!(before, snapshot::snapshot_index(&store, &keys, &id)?);
    // Never-reuse really downloads, and a missing payload must not publish.
    let inode = live.metadata()?.ino();
    let error = restore::run(
        &store,
        &keys,
        &id,
        &live,
        RestoreMode::Swap,
        Some(Reuse::Never),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("staging retained"));
    assert_eq!(live.metadata()?.ino(), inode);
    assert_eq!(
        before,
        snapshot::source_index(&keys, &live, &CaptureOptions::default())?
    );
    Ok(())
}

#[test]
fn restore_cli_handles_restrictive_umask_json_and_refused_destinations() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    fs::create_dir(source.join("child"))?;
    fs::write(source.join("child/file"), b"payload")?;
    fs::set_permissions(source.join("child"), fs::Permissions::from_mode(0o500))?;
    let repository = temp.path().join("repo");
    let store = Store::connect(repository.to_str().unwrap(), "plain", None)?;
    let id = snapshot::capture(
        &store,
        &Keys::default(),
        &source,
        &CaptureOptions::default(),
    )?;
    let live = temp.path().join("live");
    let output = Command::new("sh")
        .args(["-c", "umask 0777; exec \"$@\"", "sh"])
        .arg(env!("CARGO_BIN_EXE_cairn"))
        .arg("--repo")
        .arg(&repository)
        .arg("restore")
        .arg(&id)
        .arg(&live)
        .arg("--json")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stats: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(stats["downloaded"], 1);
    assert_eq!(stats["directories_created"], 2);
    assert_eq!(fs::read(live.join("child/file"))?, b"payload");
    assert_eq!(live.join("child").metadata()?.mode() & 0o777, 0o500);
    assert!(snapshot::restore(&store, &Keys::default(), &id, &live).is_err());
    let absent = temp.path().join("absent");
    assert!(
        restore::run(
            &store,
            &Keys::default(),
            &id,
            &absent,
            RestoreMode::New,
            Some(Reuse::Hash)
        )
        .is_err()
    );
    assert!(!absent.exists());
    assert!(
        restore::run(
            &store,
            &Keys::default(),
            &id,
            &absent,
            RestoreMode::Swap,
            None
        )
        .is_err()
    );
    let link = temp.path().join("link");
    symlink(&live, &link)?;
    assert!(
        restore::run(
            &store,
            &Keys::default(),
            &id,
            &link,
            RestoreMode::Swap,
            None
        )
        .is_err()
    );
    assert!(link.symlink_metadata()?.file_type().is_symlink());
    // Restore permissions for fixture cleanup.
    fs::set_permissions(source.join("child"), fs::Permissions::from_mode(0o700))?;
    fs::set_permissions(live.join("child"), fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[test]
fn swap_with_never_reuse_downloads_complete_tree_and_reports_displaced_path() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("file"), b"expected")?;
    let repository = temp.path().join("repo");
    let store = Store::connect(repository.to_str().unwrap(), "plain", None)?;
    let id = snapshot::capture(
        &store,
        &Keys::default(),
        &source,
        &CaptureOptions::default(),
    )?;
    let live = temp.path().join("live");
    fs::create_dir(&live)?;
    fs::write(live.join("file"), b"old data")?;
    let output = Command::new(env!("CARGO_BIN_EXE_cairn"))
        .arg("--repo")
        .arg(&repository)
        .arg("restore")
        .arg(&id)
        .arg(live.join("."))
        .args(["--mode", "swap", "--reuse", "never", "--json"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stats: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(stats["downloaded"], 1);
    assert_eq!(stats["hash_reused"], 0);
    let old = std::path::Path::new(stats["displaced_tree"].as_str().unwrap());
    assert_eq!(fs::read(old.join("file"))?, b"old data");
    assert_eq!(fs::read(live.join("file"))?, b"expected");
    Ok(())
}

#[test]
#[ignore = "requires util-linux and unprivileged user/mount namespaces"]
fn swap_refuses_bind_mount_roots_and_reuse_crossings() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    fs::create_dir_all(source.join("mounted"))?;
    fs::write(source.join("mounted/file"), b"snapshot")?;
    let repository = temp.path().join("repo");
    let store = Store::connect(repository.to_str().unwrap(), "plain", None)?;
    let id = snapshot::capture(
        &store,
        &Keys::default(),
        &source,
        &CaptureOptions::default(),
    )?;
    let live = temp.path().join("live");
    fs::create_dir_all(live.join("mounted"))?;
    fs::write(live.join("keep"), b"original")?;
    let foreign = temp.path().join("foreign");
    fs::create_dir(&foreign)?;
    fs::write(foreign.join("file"), b"foreign")?;
    // All mounts are private to the child namespace and disappear on exit.
    // Both bind mounts use the same device: st_dev alone cannot detect these.
    let output = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "--propagation", "private", "sh", "-c", r#"
set -eu
binary=$1
fixture=$2
snapshot=$3
mount --bind "$fixture/foreign" "$fixture/live/mounted"
if "$binary" --repo "$fixture/repo" restore "$snapshot" "$fixture/live" --mode swap > "$fixture/out" 2> "$fixture/error"; then
    exit 10
fi
umount "$fixture/live/mounted"
mount --bind "$fixture/foreign" "$fixture/live"
if "$binary" --repo "$fixture/repo" restore "$snapshot" "$fixture/live" --mode swap --reuse never > "$fixture/out" 2> "$fixture/root-error"; then
    exit 11
fi
umount "$fixture/live"
"#, "cairn-mount-test"])
        .arg(env!("CARGO_BIN_EXE_cairn")).arg(temp.path()).arg(id).output()?;
    assert!(
        output.status.success(),
        "mount fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_to_string(temp.path().join("error"))?
            .contains("without crossing mounts or symlinks")
    );
    assert!(fs::read_to_string(temp.path().join("root-error"))?.contains("parent's mount"));
    assert_eq!(fs::read(live.join("keep"))?, b"original");
    assert_eq!(fs::read(foreign.join("file"))?, b"foreign");
    Ok(())
}
