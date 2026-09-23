# Cairn PoC contract and operations

Format version 1 is experimental; do not promise backward compatibility yet.
Linux/Unix implementation. See [the root AGENTS.md](../AGENTS.md) for setup and a quick start.

## Implemented scope

- Capture independent folder trees into a local or HTTP object store.
- Btrfs capture creates a temporary read-only snapshot of the containing subvolume,
  selects the requested subtree and reads directly from it. There is no local
  archive, object repository or payload spool on the HTTP client.
- `--live` explicitly bypasses Btrfs for quiescent ordinary directories. It detects
  some concurrent file modifications but cannot provide an atomic multi-file view.
- Regular files, empty directories, symlinks, UTF-8 names/targets and Unix rwx mode
  bits. Unsupported special files and nested Btrfs subvolumes/stubs fail capture.
  Hard links become independent files; timestamps, ownership, ACLs, xattrs and
  sparse allocation layout are not preserved. File byte content is preserved.
- Optional gitignore-style rules from `--ignore-file`; no implicit .gitignore walk.
- Local or remote listing, unique immutable tags, file-level diff, restore into a
  new directory or opt-in atomic swap, and diff against a source without ingesting it.
- Keyless opaque replication. With an HTTP(S) source and remote destination, the
  destination server pulls directly. The initiating client transfers a control
  request. Unix/SSH sources and local destinations use CLI-mediated replication.

## Identity and format

The file digest is BLAKE3 of raw file bytes. Directory digests encode sorted names
and logical child records (kind, rwx mode, size, content digest, symlink target),
with domain separation and length framing. Snapshot ID hashes the logical root
record with a snapshot domain prefix. Root permissions participate in identity.
Snapshot IDs ignore source paths, capture times, host identity, chunk boundaries,
encryption randomness, and tags. There is no ancestry or commit object.

Storage chunking defaults to fixed-size (1 MiB; configurable 4 KiB–4 MiB).
`--chunker fastcdc` selects streaming FastCDC 2020, normalization level 1, seed 0.
Its `--chunk-size` is a target: power of two from 4 KiB–1 MiB, with minimum target/4
and maximum target*4 (at most 4 MiB). It improves reuse after insertions/deletions;
fixed chunks remain useful for aligned overwrites and lower CPU costs. Whole-file digests are
computed incrementally alongside chunk upload. Identical chunks deduplicate across
unrelated snapshots in the same key domain; repeated chunks are not sent again.

Chunk objects contain `C` followed by raw bytes. Metadata objects contain `M`
followed by serialized JSON. File and directory recipes use pages of at most 128
entries, linked to their preceding page, allowing upload without retaining a whole
file's manifest. Snapshot records hold the logical root and its recipe reference.
The first complete representation of a snapshot ID wins if another capture uses
different chunk boundaries. Later extra objects may remain unreferenced.

Physical layout: `<repository>/<domain>/{objects,snapshots,tags}/<id>`.
Object IDs hash typed plaintext payloads (keyed in encrypted domains). Tags are
UTF-8 strings encoded as hex filenames, 1–120 bytes, unique per domain/repository.
A tag can be assigned idempotently to the same snapshot; moving/deleting tags is
not implemented. A hexadecimal tag matching a snapshot ID resolves as the ID first.
Multiple requested tags are separate atomic operations, not one transaction.

## Encryption and visibility

`keygen` writes a new 32-byte random key as 64 hex characters with mode 0600.
Omitting a key or supplying 32 zero bytes explicitly selects plaintext mode.
Keys are read from files; there is no password-based key derivation in this PoC.

