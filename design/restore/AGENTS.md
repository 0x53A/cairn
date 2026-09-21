# Restore modes: proposed contract

Design discussion; these options are not implemented. Current restore still
requires a new destination and writes directly into it.

## Independent choices

- `--mode new` (default): require an absent destination, preserving current behavior.
- `--mode update`: reuse an existing directory; replace conflicting entries with
  snapshot entries. Do not follow destination symlinks when walking or writing.
- `--mode swap`: build a complete sibling tree, verify according to the selected
  reuse policy, then publish it. This is opt-in. Require same-filesystem atomic
  rename/exchange support; do not silently fall back to a two-rename gap.
- `--delete-extra`: explicitly remove destination paths absent from the snapshot
  in update mode. Otherwise leave extras for the caller to manage. An extra path
  blocking a required snapshot entry is a type conflict, not an ignorable extra.
  Swap publishes exactly the snapshot tree; retain the displaced old tree and
  report its path for explicit cleanup. A caller quiesces writers during restore.
- `--reuse never|hash|quick`: choose how existing regular-file payloads are reused.
  Proposed default for update/swap is hash. New mode has nothing to reuse.

Replacement should normally write/verify a temporary sibling file and rename it
over the destination file. This avoids corrupting the previous file on interrupted
download and avoids mutating external hard links. It costs temporary space for
changed files. Literal in-place byte writes would need a separate future option.
Directory/file/symlink conflicts need explicit deterministic handling; mounts and
nested subvolumes must not be traversed or deleted as ordinary directories.
Deletion in update mode follows successful required writes, not before downloads.
An interrupted update remains a mixed tree; it is not a transaction.

## What counts as unchanged

- Hash: matching regular-file type, size, and BLAKE3 over actual file bytes. Reuse
  content while reconciling requested permissions/metadata independently.
- Quick: matching regular-file type, size, and stored mtime at a supported precision.
  This is an explicit heuristic; equal size/mtime can conceal changed content.
  Missing/unsupported timestamp information falls back to hashing. Never report
  quick-reused content as hash-verified or insert it into a verified hash cache.
- Btrfs: future proven extent identity or a validated change map against a trusted
  baseline can avoid reads. Btrfs block checksums alone do not supply file identity.
  Without trustworthy provenance, hash or explicitly select the quick heuristic.

Current snapshots do not store mtimes. Quick mode needs optional auxiliary
timestamp metadata, outside logical identity, with backward-compatible missing
fields. Restore should preserve those timestamps if supplied. Identical logical
snapshots may keep the first stored representation, including auxiliary metadata;
do not promise recovery of every capture's timestamps for one logical ID.
Timestamp rounding/precision must be recorded or handled conservatively.

Swap can reuse validated files via reflinks where available, otherwise copy them
locally. Avoid hard links between staging and the old live tree. Swap needs space
for both trees unless shared extents reduce it; this differs from writing straight
into a new destination. Atomic publication changes a directory entry, not open
file descriptors or existing working directories held by other processes.

## Performance rationale and prior art

The user identified [Kopia issue #1535](https://github.com/kopia/kopia/issues/1535)
as relevant prior art: restoring between CI stages while retaining a dirty tree,
with roughly 10 GB unchanged out of 13 GB. It proposes separating skip/reuse from
deletion of extras. It motivates the workflow, not a measured hashing-versus-write
speed claim. Related implementation: [PR #2751](https://github.com/kopia/kopia/pull/2751).

[Kopia issue #1524](https://github.com/kopia/kopia/issues/1524) separately reports
restore overhead from per-file flushing. Its author reported about 290 seconds
initially and 126–147 seconds after changing the write path. This was a specific
Windows/VM workload, not a general benchmark for Cairn or SSDs. Atomic visibility
and durable flushing are distinct guarantees and should be benchmarked separately.

For an existing same-sized file in update mode, let H be measured read-and-hash
time, W full replacement time, and p the probability of equal content. Checking
first costs approximately H + (1-p)*W, versus W for always replacing. Thus checking
helps when H < p*W. This simplified model excludes concurrency/cache interactions.
W includes repository reads/network, decryption, writes and verification/durability;
current Cairn restore also rereads the output to check its complete BLAKE3 digest.
Reject differing size/type before hashing. Equal-size changed files can pay both
costs. Swap additionally needs a local copy or reflink for reused files.

Benchmark mostly unchanged and mostly changed trees, large files and many small
files, warm/cold caches, local/network repositories and flush policies before
introducing automatic performance thresholds. Keep hash/quick/never explicit.

## Reporting and validation

Machine-readable results should distinguish downloaded, hash-reused,
heuristically-reused, deleted and retained entries, and identify a displaced tree.
Keep full-file hash verification claims separate from heuristic completion.
Stable JSON output, cancellation and integration handoffs remain adjacent work.

Tests should cover unchanged large files, equal-size/mtime changed bytes, missing
timestamp metadata, timestamp precision, mode-only changes, symlink/hard-link and
type conflicts, extra-file policies, refused mount traversal, download failure,
interruption before publication, and unsupported atomic exchange. Do not exercise
dirty-tree operations on user data while implementing these modes.
