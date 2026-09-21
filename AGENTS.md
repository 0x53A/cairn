# Cairn

Rust snapshot-store PoC. Setup and quick-start commands are below.
Read [design/AGENTS.md](design/AGENTS.md) for the storage contract and current limitations.

Use `nix-shell --run 'cargo test'` and `nix-shell --run 'cargo clippy --all-targets -- -D warnings'`.
Do not stage, commit, create branches or publish without explicit instruction.
Do not edit human README files. AGENTS.md files are agent-maintained design notes.

## Scope

Independent immutable snapshots, optional E2E encryption (null means plaintext),
unique string tags, local/HTTP stores (TCP, Unix socket, or client-managed SSH
forward to a remote Unix socket), restore to a new directory, comparison,
server-initiated replication of missing objects without client relay or E2E keys.
`ssh://` embeds russh; `ssh-openssh://` uses the installed SSH client. Native SSH
does not parse OpenSSH config; see design notes for identity/known_hosts options.
Use bounded payload buffers. Snapshot identity describes logical files and paths,
not storage chunks, encryption randomness or tags. Btrfs capture must stream from
a temporary read-only subvolume snapshot without a local payload spool.
Explicit `--live` capture/diff reads files directly on ordinary filesystems,
without Btrfs tools or privileges. It streams the same format but does not provide
an atomic directory view; keep the source quiescent for a consistent capture.
Fixed chunking remains default. `--chunker fastcdc` is an explicit streaming CDC
option. Its target is a power of two, 4 KiB–1 MiB; maximum chunk size is 4x target.
See [chunking research](design/chunking/AGENTS.md) before changing boundary rules,
defaults, file-type policies or dependencies. Golden fixtures protect cut points.

Relationships between snapshots and mutation of existing working directories are
out of scope. Later consumers: read-only FUSE with lazy chunk retrieval and bounded
optional caching; a writable filesystem that publishes snapshots at defined sync
points.

## Development discipline

No homemade cryptographic primitives. Distinguish encryption from authorization.
Authenticated encryption must bind visible object references. Store servers never
need snapshot encryption keys. Do not claim Btrfs block checksums are file hashes.
Unknown file contents require hashing; no timestamp-only correctness shortcut.
Never treat failed or partial upload as a committed snapshot. Report limitations
explicitly, especially durability, resource limits and filesystem metadata support.

## Build and quick start

This is an experimental Linux/Unix proof of concept, with an experimental storage
format. Use disposable data or keep another backup. Windows support is not implemented.

Use a recent stable Rust toolchain (tested with Rust 1.95):

```sh
cargo install --path . --locked
```

Alternatively, `cargo build --release --locked` produces `target/release/cairn`.
The examples below assume `cairn` is on PATH. On NixOS, run these commands inside
`nix-shell`; the included `.envrc` also supports direnv.

Btrfs capture requires `btrfs-progs` and snapshot creation/deletion permissions;
`--live` capture needs neither. Embedded SSH needs no `ssh` executable; the
`ssh-openssh://` backend requires OpenSSH. The explicit SSH integration test also
requires OpenSSH tools, even when testing the embedded client:

```sh
cargo test --test ssh -- --ignored --nocapture
```

Run this quick start from a scratch directory containing a `source` folder. Keep
the repository, encryption key and restore destination outside that folder, and
keep the source unchanged during live capture:

```sh
cairn keygen ./cairn.key
cairn --repo ./store --key-file ./cairn.key capture ./source --live --tag first
cairn --repo ./store --key-file ./cairn.key list
cairn --repo ./store --key-file ./cairn.key verify first
cairn --repo ./store --key-file ./cairn.key restore first ./restored
```

Capture prints the snapshot ID; commands accept an ID or a tag. Restore requires
an absent destination. Keep the encryption key private and backed up: the server
cannot recover encrypted snapshots without it. Omit `--key-file` for plaintext;
an all-zero key also selects plaintext. Server access tokens are separate from
encryption keys.

After changing the source:

```sh
cairn --repo ./store --key-file ./cairn.key capture ./source --live --tag second
cairn --repo ./store --key-file ./cairn.key diff first second
cairn --repo ./store --key-file ./cairn.key diff-source second ./source --live
```

`diff-source` hashes the source without storing another snapshot. Transport setup,
copy commands, capture options, recovery, supported metadata and the opt-in Btrfs
test are documented in [design/AGENTS.md](design/AGENTS.md).
