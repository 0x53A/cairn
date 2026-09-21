use anyhow::{Result, ensure};
use cairn::{
    objects::{Keys, envelope},
    snapshot::{self, CaptureOptions},
    store::{Store, replicate},
};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn saved_objects(root: &Path) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    Ok(fs::read_dir(root.join("objects"))?
        .map(|e| {
            let path = e?.path();
            Ok((path.clone(), fs::read(path)?))
        })
        .collect::<std::io::Result<_>>()?)
}

#[test]
fn corruption_and_missing_objects_are_reported_before_reuse_or_publication() -> Result<()> {
    for keys in [Keys::default(), Keys::from_bytes([61; 32])] {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        fs::create_dir(&source)?;
        fs::write(source.join("file"), vec![42u8; 9000])?;
        let root = temp.path().join("a");
        let a = Store::Local(root.clone());
        let b = Store::Local(temp.path().join("b"));
        let id = snapshot::capture(&a, &keys, &source, &CaptureOptions::default())?;
        assert!(snapshot::verify(&a, &keys, &id)? > 0);
        let objects = saved_objects(&root)?;
        let (path, original) = objects
            .iter()
            .find(|(_, bytes)| keys.open(bytes).unwrap().0[0] == b'C')
            .unwrap();
        let mut damaged = original.clone();
        let i = damaged.len() - 33;
        damaged[i] ^= 1;
        fs::write(path, &damaged)?;
        assert!(snapshot::verify(&a, &keys, &id).is_err());
        assert!(snapshot::restore(&a, &keys, &id, &temp.path().join("bad-restore")).is_err());
        assert!(replicate(&a, &b, &id).is_err());
        assert!(b.list("snapshots")?.is_empty());
        // Re-capture must not bless a corrupted existing deduplication hit.
        assert!(snapshot::capture(&a, &keys, &source, &CaptureOptions::default()).is_err());
        assert_eq!(
            fs::read(path)?,
            damaged,
            "never overwrite damaged evidence silently"
        );
        fs::remove_file(path)?;
        assert!(snapshot::verify(&a, &keys, &id).is_err());
        // Re-reading the original source can replenish a missing object.
        assert_eq!(
            snapshot::capture(&a, &keys, &source, &CaptureOptions::default())?,
            id
        );
        snapshot::verify(&a, &keys, &id)?;
        replicate(&a, &b, &id)?;
        snapshot::restore(&b, &keys, &id, &temp.path().join("good-restore"))?;
    }
    Ok(())
}

#[test]
fn existing_snapshot_with_different_recipe_must_also_be_complete() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("file"), vec![73; 17000])?;
    let keys = Keys::default();
    let a = Store::Local(temp.path().join("a"));
    let root_b = temp.path().join("b");
    let b = Store::Local(root_b.clone());
    let id = snapshot::capture(
        &a,
        &keys,
        &source,
        &CaptureOptions {
            chunk_size: 4096,
            ignore_file: None,
            ..Default::default()
        },
    )?;
    assert_eq!(
        snapshot::capture(
            &b,
            &keys,
            &source,
            &CaptureOptions {
                chunk_size: 8192,
                ignore_file: None,
                ..Default::default()
            }
        )?,
        id
    );
    let (path, bytes) = saved_objects(&root_b)?
        .into_iter()
        .find(|(_, bytes)| keys.open(bytes).unwrap().0.len() == 8193)
        .unwrap();
    fs::remove_file(&path)?;
    assert!(
        replicate(&a, &b, &id).is_err(),
        "copying another recipe cannot fix the installed recipe"
    );
    fs::write(path, bytes)?;
    replicate(&a, &b, &id)?;
    snapshot::verify(&b, &keys, &id)?;
    Ok(())
}

#[test]
fn old_envelopes_remain_readable_but_new_checksums_detect_truncation() -> Result<()> {
    let keys = Keys::from_bytes([44; 32]);
    let encoded = keys.seal(b"legacy-compatible", vec![])?;
    let mut old = encoded[..encoded.len() - 32].to_vec();
    old[..4].copy_from_slice(b"CRN1");
    assert_eq!(keys.open(&old)?.0, b"legacy-compatible");
    for missing in [1, 16, 32, encoded.len() - 7] {
        ensure!(
            envelope(&encoded[..encoded.len() - missing]).is_err(),
            "truncated checksum accepted"
        );
    }
    Ok(())
}

#[test]
fn snapshot_cleanup_ignores_unmarked_paths_and_refuses_invalid_markers() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let name = format!(".cairn-capture-{}", "0".repeat(32));
    let unrelated = temp.path().join(&name);
    fs::create_dir(&unrelated)?;
    fs::write(unrelated.join("keep"), "unrelated data")?;
    assert_eq!(cairn::capture::cleanup_abandoned(temp.path())?.removed, 0);
    let marker = temp.path().join(format!("{name}.lock"));
    fs::write(&marker, "not a Cairn intent record")?;
    assert!(cairn::capture::cleanup_abandoned(temp.path()).is_err());
    assert_eq!(
        fs::read_to_string(unrelated.join("keep"))?,
        "unrelated data"
    );
    assert!(marker.exists());
    Ok(())
}
