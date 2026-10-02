# Retained Btrfs backing

Implemented on 2026-09-27. This is an opt-in local storage representation of the
same logical Cairn snapshot. The default remains portable chunk objects.

## Commands

```sh
cairn --repo /btrfs/cairn --key-file /private/cairn.key capture /btrfs/source --backend btrfs --tag first
cairn --repo /btrfs/cairn --key-file /private/cairn.key verify first
cairn --repo /btrfs/cairn --key-file /private/cairn.key restore first /btrfs/restored
cairn --repo /btrfs/cairn --key-file /private/cairn.key copy first --to https://backup --chunker fastcdc
cairn --repo /btrfs/cairn --key-file /private/cairn.key materialize first --chunker fastcdc
```

`capture --backend btrfs` requires a local repository and conflicts with `--live`.
The default snapshot parent is `<repo>/<domain>/backings`, on the source's Btrfs
filesystem. `--snapshot-dir DIR` selects an existing trusted parent on that
filesystem, allowing repository metadata to live on another filesystem. Keep the
repository outside the selected source. Existing capture filters, supported file
types and logical metadata restrictions still apply.

`copy` from a local Btrfs backing streams ordinary objects to a local or remote
destination. It requires the source domain's key when encrypted; `--domain` alone
cannot export the backing of an encrypted namespace. Copy's `--chunker` and
`--chunk-size` select the export representation only. Portable sources continue
using opaque replication and retain their existing recipes.

`materialize SNAPSHOT_OR_TAG` converts the backing within the same local repository.
It accepts existing chunking options, preserves the ID/tags, and releases the old
subvolume after committing the replacement. On an already portable snapshot it
verifies the representation and otherwise does nothing; it does not rechunk it.

## Representation and identity

Capture hashes every selected regular file and writes metadata pages only.
Nonempty files have `BtrfsFile` recipes containing paths relative to the selected
subtree. Empty files have no recipe, as before. Directory recipes and the logical
identity algorithm are unchanged. No content chunks are written during retained
capture, regardless of the requested chunker.

The version-2 snapshot record contains an absolute backing path, selected subtree,
filesystem UUID, subvolume UUID and numeric subvolume ID. An authenticated
`local: true` CRN2 header field marks this record and its Btrfs file-recipe objects.
Portable records remain version 1 and omit the backing and local marker.

UUIDs identify storage instances, not content. Readers open the subvolume without
following symlinks and check UUIDs, numeric ID and read-only status via ioctls on
that handle. File opens use the pinned subtree handle and Linux openat2 with
BENEATH, NO_SYMLINKS and NO_XDEV. Replaced/missing/writable backings fail explicitly.
Relocation of repository/backing or mount paths is not automatically repaired.

Restore validates whole-file hashes. Verify hashes all retained files, including
checking empty files, as well as validating metadata identities and references.
Export/materialize traverses the recorded manifest, so filtering, recorded modes
and symlink targets survive conversion. Every file's bytes must match its size and
digest before snapshot publication. Payload buffers and recipe pages are bounded;
the existing per-directory/index/visited-object metadata limits still apply.

Retained payload has the source filesystem's at-rest protection. A key file
encrypts Cairn metadata locally and exported/materialized objects before upload;
it does not encrypt the files inside the retained subvolume. Plaintext/null-key
namespaces export plaintext. Servers never receive encryption keys.

## Retention and concurrency

Before publication the subvolume is synchronized and renamed from its temporary
name to `.cairn-retained-*`. Parent/filesystem synchronization precedes publication;
the temporary marker is removed. `cleanup-snapshots` never sweeps retained names.
Keep these Cairn-owned subvolumes outside other snapshot managers' retention rules.
Btrfs has no general persistent administrator-release hold: read-only status does
not prevent privileged deletion or an owner making the subvolume writable.
Repository/backing parents must be trusted.

Local Btrfs readers hold a shared per-snapshot advisory lock for the whole operation.
Publication/materialization holds the corresponding exclusive lock. Persistent
`<domain>/backing-locks` files are never unlinked. Portable reads need neither a
lock nor write permission. The first complete representation still wins on capture.
Btrfs capture of an existing ID verifies the installed representation and discards
the new temporary snapshot. Object capture into an existing retained ID reports
that materialization is needed; it cannot silently replace that record.

Materialization writes and synchronizes objects, checks their reachable envelopes,
then atomically replaces and synchronizes the snapshot record. Only afterward does
it unlock/delete the old subvolume, checking its identity again. Active readers
delay conversion. No in-place working-directory update or hard-link reuse occurs.

## Failures and limits

Failure before replacement leaves the original backing installed; partial objects
remain unreferenced. A release failure after replacement explicitly reports that
the snapshot is materialized and prints the old backing path. Creation can work
without `user_subvol_rm_allowed`, while deletion cannot. Failed release may already
have cleared read-only status; the object-backed snapshot is still usable.

A crash after capture's rename but before publication, or after materialization's
replacement but before deletion, may leave an orphan retained subvolume. Retained
backing GC/journaling is not implemented. Inspection and explicit administrative
cleanup are required; retrying materialize does not discover/remove old orphans.
These synchronization paths have not been power-loss fault tested.

Capturing a selected directory retains the containing subvolume, including ignored
files and unselected siblings. Nested-subvolume capture remains refused. Shared
extents, filesystem failure domains and required privileges remain Btrfs properties.

## Protocol boundary

Ordinary store PUT and graph-completeness checks reject local markers. HTTP GET,
HEAD and PUT refuse these records; opaque replication rejects them before publishing
a destination. Listing may show a retained ID, but remote consumption requires
materialization or a key-holding local copy. Only local capture can publish a
Btrfs-backed record. Export writes portable recipes/version-1 records without
filesystem paths or local markers. Old binaries cannot read the local format.

## Validation

Ordinary tests cover metadata-only building, filters, paginated file/directory
recipes, both chunkers, plaintext/encrypted export, stable IDs, changed-content
rejection before publication, opaque-copy refusal, idempotent portable materialize
and CLI option conflicts.

The optional real Btrfs test uses disposable fixtures for UUID/path replacement
rejection, read-only enforcement, original-source isolation, local restore/verify,
HTTP rejection of local records, encrypted CLI export, CLI conversion and reader
locking. Without deletion permission it checks the explicit post-publication error
and cleans only its own fixture by emptying subvolumes; production never uses that
fallback. With deletion permission it also checks duplicate capture and release.

```sh
CAIRN_BTRFS_TEST_ROOT=/path/to/btrfs nix-shell --run 'cargo test --test retained_btrfs -- --ignored --nocapture'
```

Ioctl layouts follow the [Linux Btrfs UAPI](https://github.com/torvalds/linux/blob/master/include/uapi/linux/btrfs.h).
GET_SUBVOL_INFO exposes on-disk root flags; read-only checking uses GETFLAGS, whose
public bit assignments are different.

Validation on 2026-09-27: the ordinary `nix-shell --run 'cargo test'` suite and
`nix-shell --run 'cargo clippy --all-targets -- -D warnings'` passed, as did the
additional backing path/lock unit tests. The real Btrfs test passed on `/mnt/T5`
with plaintext and encrypted CLI capture/export/materialize. That mount lacks
`user_subvol_rm_allowed`, so it exercised explicit deletion-permission failure
after successful conversion; successful subvolume release and duplicate-capture
cleanup remain unexercised in this environment.
