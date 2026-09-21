use anyhow::Result;
use cairn::{
    objects::{Keys, envelope},
    snapshot::{self, CaptureOptions},
    store::{Store, replicate},
};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};

fn local(path: &std::path::Path, keys: &Keys) -> Store {
    Store::Local(path.join(keys.domain()))
}
fn options(size: usize) -> CaptureOptions {
    CaptureOptions {
        chunk_size: size,
        ignore_file: None,
        ..Default::default()
    }
}

#[test]
fn identity_ignores_chunking_encryption_and_tags_and_restore_spans_pages() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    let data: Vec<u8> = (0..4096 * 130 + 17)
        .map(|i| ((i * 17 + i / 31) % 251) as u8)
        .collect();
    fs::write(source.join("large"), &data)?;
    fs::set_permissions(source.join("large"), fs::Permissions::from_mode(0o640))?;
    fs::create_dir(source.join("empty-dir"))?;
    fs::write(source.join("empty-file"), [])?;
    symlink("large", source.join("link"))?;
    for i in 0..130 {
        fs::write(source.join(format!("small-{i:03}")), b"same content")?;
    }
    let plain = Keys::default();
    let encrypted = Keys::from_bytes([7; 32]);
    let a = local(&temp.path().join("a"), &plain);
    let b = local(&temp.path().join("b"), &encrypted);
    let id_a = snapshot::capture(&a, &plain, &source, &options(4096))?;
    let id_b = snapshot::capture(&b, &encrypted, &source, &options(8192))?;
    assert_eq!(id_a, id_b);
    a.tag("current / α", &id_a)?;
    a.tag("another", &id_a)?;
    assert_eq!(a.resolve("current / α")?, id_a);
    for (store, keys, name) in [(&a, &plain, "restore-a"), (&b, &encrypted, "restore-b")] {
        let dest = temp.path().join(name);
        snapshot::restore(store, keys, &id_a, &dest)?;
        assert_eq!(fs::read(dest.join("large"))?, data);
        assert_eq!(
            fs::read_link(dest.join("link"))?,
            std::path::Path::new("large")
        );
        assert!(dest.join("empty-dir").is_dir());
        assert_eq!(
            fs::metadata(dest.join("large"))?.permissions().mode() & 0o777,
            0o640
        );
        assert!(snapshot::restore(store, keys, &id_a, &dest).is_err());
    }
    assert!(snapshot::load_snapshot(&b, &Keys::from_bytes([8; 32]), &id_b).is_err());
    Ok(())
}

#[test]
fn unrelated_snapshots_share_chunks_and_copy_only_missing_objects() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let keys = Keys::from_bytes([9; 32]);
    let a = local(&temp.path().join("a"), &keys);
    let b = local(&temp.path().join("b"), &keys);
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("a"), vec![42u8; 9000])?;
    let first = snapshot::capture(&a, &keys, &source, &options(4096))?;
    let copied = replicate(&a, &b, &first)?;
    assert!(copied.objects_copied > 0);
    assert_eq!(replicate(&a, &b, &first)?.objects_copied, 0);
    fs::rename(source.join("a"), source.join("renamed"))?;
    let second = snapshot::capture(&a, &keys, &source, &options(4096))?;
    assert_ne!(first, second);
    let copied = replicate(&a, &b, &second)?;
    assert_eq!(
        copied.objects_copied, 1,
        "only the changed directory page is new"
    );
    assert!(copied.objects_present > 0);
    snapshot::restore(&b, &keys, &second, &temp.path().join("restored"))?;
    Ok(())
}

#[test]
fn tags_are_atomic_and_never_overwrite() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let keys = Keys::default();
    let store = local(&temp.path().join("repo"), &keys);
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("f"), "a")?;
    let a = snapshot::capture(&store, &keys, &source, &options(4096))?;
    fs::write(source.join("f"), "b")?;
    let b = snapshot::capture(&store, &keys, &source, &options(4096))?;
    std::thread::scope(|scope| {
        let first = scope.spawn(|| store.tag("race", &a));
        let second = scope.spawn(|| store.tag("race", &b));
        assert_ne!(
            first.join().unwrap().is_ok(),
            second.join().unwrap().is_ok()
        );
    });
    let winner = store.resolve("race")?;
    store.tag("race", &winner)?;
    assert!([a, b].contains(&winner));
    Ok(())
}

#[test]
fn source_diff_does_not_ingest_and_filters_apply() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let keys = Keys::default();
    let store = local(&temp.path().join("repo"), &keys);
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("keep"), "original")?;
    fs::write(source.join("ignored.tmp"), "private")?;
    let ignore = temp.path().join("ignore");
    fs::write(&ignore, "*.tmp\n")?;
    let opts = CaptureOptions {
        ignore_file: Some(ignore),
        ..options(4096)
    };
    let id = snapshot::capture(&store, &keys, &source, &opts)?;
    let original = snapshot::snapshot_index(&store, &keys, &id)?;
    assert!(!original.contains_key("ignored.tmp"));
    let Store::Local(root) = &store else {
        unreachable!()
    };
    let count = fs::read_dir(root.join("objects"))?.count();
    fs::write(source.join("keep"), "changed")?;
    let live = snapshot::source_index(&keys, &source, &opts)?;
    assert_eq!(
        snapshot::differences(&original, &live),
        vec![('M', "keep".into())]
    );
    assert_eq!(fs::read_dir(root.join("objects"))?.count(), count);
    assert_eq!(store.list("snapshots")?, vec![id]);
    Ok(())
}

#[test]
fn authentication_detects_payload_and_inventory_tampering() -> Result<()> {
    let keys = Keys::from_bytes([3; 32]);
    let id = "a".repeat(64);
    let ciphertext = keys.seal(b"secret data", vec![id])?;
    assert!(!ciphertext.windows(11).any(|w| w == b"secret data"));
    let mut damaged = ciphertext.clone();
    *damaged.last_mut().unwrap() ^= 1;
    assert!(keys.open(&damaged).is_err());
    let mut damaged = ciphertext.clone();
    let i = damaged
        .windows(64)
        .position(|w| w == "a".repeat(64).as_bytes())
        .unwrap();
    damaged[i] = b'b';
    assert!(envelope(&damaged).is_err());
    // Even a rewritten public checksum must not bypass E2E authentication.
    let end = damaged.len() - 32;
    let checksum = blake3::hash(&damaged[..end]);
    damaged[end..].copy_from_slice(checksum.as_bytes());
    assert!(envelope(&damaged).is_ok());
    assert!(keys.open(&damaged).is_err());
    assert_eq!(Keys::from_bytes([0; 32]).domain(), "plain");
    Ok(())
}

#[test]
fn missing_chunks_cannot_publish_a_replica() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let keys = Keys::default();
    let a = local(&temp.path().join("a"), &keys);
    let b = local(&temp.path().join("b"), &keys);
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("f"), "content")?;
    let id = snapshot::capture(&a, &keys, &source, &options(4096))?;
    let Store::Local(root) = &a else {
        unreachable!()
    };
    for e in fs::read_dir(root.join("objects"))? {
        let path = e?.path();
        let (body, _) = keys.open(&fs::read(&path)?)?;
        if body.first() == Some(&b'C') {
            fs::remove_file(path)?;
            break;
        }
    }
    assert!(replicate(&a, &b, &id).is_err());
    assert!(b.list("snapshots")?.is_empty());
    Ok(())
}