BLAKE3 derive-key contexts separate encryption keys, object-ID keys and namespace
IDs. Encrypted objects use XChaCha20-Poly1305 with a fresh random 24-byte nonce.
New objects have a CRN2 envelope: magic, little-endian u32 header length, JSON
header, plaintext or ciphertext, then a 32-byte BLAKE3 checksum of all preceding
encoded bytes. The checksum lets keyless servers detect accidental storage damage
before reuse, publication or replication. It is not authentication against a
malicious server. The header holds nonce and opaque references and is authenticated
as associated data. Key-holding readers verify plaintext object IDs and compare
decoded references with the visible graph. Logical format/IDs remain version 1.
Readers still accept CRN1 archives, which lack the public storage checksum;
keyless integrity checks on those remain limited to envelope structure. Old Cairn
binaries cannot read newly written CRN2 objects. No automatic rewrite/migration.

Servers see namespace IDs, logical snapshot IDs, tag strings, object sizes, object
references and equality within a key domain. This is not metadata-hiding storage.
Exposed snapshot IDs can reveal known whole-tree matches. Servers never need the
E2E key. Different key domains do not deduplicate or support blind re-encryption.
The current format has not had an independent cryptographic/security review.

## Streaming, publication and recovery

Client payload buffers are bounded by maximum chunk size, with one transfer in flight.
FastCDC retains one max-sized read buffer plus a current chunk; files smaller
than its minimum bypass the larger CDC buffer. Encryption/transport have additional
bounded buffers. Short reads/EINTR preserve boundaries; real read errors abort.
There is no parallel uploader yet. Hashing never loads a whole file into memory.
Restore walks recipe pages backwards and writes chunks at their final offsets,
then rereads the destination sequentially to verify the whole-file hash. It does
not use a temporary full-file payload copy. A decreasing coverage boundary checks
for gaps, overlaps, cycles and truncated recipes without retaining every page ID.

Metadata memory is separate: capture sorts one directory at a time, restore loads
one directory's entries, diff builds an in-memory path index, and replication keeps
visited object IDs. Thus total metadata memory is not constant with tree/object
count. Metadata pagination avoids whole-file recipe buffering. Format object size
limit is 5 MiB, header 64 KiB, maximum depth 256. Directory restore caps entries at
one million; replication caps visited objects at ten million. These are defensive
limits, not a claim of supporting trees that large efficiently. Listing is not yet
paginated. Server allows four ordinary requests and one import in progress, using
separate admission so reciprocal transfers do not consume their own GET slots.
It is a basic PoC service.

Local writes use a same-directory temporary file, fsync, atomic create-if-absent,
and directory fsync. Objects precede snapshot publication. Local and HTTP snapshot
publication check all reachable envelopes. An existing snapshot may use a different
chunk recipe; that installed recipe must also pass validation before retry reports
success. Deduplication presence checks validate stored envelopes (extra disk reads,
no download to the HTTP client). Import publishes only after the graph is copied.
Servers cannot validate encrypted plaintext hashes; clients authenticate and
hash-check objects when reading/restoring them. ENOSPC/quota errors map to HTTP
507, other filesystem errors to 500.

Interrupted capture/copy leaves unreferenced objects, which a retry reuses. There is
no GC, deletion, resumable session journal or quota system yet. Default new-mode
restore may leave a partial destination on failure and refuses an existing
destination on a new invocation. Opt-in `restore --mode swap` stages and verifies
a complete replacement, then atomically exchanges it with an existing directory
and retains the old tree. Swap uses hash-checked local copies by default
(`--reuse never` disables reuse), requires Linux openat2 and atomic exchange support,
and leaves staging on failures. No in-place merge or automatic rollback is provided.
See the [restore contract](restore/AGENTS.md).
Power-loss guarantees have not been tested with fault injection.

`verify SNAPSHOT_OR_TAG` checks snapshot/directory identities, authenticates and
hash-checks all reachable objects, and validates metadata page references, without
writing a restored tree. It requires the E2E key for an encrypted repository. It
does not reconstruct whole files to check their logical digest/coverage; restore
performs those additional checks. Corruption fails explicitly and is never silently
overwritten. To repair, preserve/quarantine the identified damaged object outside
the repository, then retry capture from intact original files (with the same
chunking) or copy from an intact replica. Missing objects can be replenished by
retry; an existing snapshot with another recipe needs its own missing objects.
Already-published snapshots are not hidden from listings if data later goes bad.

