use anyhow::{Result, ensure};
use cairn::{objects::Keys, store::Store};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    process::{Command, Output},
};

fn cli(repo: &Path, key: Option<&Path>, args: &[&str]) -> Result<Output> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cairn"));
    // Direct reads must work even without btrfs-progs or other executables.
    command
        .env("PATH", "")
        .env_remove("CAIRN_TOKEN")
        .arg("--repo")
        .arg(repo);
    if let Some(key) = key {
        command.arg("--key-file").arg(key);
    }
    Ok(command.args(args).output()?)
}
fn success(output: Output) -> Result<String> {
    ensure!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[test]
fn live_cli_capture_restore_filter_diff_and_identity_without_btrfs_tools() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    fs::create_dir(source.join("empty"))?;
    let data: Vec<u8> = (0..20000).map(|i| (i % 251) as u8).collect();
    fs::write(source.join("file"), &data)?;
    fs::set_permissions(source.join("file"), fs::Permissions::from_mode(0o640))?;
    symlink("file", source.join("link"))?;
    fs::write(source.join("ignored.tmp"), "excluded")?;
    let ignore = temp.path().join("ignore");
    fs::write(&ignore, "*.tmp\n")?;
    let key = temp.path().join("key");
    fs::write(&key, hex::encode([31; 32]))?;
    let src = source.to_str().unwrap();
    let mut expected_id = None;
    for (name, key_path, keys) in [
        ("plain", None, Keys::default()),
        ("encrypted", Some(key.as_path()), Keys::from_bytes([31; 32])),
    ] {
        let repo = temp.path().join(name);
        let id = success(cli(
            &repo,
            key_path,
            &[
                "capture",
                src,
                "--live",
                "--ignore-file",
                ignore.to_str().unwrap(),
                "--tag",
                "initial",
                "--chunk-size",
                "4096",
            ],
        )?)?;
        if let Some(expected) = &expected_id {
            assert_eq!(&id, expected);
        } else {
            expected_id = Some(id.clone());
        }
        let store = Store::Local(repo.join(keys.domain()));
        assert_eq!(store.resolve("initial")?, id);
        let restored = temp.path().join(format!("restored-{name}"));
        success(cli(
            &repo,
            key_path,
            &["restore", "initial", restored.to_str().unwrap()],
        )?)?;
        assert_eq!(fs::read(restored.join("file"))?, data);
        assert_eq!(
            fs::metadata(restored.join("file"))?.permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(fs::read_link(restored.join("link"))?, Path::new("file"));
        assert!(restored.join("empty").is_dir());
        assert!(!restored.join("ignored.tmp").exists());
        let diff = || {
            cli(
                &repo,
                key_path,
                &[
                    "diff-source",
                    "initial",
                    src,
                    "--live",
                    "--ignore-file",
                    ignore.to_str().unwrap(),
                ],
            )
        };
        assert!(success(diff()?)?.is_empty());
        let count = || {
            fs::read_dir(repo.join(keys.domain()).join("objects")).map(|entries| entries.count())
        };
        let before = count()?;
        fs::write(source.join("file"), "changed")?;
        assert_eq!(success(diff()?)?, "M\t\"file\"");
        assert_eq!(count()?, before);
        assert_eq!(store.list("snapshots")?, vec![id]);
        fs::write(source.join("file"), &data)?;
        assert!(
            !cli(
                &repo,
                key_path,
                &["capture", src, "--live", "--snapshot-dir", src]
            )?
            .status
            .success()
        );
        assert!(
            !cli(
                &repo,
                key_path,
                &[
                    "diff-source",
                    "initial",
                    src,
                    "--live",
                    "--snapshot-dir",
                    src
                ]
            )?
            .status
            .success()
        );
    }
    let nested = source.join("repository");
    let output = cli(&nested, None, &["capture", src, "--live"])?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("repository must be outside"));
    Ok(())
}
