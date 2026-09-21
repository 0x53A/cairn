use anyhow::Result;
use cairn::{
    chunking::Chunker,
    objects::Keys,
    snapshot::{self, CaptureOptions},
    store::{Store, replicate},
};
use std::{collections::HashSet, fs};

fn data(size: usize) -> Vec<u8> {
    let mut bytes = vec![0; size];
    let mut h = blake3::Hasher::new();
    h.update(b"cairn chunking fixture v1");
    h.finalize_xof().fill(&mut bytes);
    bytes
}
fn options(chunker: Chunker, size: usize) -> CaptureOptions {
    CaptureOptions {
        chunker,
        chunk_size: size,
        ignore_file: None,
    }
}

#[test]
fn snapshot_identity_restore_copy_and_diff_ignore_chunker() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("large"), data(2 * 1024 * 1024 + 37))?;
    fs::write(source.join("tiny"), b"one chunk")?;
    fs::write(source.join("empty"), b"")?;
    let mut expected = None;
    for (i, (mode, size, keys)) in [
        (Chunker::Fixed, 5000, Keys::default()),
        (Chunker::FastCdc, 4096, Keys::default()),
        (Chunker::FastCdc, 16384, Keys::from_bytes([81; 32])),
    ]
    .into_iter()
    .enumerate()
    {
        let store = Store::Local(temp.path().join(format!("repo-{i}")));
        let id = snapshot::capture(&store, &keys, &source, &options(mode, size))?;
        if let Some(expected) = &expected {
            assert_eq!(&id, expected);
        } else {
            expected = Some(id.clone());
        }
        snapshot::verify(&store, &keys, &id)?;
        let replica = Store::Local(temp.path().join(format!("replica-{i}")));
        assert!(replicate(&store, &replica, &id)?.objects_copied > 0);
        assert_eq!(replicate(&store, &replica, &id)?.bytes_copied, 0);
        let restored = temp.path().join(format!("restored-{i}"));
        snapshot::restore(&replica, &keys, &id, &restored)?;
        assert_eq!(
            fs::read(restored.join("large"))?,
            fs::read(source.join("large"))?
        );
        assert_eq!(fs::read(restored.join("tiny"))?, b"one chunk");
        assert_eq!(fs::metadata(restored.join("empty"))?.len(), 0);
        let index = snapshot::snapshot_index(&store, &keys, &id)?;
        let current = snapshot::source_index(&keys, &source, &CaptureOptions::default())?;
        assert!(snapshot::differences(&index, &current).is_empty());
    }
    Ok(())
}

#[test]
fn prefix_insertion_reuses_most_chunks_in_encrypted_transfer() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    let original = data(8 * 1024 * 1024);
    fs::write(source.join("file"), &original)?;
    let keys = Keys::from_bytes([82; 32]);
    let a = Store::Local(temp.path().join("a"));
    let b = Store::Local(temp.path().join("b"));
    let opts = options(Chunker::FastCdc, 64 * 1024);
    let first = snapshot::capture(&a, &keys, &source, &opts)?;
    replicate(&a, &b, &first)?;
    let mut changed = b"a short insertion shifts every fixed block\n".to_vec();
    changed.extend_from_slice(&original);
    fs::write(source.join("file"), &changed)?;
    let second = snapshot::capture(&a, &keys, &source, &opts)?;
    assert_ne!(first, second);
    let transferred = replicate(&a, &b, &second)?;
    assert!(transferred.objects_present > 10);
    assert!(
        transferred.bytes_copied < original.len() as u64 / 10,
        "CDC failed to regain boundaries: {transferred:?}"
    );
    assert_eq!(replicate(&a, &b, &second)?.bytes_copied, 0);
    let restored = temp.path().join("restored");
    snapshot::restore(&b, &keys, &second, &restored)?;
    assert_eq!(fs::read(restored.join("file"))?, changed);
    Ok(())
}

#[test]
fn fastcdc_boundary_fixture_and_repeated_data_remain_stable() -> Result<()> {
    let bytes = data(1024 * 1024 + 37);
    let chunks = Chunker::FastCdc
        .chunks(bytes.as_slice(), 16384)?
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut digest = blake3::Hasher::new();
    for chunk in &chunks {
        digest.update(&(chunk.len() as u64).to_le_bytes());
    }
    assert_eq!(
        digest.finalize().to_hex().as_str(),
        "12fa1fed68e7ea050b70619f4719aa1a4414b8d688018b057e827ac9e98475ec",
        "boundary changes require an explicit compatibility decision"
    );
    let zeros = vec![0; 1024 * 1024];
    let chunks = Chunker::FastCdc
        .chunks(zeros.as_slice(), 16384)?
        .collect::<std::io::Result<Vec<_>>>()?;
    assert!(chunks.iter().all(|c| !c.is_empty() && c.len() <= 65536));
    let unique: HashSet<_> = chunks.iter().map(|c| blake3::hash(c)).collect();
    assert!(unique.len() <= 2);
    assert_eq!(chunks.concat(), zeros);
    Ok(())
}