Btrfs snapshot operations require permissions granted by the filesystem/kernel.
Unprivileged capture needs ownership of the containing subvolume (ownership of
the selected directory alone is insufficient), a writable snapshot destination,
and `user_subvol_rm_allowed` for deletion. The temporary snapshot remains read-only
through capture; cleanup clears its read-only property immediately before deleting
it, because unprivileged deletion otherwise fails with `Read-only file system`.
The adapter does not invoke sudo, change mounts, stop processes or freeze a whole
filesystem. Use `--snapshot-dir` for an existing suitable directory on the same
filesystem. Capture-owned snapshots are deleted on normal success/error. SIGKILL
or power loss can leave `.cairn-capture-*` snapshots; stderr reports their paths.
New captures create an adjacent `.lock` intent record and hold an advisory file
lock throughout capture. `cleanup-snapshots --snapshot-dir DIR` removes abandoned
marked snapshots and skips active locks. It only operates in the explicitly named
directory; unrelated/unmarked subvolumes are untouched. Invalid markers fail
closed and need inspection. Old snapshots without records require manual cleanup.
Use a trusted snapshot directory. A hard kill during the external Btrfs creation
command itself can race that utility; wait for it to finish before recovery.
Snapshots retain changed extents and consume metadata space: no payload spool does
not imply zero local disk usage. Ordinary file digests cannot be recovered from
Btrfs block checksums; no unchanged-extent hash cache is implemented yet.

## Commands

Enter `nix-shell`, then `cargo build`; binary is `target/debug/cairn`.
All examples use explicit paths to keep repository data outside the source tree.

```sh
cairn keygen /path/to/cairn.key
cairn --key-file /path/to/cairn.key domain
cairn --repo /path/to/store --key-file /path/to/cairn.key capture /path/to/source --tag initial
cairn --repo /path/to/store --key-file /path/to/cairn.key list
cairn --repo /path/to/store --key-file /path/to/cairn.key restore initial /path/to/new-destination
cairn --repo /path/to/store --key-file /path/to/cairn.key verify initial
cairn cleanup-snapshots --snapshot-dir /path/to/temporary-snapshots
cairn --repo /path/to/store --key-file /path/to/cairn.key diff initial next
cairn --repo /path/to/store --key-file /path/to/cairn.key diff-source initial /path/to/source
```

Use `capture ... --live` and `diff-source ... --live` for an ordinary quiescent
directory (including an already read-only Btrfs snapshot) without creating another
Btrfs snapshot. `--ignore-file PATH` reads explicit gitignore-style rules. A local
repository nested in the selected source is rejected by the CLI.

Direct reads work without Btrfs tools or snapshot privileges, with either local
or HTTP storage. `--live` conflicts with `--snapshot-dir`. It is explicit: a failed
Btrfs snapshot never silently falls back to reading a changing source. File-level
size/mtime checks catch some concurrent writes, but do not guarantee a consistent
directory tree. Normal read/traversal permissions and the format's supported file
types still apply; nested filesystems require separate captures.

```sh
cairn --repo /path/to/store capture /path/to/folder --live --tag initial
cairn --repo /path/to/store diff-source initial /path/to/folder --live
cairn --repo https://server --key-file /path/to/key capture /path/to/folder --live
cairn --repo https://server --key-file /path/to/key capture /path/to/folder --live --chunker fastcdc
```

