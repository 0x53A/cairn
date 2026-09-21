//! Deterministic synthetic edit benchmark, not a comparison of whole backup tools.
//! cargo run --release --example chunking_bench > design/chunking/benchmark.jsonl
use anyhow::Result;
use cairn::chunking::Chunker;
use fastcdc::v2020::{Normalization, StreamCDC};
use flate2::{Compression, write::GzEncoder};
use rand::{RngCore, SeedableRng};
use serde::Serialize;
use std::{
    collections::HashSet,
    io::{Cursor, Write},
    time::Instant,
};

const MIB: usize = 1024 * 1024;
#[derive(Clone, Copy, Debug)]
enum Algorithm {
    Fixed,
    FastCdc1,
    FastCdc2,
}
#[derive(Serialize)]
struct Row {
    corpus: String,
    edit: String,
    algorithm: String,
    target: usize,
    input_bytes: usize,
    chunks: usize,
    new_unique_bytes: usize,
    reused_bytes: usize,
    recipe_json_bytes: usize,
    min_chunk: usize,
    max_chunk: usize,
    median_mib_per_s: f64,
}
fn chunks(data: &[u8], algorithm: Algorithm, target: usize) -> Result<Vec<(String, usize)>> {
    let mut result = vec![];
    let mut push = |bytes: &[u8]| {
        // Match Cairn's plaintext typed chunk IDs, including the 'C' prefix.
        let mut h = blake3::Hasher::new();
        h.update(b"C");
        h.update(bytes);
        result.push((h.finalize().to_hex().to_string(), bytes.len()));
    };
    match algorithm {
        Algorithm::Fixed | Algorithm::FastCdc1 => {
            let mode = match algorithm {
                Algorithm::Fixed => Chunker::Fixed,
                _ => Chunker::FastCdc,
            };
            for chunk in mode.chunks_for_file(Cursor::new(data), target, Some(data.len() as u64))? {
                push(&chunk?);
            }
        }
        Algorithm::FastCdc2 => {
            let level = Normalization::Level2;
            if data.len() <= target / 4 {
                push(data);
                return Ok(result);
            }
            for chunk in
                StreamCDC::with_level(Cursor::new(data), target / 4, target, target * 4, level)
            {
                push(&chunk?.data);
            }
        }
    }
    Ok(result)
}
fn run(
    corpus: &str,
    original: &[u8],
    edited: &[u8],
    edit: &str,
    algorithm: Algorithm,
    target: usize,
) -> Result<()> {
    let known: HashSet<_> = chunks(original, algorithm, target)?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let mut rates = vec![];
    let mut recipe = vec![];
    for _ in 0..3 {
        let start = Instant::now();
        recipe = chunks(edited, algorithm, target)?;
        rates.push(edited.len() as f64 / MIB as f64 / start.elapsed().as_secs_f64());
    }
    rates.sort_by(f64::total_cmp);
    let mut seen_new = HashSet::new();
    let mut new_bytes = 0;
    let mut reused = 0;
    let mut offset = 0u64;
    let mut metadata_bytes = 0;
    for (id, len) in &recipe {
        if known.contains(id) {
            reused += len;
        } else if seen_new.insert(id) {
            new_bytes += len;
        }
        metadata_bytes += serde_json::to_vec(&cairn::snapshot::Chunk {
            id: id.clone(),
            offset,
            len: *len,
        })?
        .len();
        offset += *len as u64;
    }
    println!(
        "{}",
        serde_json::to_string(&Row {
            corpus: corpus.into(),
            edit: edit.into(),
            algorithm: format!("{algorithm:?}"),
            target,
            input_bytes: edited.len(),
            chunks: recipe.len(),
            new_unique_bytes: new_bytes,
            reused_bytes: reused,
            recipe_json_bytes: metadata_bytes,
            min_chunk: recipe.iter().map(|(_, n)| *n).min().unwrap_or(0),
            max_chunk: recipe.iter().map(|(_, n)| *n).max().unwrap_or(0),
            median_mib_per_s: rates[1],
        })?
    );
    Ok(())
}
fn gzip(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}
fn main() -> Result<()> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(0xc_a1_12);
    let mut random = vec![0; 16 * MIB];
    rng.fill_bytes(&mut random);
    let mut text = Vec::new();
    for i in 0..180_000u64 {
        writeln!(
            text,
            "2026-09-21 request={i:08} host=worker-{} path=/objects/{:016x} bytes={} status=200",
            i % 23,
            rng.next_u64(),
            rng.next_u32() % 65536
        )?;
    }
    let mut sparse = vec![0; 16 * MIB];
    for offset in (0..sparse.len()).step_by(256 * 1024) {
        rng.fill_bytes(&mut sparse[offset..offset + 4096]);
    }
    let compressed = gzip(&random)?;
    for (name, original) in [
        ("opaque-random", random),
        ("log-text", text),
        ("sparse-image", sparse),
        ("gzip-random", compressed),
    ] {
        for edit in [
            "unchanged",
            "insert-prefix",
            "insert-middle",
            "delete-middle",
            "append",
            "overwrite-page",
        ] {
            let mut edited = original.clone();
            match edit {
                "insert-prefix" => {
                    edited.splice(0..0, [0x31; 37]);
                }
                "insert-middle" => {
                    let mid = edited.len() / 2;
                    edited.splice(mid..mid, [0x32; 37]);
                }
                "delete-middle" => {
                    let mid = edited.len() / 2;
                    edited.drain(mid..mid + 37);
                }
                "append" => edited.extend([0x33; 4096]),
                "overwrite-page" => {
                    let mid = edited.len() / 2;
                    edited[mid..mid + 4096].fill(0x34);
                }
                _ => {}
            }
            for target in [64 * 1024, MIB] {
                for algorithm in [Algorithm::Fixed, Algorithm::FastCdc1, Algorithm::FastCdc2] {
                    run(name, &original, &edited, edit, algorithm, target)?;
                }
            }
        }
    }
    // Recompression can change the encoded stream globally; CDC cannot undo that.
    let mut raw = vec![0; 8 * MIB];
    rng.fill_bytes(&mut raw);
    let before = gzip(&raw)?;
    raw.splice(0..0, [0x35; 37]);
    let after = gzip(&raw)?;
    for algorithm in [Algorithm::Fixed, Algorithm::FastCdc1, Algorithm::FastCdc2] {
        run(
            "gzip-recompressed",
            &before,
            &after,
            "insert-raw-prefix",
            algorithm,
            MIB,
        )?;
    }
    // Tiny files are intentionally chunked separately, as capture does.
    for algorithm in [Algorithm::Fixed, Algorithm::FastCdc1, Algorithm::FastCdc2] {
        run(
            "small-file",
            b"small source file\n",
            b"small source file\n// edited\n",
            "append",
            algorithm,
            MIB,
        )?;
    }
    Ok(())
}
