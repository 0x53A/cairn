# Chunking research and implementation, 2026-09-21

Established backup tools, heuristics and per-file-type policies were examined
before selecting Cairn's implementation. Cairn offers an explicit
`--chunker fastcdc` option; fixed-size remains the default. No automatic file-type
classifier or compression was added. Benefits depend on edit pattern, not just
extension.

## Primary sources examined

- Kopia registers fixed-size, Buzhash and Rabin-Karp splitters, with size variants
  from 128 KiB to 8 MiB. Its default in the inspected source is
  `DYNAMIC-4M-BUZHASH`. Its incremental splitter API takes byte slices and exposes
  a maximum segment size. Inspected commit `0295aac97fe7e1f9d226b704af0b334ac49e08eb`.
  [Source](https://github.com/kopia/kopia/blob/0295aac97fe7e1f9d226b704af0b334ac49e08eb/repo/splitter/splitter.go).
  [Buzhash](https://github.com/kopia/kopia/blob/0295aac97fe7e1f9d226b704af0b334ac49e08eb/repo/splitter/splitter_buzhash32.go)
  uses a 64-byte window, minimum target/2 and maximum target*2.
- Kopia permits a splitter override through scoped policies.
  [Policy reference](https://kopia.io/docs/reference/command-line/common/policy-set/)
  Its documented file-size/extension heuristics control compression, which happens
  after splitting and deduplication. These should not be mistaken for evidence of
  automatic format-aware chunking. Its actual
  [SplitterForFile](https://github.com/kopia/kopia/blob/0295aac97fe7e1f9d226b704af0b334ac49e08eb/snapshot/policy/splitter_policy.go)
  ignores the file entry and returns the configured algorithm. Its
  [compression policy](https://github.com/kopia/kopia/blob/0295aac97fe7e1f9d226b704af0b334ac49e08eb/snapshot/policy/compression_policy.go)
  uses extension and size.
- Restic documents variable-sized chunks from Rabin fingerprints over a 64-byte
  sliding window and a repository-selected polynomial.
  The [v0.4.0 chunker source](https://github.com/restic/chunker/blob/v0.4.0/chunker.go)
  uses a 512 KiB read buffer and a caller-supplied chunk buffer, with default
  512 KiB minimum and 8 MiB maximum chunks. Window, read-buffer and chunk size
  are separate bounds.
- Borg's stable 1.4 documentation describes Buzhash CDC and fixed-size splitting.
  Fixed splitting supports a separate header size and sparse-file processing;
  this is configurable layout support, not documented automatic format detection.
  [1.4.5 source](https://github.com/borgbackup/borg/blob/1.4.5/src/borg/chunker.pyx)
  reads data in bounded blocks and selects the chunker from explicit parameters.
- Rust fastcdc 5.0.0 (MIT) implements FastCDC 2020, Gear hashing, normalization
  and min/max bounds. Its [streaming API](https://docs.rs/fastcdc/5.0.0/fastcdc/v2020/struct.StreamCDC.html)
  allocates a max-sized input buffer and returns one owned chunk. Inspected
  registry source `src/v2020/mod.rs`, including refill, drain, error handling and
  constructor bounds. No upstream code was copied into Cairn.

## Reproducible measurements

Run from Cairn's root inside `nix-shell`:

```sh
cargo run --release --example chunking_bench > design/chunking/benchmark.jsonl
cargo build --release
cargo run --release --example capture_bench -- target/release/cairn > design/chunking/capture-benchmark.jsonl
```

Host: Intel Core i5-1345U, Linux 7.2.6, rustc 1.95.0. Full pipeline storage was
local ext4; all inputs were generated temporary data. These measurements predate
the SSH backends; peak RSS is not a measurement of their overhead. GNU time,
provided by shell.nix, measures process peak RSS.

The [edit benchmark](benchmark.jsonl) has 150 JSON records. It compares production
fixed/FastCDC-level-1 splitters with experimental FastCDC-level-2, at 64 KiB and
1 MiB targets. Corpora include deterministic random bytes, structured log text,
sparse-image-like bytes, gzip data, recompressed gzip, and a tiny file. Edits are
unchanged input, prefix/middle insertion, middle deletion, append and page overwrite.
Timing is the median of three passes over in-memory inputs, including streaming
buffer copies and BLAKE3 chunk hashes. Corpus construction is excluded.

`new_unique_bytes` counts missing unique chunk payload bytes, not encrypted network
bytes. `recipe_json_bytes` counts chunk descriptors without page/envelope overhead.
These are synthetic microbenchmarks, not measured Kopia/restic/Borg performance,
not real application consistency tests, and not representative of every file type.
Tiny-file timing is especially noisy. Repeated bytes are logical data; this test
does not exercise sparse filesystem allocation or hole-seeking optimizations.

Selected results at a 1 MiB target (payload MiB needing transfer):

| Input/edit | Fixed | FastCDC level 1 | FastCDC level 2 |
| --- | ---: | ---: | ---: |
| 16 MiB random, insert 37 bytes at start | 16.00 | 1.44 | 1.40 |
| Log text, insert 37 bytes at start | 16.55 | 1.89 | 1.60 |
| 16 MiB random, overwrite aligned 4 KiB page | 1.00 | 2.72 | 2.27 |
| Sparse-image-like bytes, overwrite aligned page | 1.00 | 4.00 | 4.00 |
| Recompress gzip after raw prefix insertion | 8.00 | 8.00 | 8.00 |

FastCDC level 1 reused about 91% of the random file after prefix insertion. The
fixed splitter was roughly 2–3 times faster in the large-input splitting/hash
microbenchmarks. Level 2 is promising but these limited synthetic results do not
justify deviating from the library's canonical level-1 default. Keep that choice
explicit and use the golden boundary fixture to detect unintended changes.

The [full pipeline measurement](capture-benchmark.jsonl) captures/restores a
128 MiB generated random file with encryption and a 96 MiB virtual-address-space
limit on each child. Observed peak RSS was 9.0 MiB fixed capture, 22.4 MiB FastCDC
capture, 7.4 MiB fixed restore and 12.2 MiB FastCDC restore. Capture took 1.20/0.96
seconds respectively in this single run; ordering, caching and different object
counts affect those numbers, so they do not establish a throughput winner.
This is separate from the edit benchmark's in-memory corpus allocations.

## Implemented contract

- `--chunker fixed` stays default; existing `--chunk-size` behavior is preserved.
- `--chunker fastcdc` uses FastCDC 2020, normalization level 1, seed 0, minimum
  target/4 and maximum target*4. Target defaults to 1 MiB and must be a power of
  two from 4 KiB through 1 MiB, keeping chunks within the existing 4 MiB limit.
- Small files at or below the minimum use a smaller direct-read buffer and a
  single chunk. The size hint does not truncate input if a file grows; the usual
  post-read size/mtime checks still apply. No type detection is performed.
- The streaming reader retries EINTR, preserves cut points across short reads,
  stops on real I/O errors, and retains at most a max-sized input buffer plus one
  chunk. Encryption and transport have additional bounded buffers.
- Whole-file hashes and logical snapshot IDs are independent of chunker/target.
  Recipes already record offsets/lengths, so restore, verify and keyless copy need
  no algorithm flag and no format migration. Source diff hashes without CDC work.
- Existing snapshots keep their first stored recipe. Selecting a new chunker may
  upload unreferenced alternate chunks; it does not rewrite an existing snapshot.
  A retry should use the same chunker/target to maximize reuse.
- Boundary selection is not encryption or metadata hiding. Existing visibility
  limitations still apply. Algorithm, seed, normalization or dependency changes
  must be deliberate; tests pin a boundary digest over a deterministic fixture.

Validation: four reader unit tests and three integration tests cover bounds,
short reads/EINTR/errors, upstream boundary agreement, a golden fixture, repeated
data, multi-page recipes, identity, restore, comparison and encrypted copy/reuse.
Both chunkers passed 128 MiB capture/restore under a 96 MiB address-space cap and
actual killed-upload/killed-server retry tests. FastCDC HTTP capture and real
Btrfs-to-HTTP capture also passed. Current test coverage and commands are described
in the [operations notes](../AGENTS.md) and [root AGENTS.md](../../AGENTS.md).

Future work should use real workload corpora before choosing automatic per-path
policies, a new default, smaller target sizes, header-aware fixed splitting or
format parsing. Keep compression heuristics and sparse allocation handling
separate from boundary selection. No universal file-type heuristic is justified
by this measurement set.