```sh
# Each server: CAIRN_TOKEN is its own access token, unrelated to encryption.
cairn --repo /srv/cairn serve --listen 127.0.0.1:7443
# Client: CAIRN_TOKEN authenticates source, CAIRN_DEST_TOKEN destination.
cairn --repo https://server-a --key-file /path/to/cairn.key capture /path/to/source --tag initial
cairn --repo https://server-a copy initial --to https://server-b --domain DOMAIN_ID
cairn --repo https://server-b --key-file /path/to/cairn.key restore SNAPSHOT_ID /path/to/new-destination
```

Copy does not copy tags implicitly; use `--tag NAME` to assign a destination tag
and fail on a collision. Domain ID comes from `domain` on a key-holding client;
the copy command itself needs no key file. Local repositories copy the same way.

The server offers HTTP over TCP or a Unix domain socket; use HTTPS termination or
an SSH tunnel for remote transport. Non-loopback TCP
binding requires an access token of at least 32 characters. Loopback without a token
is intended for local development. One token grants full service access, including
outbound HTTP imports: this is not a public/multi-tenant or untrusted-user service.
The import source must be reachable from the destination server. Client timeout is
one hour; retry imports after interruption. HTTP redirects are disabled.

Unix socket transport uses the same HTTP API, auth and object format:

```sh
# Use an existing private directory (e.g. XDG_RUNTIME_DIR).
cairn --repo /path/to/store serve --unix-socket /run/user/1000/cairn.sock
cairn --repo unix:///run/user/1000/cairn.sock capture /path/to/source --live
```

`--unix-socket` and `--listen` conflict; omitting both retains TCP loopback port
7443. Socket mode is set to 0600; use a trusted private parent directory to protect
the interval between binding and setting permissions. CAIRN_TOKEN is optional on
Unix sockets and, when set, is checked just as on TCP. Unix clients disable HTTP
proxies and take a literal absolute path after `unix://` (no URL percent decoding).
An adjacent `.lock` file is held for the listener lifetime and deliberately remains
on disk (deleting lock files can race other processes). SIGINT/SIGTERM gracefully
drain requests and remove the socket. Startup under the lock recovers a stale socket
only after connection refusal. Active listeners, symlinks, regular files, and
uncertain connection errors are left untouched. Cleanup checks device/inode identity
and does not delete a replacement path. SIGKILL can leave a socket; restart recovers it.

Copies from a Unix source are relayed by the CLI, because the path belongs to the
client host. HTTP(S) sources can still be pulled directly by a destination reached
over either transport. The server import API accepts only HTTP(S) source URLs.
Unix-to-Unix and local/Unix copies use the ordinary opaque replication path.
SSH repositories use `ssh://[user@]host[:port]/absolute/socket`, for example:

```sh
cairn --repo ssh://user@host/run/user/1000/cairn.sock --key-file /path/to/key capture /path/to/source --live
```

`ssh://` uses embedded russh 0.63.3, with HTTP directly over a
`direct-streamlocal@openssh.com` channel. No subprocess or local socket exists.
The SSH session is reused across requests; each request currently opens its own
channel/HTTP connection. Responses are bounded by MAX_OBJECT, requests time out
after one hour, and connection/authentication after 120 seconds. Session/runtime
lifetime is shared across cloned stores; process death closes its connections.
Native SSH needs a literal hostname/IP (no OpenSSH config parsing), defaults to
port 22 and USER, and supports:

- `CAIRN_SSH_IDENTITY`: explicit private key path. Optional
  `CAIRN_SSH_KEY_PASSPHRASE` unlocks it; neither is the snapshot encryption key.
- Without an explicit identity: plain public keys from SSH_AUTH_SOCK, then standard
  id_ed25519/id_ecdsa/id_rsa files. Encrypted default files require an agent or an
  explicit identity/passphrase. Password/interactive/certificate auth is not implemented.
- `CAIRN_SSH_KNOWN_HOSTS`: defaults to ~/.ssh/known_hosts. Unknown or changed keys
  fail; there is no automatic acceptance or modification of known_hosts. Provision
  verified entries separately. Host certificates and files with @ markers are
  rejected rather than ignoring their semantics; use OpenSSH for those setups.

