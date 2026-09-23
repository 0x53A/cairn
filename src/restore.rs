//! Restore into a new tree, optionally exchanging it with a quiescent destination.
use crate::{
    objects::Keys,
    snapshot::{
        MAX_CHUNK, MAX_DEPTH, Node, Page, directory_entries, get_object, get_page, load_snapshot,
        validate_node,
    },
    store::Store,
};
use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use serde::Serialize;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum RestoreMode {
    #[default]
    New,
    /// Build a verified sibling tree, atomically exchange it, and retain the old tree.
    Swap,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Reuse {
    Never,
    /// Copy matching local regular files and verify the copied bytes with BLAKE3.
    #[default]
    Hash,
}

/// Counts refer to regular files, except the explicit directory/symlink fields.
#[derive(Debug, Default, Serialize)]
pub struct RestoreStats {
    pub downloaded: u64,
    pub hash_reused: u64,
    pub directories_created: u64,
    pub symlinks_created: u64,
    pub displaced_tree: Option<PathBuf>,
}

pub fn run(
    store: &Store,
    keys: &Keys,
    id: &str,
    destination: &Path,
    mode: RestoreMode,
    reuse: Option<Reuse>,
) -> Result<RestoreStats> {
    ensure!(
        mode != RestoreMode::New || reuse.is_none(),
        "--reuse requires --mode swap"
    );
    let snapshot = load_snapshot(store, keys, id)?;
    let mut writer = Writer {
        store,
        keys,
        old: None,
        stats: RestoreStats::default(),
    };
    match mode {
        RestoreMode::New => {
            match fs::symlink_metadata(destination) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
                Ok(_) => bail!("destination must not exist"),
            }
            writer.node(&snapshot.root, destination, Path::new(""), 0)?;
            File::open(parent(destination))?.sync_all()?;
        }
        RestoreMode::Swap => swap(
            &mut writer,
            &snapshot.root,
            destination,
            reuse.unwrap_or_default(),
        )?,
    }
    Ok(writer.stats)
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

#[cfg(target_os = "linux")]
fn swap(writer: &mut Writer<'_>, root: &Node, destination: &Path, reuse: Reuse) -> Result<()> {
    use rustix::fs::{Mode, OFlags, RenameFlags, ResolveFlags, openat2, renameat_with};
    let destination: PathBuf = destination.components().collect();
    // Resolve the parent once; reject a root/current/parent-directory operand.
    // The final component must be a real directory, never a symlink.
    ensure!(
        destination
            .components()
            .next_back()
            .is_some_and(|c| matches!(c, std::path::Component::Normal(_))),
        "swap requires a named destination directory"
    );
    let name = destination
        .file_name()
        .context("swap requires a named destination directory")?;
    let parent_path = fs::canonicalize(parent(&destination))?;
    let destination = parent_path.join(name);
    let parent_fd = File::open(&parent_path)?;
    let old = File::from(
        openat2(
            &parent_fd,
            name,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        )
        .context(
            "swap requires an existing directory on the parent's mount (Linux openat2 required)",
        )?,
    );
    let identity = old.metadata()?;
    ensure!(
        identity.dev() == parent_fd.metadata()?.dev(),
        "swap refuses a nested filesystem or Btrfs subvolume"
    );
    if reuse == Reuse::Hash {
        writer.old = Some(old);
    }
    let staging = tempfile::Builder::new()
        .prefix(".cairn-restore-")
        .tempdir_in(&parent_path)?;
    fs::set_permissions(staging.path(), fs::Permissions::from_mode(0o700))?;
    // Persist ownership before publication: no destructor may recursively remove
    // the old destination after exchange. Failures intentionally retain staging.
    let staging = staging.keep();
    eprintln!("restore staging directory: {}", staging.display());
    let staged_tree = staging.join("tree");
    let mut build = || -> Result<()> {
        writer.node(root, &staged_tree, Path::new(""), 0)?;
        let stage_fd = File::open(&staging)?;
        stage_fd.sync_all()?;
        parent_fd.sync_all()?;
        let current = fs::symlink_metadata(&destination)?;
        ensure!(
            current.is_dir() && (current.dev(), current.ino()) == (identity.dev(), identity.ino()),
            "destination changed during staged restore"
        );
        renameat_with(&stage_fd, "tree", &parent_fd, name, RenameFlags::EXCHANGE)
            .context("atomic directory exchange failed; no two-rename fallback is used")?;
        writer.stats.displaced_tree = Some(staged_tree.clone());
        // Both directories changed. If flushing fails, publication has already
        // happened; make that explicit instead of claiming the old tree is live.
        eprintln!("displaced tree retained at: {}", staged_tree.display());
        stage_fd
            .sync_all()
            .and_then(|()| parent_fd.sync_all())
            .context("swap was published, but directory synchronization failed")?;
        Ok(())
    };
    build().with_context(|| format!("restore staging retained at {}", staging.display()))
}

#[cfg(not(target_os = "linux"))]
fn swap(_: &mut Writer<'_>, _: &Node, _: &Path, _: Reuse) -> Result<()> {
    bail!("swap restore currently requires Linux atomic exchange and openat2")
}

struct Writer<'a> {
    store: &'a Store,
    keys: &'a Keys,
    old: Option<File>,
    stats: RestoreStats,
}

