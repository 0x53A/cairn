# Restore modes: implementation and remaining contract

As of 2026-09-22, `--mode new|swap` and swap's `--reuse hash|never` are implemented.
New remains the default and requires an absent destination. Update, delete-extra,
quick reuse and auxiliary timestamp metadata remain proposed below.

## Implemented staged restore

`restore SNAPSHOT DEST --mode swap [--reuse hash|never] [--json]` requires an
existing real directory and a trusted, writable parent. Quiesce destination writers
and do not rename its ancestors during restore. Linux `openat2` with no-symlink and
no-mount-crossing resolution is required. A destination that is a mount point or
Btrfs subvolume is refused. Reuse refuses mount crossings, including bind mounts,
and never follows a symlink below the old root. Type conflicts are populated from
the snapshot instead; the old entry remains displaced. Extras are not traversed
for reuse or removed; mounted extras are retained with the old tree.

A private `.cairn-restore-*` directory in the destination's parent holds a `tree`
child. Files and directories are verified and synchronized before a single
`renameat2(RENAME_EXCHANGE)` publishes that child. Unsupported atomic exchange fails
without a two-rename fallback. Both changed parent directories are synchronized
after publication. A post-exchange sync failure explicitly reports that publication
already happened. These fsync paths have not been power-loss fault tested.

Hash reuse is the default. Matching regular-file type and size permit a bounded
copy-and-hash pass from the old tree; matching BLAKE3 allows that copy to be used.
The resulting file is reread and hash-verified too. A digest mismatch discards the
copy and downloads the stored recipe instead. No hard links or reflinks are used
yet; old hard-link aliases and permissions are untouched. New permissions come
from the snapshot. This saves repository payload reads for reused files, but still
writes their full content locally and may write a rejected candidate before download.
There are no automatic performance thresholds or timestamp shortcuts. Reuse verifies
restored content, not availability of unused stored chunks; use `verify` separately
to check repository integrity.

Staging paths are printed to stderr before building. On success the old tree remains
at `.cairn-restore-*/tree`, and its absolute path is printed and returned in JSON.
On errors or interruption staging is retained for explicit inspection and cleanup.
A retry creates fresh staging and never sweeps older directories. SIGKILL after
exchange may leave an already-published tree without final output; the previously
printed staging path still identifies the retained old tree. Cairn never recursively
deletes a displaced tree.

`--json` works for both modes, returning regular-file counts `downloaded` and
`hash_reused`, `directories_created`, `symlinks_created`, and `displaced_tree`
(null for new mode). Empty files count even with no payload transfer. `--reuse`
in new mode is rejected. Heuristic/deletion counters are deferred with those modes.
Existing `snapshot::restore` callers retain new-mode behavior; the richer library
API is `restore::run`.

Regression coverage includes encrypted large-file reuse with stored chunks absent,
equal-size changed content, mode-only differences, independent hard links, symlink
and directory/file conflicts, retained extras, restrictive umask, failed downloads,
CLI JSON, and actual SIGKILL before publication followed by retry. Unsupported
exchange is propagated without a fallback; an unsupported-filesystem fixture has
not yet been run. Explicit private user/mount-namespace tests passed for bind-mount
root refusal and reuse crossing refusal, including same-device bind mounts.
Both chunkers' 128 MiB encrypted file fixtures also passed swap/hash reuse under
a 96 MiB process address-space cap.

Repeat the optional mount test with `nix-shell --run 'cargo test --test restore
-- --include-ignored'` on one line (requires util-linux and unprivileged namespaces).

## Independent choices

- `--mode new` (implemented default): require an absent destination.
- `--mode update` (proposed): reuse an existing directory; replace conflicting entries with
  snapshot entries. Do not follow destination symlinks when walking or writing.
- `--mode swap` (implemented): build a complete sibling tree, verify according to the selected
  reuse policy, then publish it. This is opt-in. Require same-filesystem atomic
  rename/exchange support; do not silently fall back to a two-rename gap.
- `--delete-extra` (proposed): explicitly remove destination paths absent from the snapshot
  in update mode. Otherwise leave extras for the caller to manage. An extra path
  blocking a required snapshot entry is a type conflict, not an ignorable extra.
  Swap publishes exactly the snapshot tree; retain the displaced old tree and
  report its path for explicit cleanup. A caller quiesces writers during restore.
- `--reuse never|hash|quick`: choose how existing regular-file payloads are reused.
  Never/hash are implemented for swap; quick and update remain proposed.
  Default for swap is hash, also proposed for update. New mode has nothing to reuse.

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

Future swap optimization can reuse validated files via reflinks where available;
the implementation currently copies them locally. Avoid hard links between staging
and the old live tree. Swap needs space
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