`ssh-openssh://` launches installed OpenSSH with a Unix-to-Unix forward in a private
temporary directory. Host aliases, identities, ProxyJump and host verification use
normal SSH configuration. User/port in the address override config when supplied.
SSH diagnostics and terminal authentication prompts remain visible; setup times
out after 120 seconds. No server-side shell command or Cairn launch is performed:
the remote server must already listen on that socket, accessible to the SSH user.
Paths are literal; whitespace, percent escapes, colons/brackets in socket paths,
queries and fragments are rejected. No tilde or environment-variable expansion.
SSH child lifetime is shared across cloned stores; normal success/error drops kill
and reap it and remove the temporary directory. Multiplexing/backgrounding is
disabled so Cairn owns its tunnel; agent/X11 forwarding and local commands are off.
A hard kill of the OpenSSH-backend client can still leave its SSH child/temp directory; server
stale-socket recovery does not clean client tunnel directories.
SSH sources, like Unix sources, copy through the CLI rather than asking the
destination server to interpret client-side SSH configuration. An HTTP source can
be imported by a destination reached over SSH. HTTP import rejects SSH source URLs.

Unix transport tests exercise encrypted CLI capture/verify/restore, authentication,
socket permissions, keyless CLI relay with retries, HTTP-source server import via a
Unix destination, existing-path preservation and conflicting listener options.
Socket lifecycle tests cover competing servers, SIGKILL/restart, SIGTERM cleanup,
foreign listeners, symlinks and replacement files. SSH parser unit tests cover
aliases/user/port/IPv6 and rejected inputs. Explicit `cargo test --test ssh --
--ignored --nocapture` runs a temporary loopback sshd with generated keys and pinned
host verification and encrypted FastCDC capture/verify/restore/copy on both backends.
Native clients run with an empty PATH, including agent authentication and HTTP-source
import controlled via SSH. OpenSSH tests check child reaping on success, application
errors and rejected host keys. Native tests reject unknown keys too.
It does not read user credentials or connect to external hosts.
Windows portability is low priority and is not part of this implementation.

## Future blocks

Opt-in staged swap restore with hash/never reuse is implemented; dirty-tree update,
delete-extra and quick reuse remain specified in the [restore design](restore/AGENTS.md).
Timestamp-based reuse would be an explicit heuristic, not proof of content identity.

Read-only FUSE can resolve immutable manifests lazily with bounded chunk caching.
A writable filesystem can add mutable working state and explicit durable checkpoint
semantics above the store. Neither belongs inside the snapshot-store protocol.
Btrfs unchanged-extent reuse, streaming metadata diff,
range reads, GC, richer metadata and protocol hardening remain separate work.
Chunking research, pinned primary sources, reproducible benchmarks and the decision
to offer FastCDC without changing the default: [chunking/AGENTS.md](chunking/AGENTS.md).
Automatic per-file policies, compression and header-aware splitting need further
workload evidence. Preserve logical identity and bounded memory in future changes.

## Review and validation on 2026-09-22

The ordinary suite passed with ten unit and twenty-six integration tests, plus
the separately enabled SSH and bind-mount restore tests. Clippy with all targets
and warnings denied passed. Staged restore tests cover hash/never reuse, old-tree
retention, modes/type conflicts, symlinks and independent hard links, interrupted
download/retry, and a 128 MiB file under a 96 MiB process address-space limit.
The ordinary restore tests also passed with disposable fixtures on Btrfs.
The Btrfs integration test was not rerun in this review: the available Btrfs mounts
lack `user_subvol_rm_allowed`. Unsupported-exchange and power-loss fault injection
remain untested.