impl Writer<'_> {
    fn node(&mut self, node: &Node, path: &Path, relative: &Path, depth: usize) -> Result<()> {
        ensure!(depth <= MAX_DEPTH, "directory nesting limit exceeded");
        validate_node(node)?;
        match node.logical.kind.as_str() {
            "directory" => {
                let entries = directory_entries(self.store, self.keys, node)?;
                // No intermediate payload is exposed to other users, and even
                // an umask denying owner access cannot block tree construction.
                fs::DirBuilder::new().mode(0o700).create(path)?;
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
                for e in entries {
                    self.node(
                        &e.node,
                        &path.join(&e.name),
                        &relative.join(&e.name),
                        depth + 1,
                    )?;
                }
                let dir = File::open(path)?;
                dir.set_permissions(fs::Permissions::from_mode(node.logical.mode))?;
                dir.sync_all()?;
                self.stats.directories_created += 1;
            }
            "file" => {
                let mut file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(path)?;
                let reused = self.reuse(node, relative, &mut file)?;
                if !reused {
                    file.set_len(0)?;
                    restore_file(self.store, self.keys, node, &mut file)?;
                }
                file.set_permissions(fs::Permissions::from_mode(node.logical.mode))?;
                file.sync_all()?;
                if reused {
                    self.stats.hash_reused += 1;
                } else {
                    self.stats.downloaded += 1;
                }
            }
            "symlink" => {
                symlink(
                    node.logical
                        .target
                        .as_ref()
                        .context("missing symlink target")?,
                    path,
                )?;
                self.stats.symlinks_created += 1;
            }
            _ => bail!("unsupported entry kind"),
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn reuse(&self, node: &Node, relative: &Path, output: &mut File) -> Result<bool> {
        use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
        let Some(root) = &self.old else {
            return Ok(false);
        };
        let input = match openat2(
            root,
            relative,
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        ) {
            Ok(fd) => fd,
            // Missing/type-conflicting/unreadable local entries simply download.
            Err(
                rustix::io::Errno::NOENT
                | rustix::io::Errno::NOTDIR
                | rustix::io::Errno::LOOP
                | rustix::io::Errno::ACCESS,
            ) => return Ok(false),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "open reuse candidate {} without crossing mounts or symlinks",
                        relative.display()
                    )
                });
            }
        };
        let input = File::from(input);
        let meta = input.metadata()?;
        // Btrfs subvolumes can have a distinct st_dev without a mount transition
        // that RESOLVE_NO_XDEV would catch.
        ensure!(
            meta.dev() == root.metadata()?.dev(),
            "reuse refuses a nested filesystem or Btrfs subvolume: {}",
            relative.display()
        );
        if !meta.is_file() || meta.len() != node.logical.size {
            return Ok(false);
        }
        let mut input = match openat2(
            root,
            relative,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        ) {
            Ok(fd) => File::from(fd),
            Err(rustix::io::Errno::ACCESS) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let opened = input.metadata()?;
        ensure!(
            opened.is_file() && (opened.dev(), opened.ino()) == (meta.dev(), meta.ino()),
            "reuse candidate changed during restore"
        );
        // Copy and hash in one bounded pass. Hash the destination as well, just
        // as downloaded files are verified; never share an inode with old data.
        let (size, digest) = copy_hash(&mut input, output, node.logical.size)?;
        if size != node.logical.size || digest != node.logical.digest {
            return Ok(false);
        }
        verify_file(output, node)?;
        Ok(true)
    }

    #[cfg(not(target_os = "linux"))]
    fn reuse(&self, _: &Node, _: &Path, _: &mut File) -> Result<bool> {
        Ok(false)
    }
}

fn copy_hash(input: &mut File, output: &mut File, size: u64) -> Result<(u64, String)> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0; 64 * 1024];
    let mut copied = 0;
    // An unexpectedly growing source cannot keep a restore alive indefinitely.
    let mut input = input.take(size.saturating_add(1));
    loop {
        let n = read_retry(&mut input, &mut buffer)?;
        if n == 0 {
            break;
        }
        output.write_all(&buffer[..n])?;
        hasher.update(&buffer[..n]);
        copied += n as u64;
    }
    Ok((copied, hasher.finalize().to_hex().to_string()))
}

fn read_retry(reader: &mut impl Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    loop {
        match reader.read(buffer) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn verify_file(file: &mut File, node: &Node) -> Result<()> {
    ensure!(
        file.metadata()?.len() == node.logical.size,
        "restored file size mismatch"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let n = read_retry(file, &mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    ensure!(
        hasher.finalize().to_hex().as_str() == node.logical.digest,
        "restored file hash mismatch"
    );
    Ok(())
}

fn restore_file(store: &Store, keys: &Keys, node: &Node, file: &mut File) -> Result<()> {
    let mut end = node.logical.size;
    let mut next = node.recipe.clone();
    let mut pages = 0usize;
    while let Some(id) = next {
        pages += 1;
        ensure!(pages <= 1_000_000, "invalid file recipe chain");
        let Page::File { previous, chunks } = get_page(store, keys, &id)? else {
            bail!("not a file page");
        };
        for c in chunks.iter().rev() {
            ensure!(
                c.len > 0 && c.len <= MAX_CHUNK && c.offset.checked_add(c.len as u64) == Some(end),
                "invalid chunk coverage"
            );
            let (bytes, refs) = get_object(store, keys, &c.id)?;
            ensure!(
                refs.is_empty() && bytes.first() == Some(&b'C') && bytes.len() - 1 == c.len,
                "invalid chunk object"
            );
            file.seek(SeekFrom::Start(c.offset))?;
            file.write_all(&bytes[1..])?;
            end = c.offset;
        }
        next = previous;
    }
    ensure!(end == 0, "incomplete file recipe");
    verify_file(file, node)
}