Shared logical-node validation now rejects unsupported kinds/permissions and
inconsistent file/directory/symlink fields in verify, diff and restore. Verify no
longer fetches each metadata object twice within its graph traversal. Live capture's
Btrfs detection uses fstatfs rather than depending on an external stat executable.
Restore builds directories privately with owner access even under restrictive umask,
and new-directory restore and repository-directory creation synchronize the current
directory when the destination uses a single relative path component.

## Validation on 2026-09-21

`cargo test`: nine unit tests and twenty-two integration tests passed (plus the
separately enabled Btrfs integration test below). Coverage includes:
- identical snapshot IDs across different chunk sizes and encryption modes;
- fixed/FastCDC identity equivalence, golden chunk boundaries, short-read/EINTR
  stability, bounds and read errors; prefix insertion reuses most encrypted
  chunks; killed upload/server recovery exercised with both chunkers;
- multi-page file/directory recipes, symlinks, empty files/directories, mode restore;
- deduplication across unrelated snapshots and zero new object bytes on repeat copy;
- atomic tag collision handling, wrong keys, modified ciphertext/reference headers;
- missing objects preventing replica publication and source diff without ingestion;
- two authenticated HTTP servers, encrypted capture, destination-initiated keyless
  import and verified restore, plus rejection of incomplete HTTP snapshot graphs;
- direct CLI capture on ext4 with an empty executable search path, plaintext and
  encrypted restore, filters, tags, symlinks/modes, and diff without ingestion;
- encrypted `--live` CLI upload to HTTP with no client repository, and rejection
  of `--live` combined with `--snapshot-dir` or a repository inside the source;
- actual SIGKILL during upload and destination-server import, at deterministic
  HTTP boundaries: no premature snapshot/tag publication, byte-identical reuse
  of previously written encrypted objects, restart/retry and verified restore;
- test-only simulated ENOSPC after a partial object write: temporary file removed,
  no snapshot publication, retry reuses complete objects; HTTP status mapping for
  disk-full/quota/I/O errors. This does not fill the user's filesystem or test
  hardware power loss, filesystem allocation exhaustion, or kernel fsync failure;
- missing/corrupt objects fail verification, restore, capture reuse and keyless
  copy; source re-capture repairs missing objects; different installed recipes
  cannot falsely report successful repair; CRN1 read compatibility and truncated
  CRN2 rejection; cleanup refuses invalid markers and ignores unmarked paths;
- encrypted fixed and FastCDC capture/restore of a 128 MiB file with each client process limited
  to 96 MiB of virtual address space. This is a bounded-payload regression check,
  not a throughput benchmark or a guarantee for arbitrary metadata counts.

`cargo clippy --all-targets -- -D warnings` passed; own source formatted with rustfmt.
The HTTP integration test was rerun after separating import admission from reads.

Real Btrfs integration passed without sudo on a filesystem mounted with
`user_subvol_rm_allowed`. The opt-in test covers:
- read-only isolation while the original file changes;
- selected-folder capture, filtering and encrypted restore;
- diff without ingestion, default and explicit snapshot destinations (including
  a destination in a different subvolume on the same filesystem);
- CLI capture directly to an HTTP server, encrypted restore and CLI source diff;
- nested-subvolume/stub rejection and cleanup after success and capture errors;
- actual client SIGKILL with a snapshot active: cleanup skips it while locked,
  removes it after process death, leaves original files intact and is idempotent;
- rejection when the containing subvolume is root-owned, despite the selected
  ordinary directory being writable, followed by successful direct reading with
  `--live`. Successful snapshot creation for the root-owned
  top-level subvolume was not tested; that requires additional privileges.

Repeat with `CAIRN_BTRFS_TEST_ROOT=/path/to/writable/btrfs/test-directory
nix-shell --run 'cargo test --test btrfs -- --ignored --nocapture'` (on one shell
line). The test creates only an isolated temporary directory and tracks its own
subvolumes for deletion. It is ignored by default so ordinary CI does not require
Btrfs. Test fixtures are cleaned up after successful runs.
