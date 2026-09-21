//! Direct directory reads or temporary Btrfs capture. Neither freezes applications.
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Command,
};

pub struct Source {
    pub path: PathBuf,
    temporary: Option<PathBuf>,
    marker: Option<PathBuf>,
    lock: Option<File>,
}
const MARKER: &[u8] = b"cairn temporary Btrfs snapshot v1\n";
impl Source {
    pub fn open(source: &Path, live: bool, snapshot_dir: Option<&Path>) -> Result<Self> {
        ensure!(
            !live || snapshot_dir.is_none(),
            "--live cannot be combined with --snapshot-dir"
        );
        let source = fs::canonicalize(source).context("resolve source directory")?;
        ensure!(source.is_dir(), "source must be a directory");
        if live {
            return Ok(Self {
                path: source,
                temporary: None,
                marker: None,
                lock: None,
            });
        }
        let fs_type = Command::new("stat")
            .args(["-f", "-c", "%T"])
            .arg(&source)
            .output()?;
        ensure!(
            fs_type.status.success() && String::from_utf8_lossy(&fs_type.stdout).trim() == "btrfs",
            "source is not Btrfs; use --live explicitly for a quiescent ordinary directory"
        );
        let mut subvolume = source.clone();
        let device = source.metadata()?.dev();
        loop {
            // Btrfs subvolume roots have inode 256. Unlike `subvolume show`,
            // this does not require privileged B-tree search access.
            let meta = subvolume.metadata()?;
            ensure!(
                meta.dev() == device,
                "cannot find a subvolume root within this mount"
            );
            if meta.ino() == 256 {
                break;
            }
            if !subvolume.pop() {
                bail!("cannot locate containing Btrfs subvolume; check access permissions");
            }
        }
        let relative = source.strip_prefix(&subvolume)?.to_owned();
        // st_dev may differ between subvolumes on the SAME Btrfs filesystem.
        // Let the snapshot ioctl validate filesystem identity for the destination.
        let default_parent = subvolume
            .parent()
            .filter(|p| {
                Command::new("stat")
                    .args(["-f", "-c", "%T"])
                    .arg(p)
                    .output()
                    .is_ok_and(|o| {
                        o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "btrfs"
                    })
            })
            .unwrap_or(&subvolume);
        let parent = snapshot_dir.unwrap_or(default_parent);
        let parent = fs::canonicalize(parent).context("snapshot directory must already exist")?;
        let name = format!(".cairn-capture-{}", hex::encode(rand::random::<[u8; 16]>()));
        let temporary = parent.join(name);
        let marker = temporary.with_extension("lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&marker)?;
        lock.try_lock()?;
        // Persist the intent before starting creation. After creation, recovery
        // uses the advisory lock rather than guessing liveness from a PID.
        let mut result = Self {
            path: temporary.join(relative),
            temporary: Some(temporary.clone()),
            marker: Some(marker),
            lock: Some(lock),
        };
        let lock = result.lock.as_mut().unwrap();
        lock.write_all(MARKER)?;
        lock.sync_all()?;
        File::open(&parent)?.sync_all()?;
        let output = Command::new("btrfs")
            .args(["subvolume", "snapshot", "-r"])
            .arg(&subvolume)
            .arg(&temporary)
            .output()?;
        ensure!(
            output.status.success(),
            "Btrfs snapshot failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        eprintln!("temporary read-only snapshot: {}", temporary.display());
        Ok(result)
    }
    pub fn finish(mut self) -> Result<()> {
        self.cleanup()
    }
    fn cleanup(&mut self) -> Result<()> {
        if let Some(path) = &self.temporary
            && path.try_exists()?
        {
            // Unprivileged deletion checks write access to the subvolume itself.
            // Keep it read-only throughout capture, then unlock only our disposable
            // snapshot immediately before deletion (never the original source).
            let output = Command::new("btrfs")
                .args(["property", "set", "-ts"])
                .arg(path)
                .args(["ro", "false"])
                .output()?;
            ensure!(
                output.status.success(),
                "unlock temporary snapshot {} for deletion: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr)
            );
            let output = Command::new("btrfs")
                .args(["subvolume", "delete"])
                .arg(path)
                .output()?;
            ensure!(
                output.status.success(),
                "remove temporary snapshot {}: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        self.temporary = None;
        if let Some(marker) = &self.marker {
            fs::remove_file(marker)?;
            File::open(marker.parent().unwrap())?.sync_all()?;
            self.marker = None;
        }
        self.lock = None;
        Ok(())
    }
}

#[derive(Default)]
pub struct CleanupStats {
    pub removed: usize,
    pub active: usize,
}

/// Explicit recovery only: inspect Cairn intent records, never sweep arbitrary
/// subvolumes or use PID liveness (PIDs are reused). An active lock always wins.
pub fn cleanup_abandoned(directory: &Path) -> Result<CleanupStats> {
    let directory = fs::canonicalize(directory)?;
    let mut stats = CleanupStats::default();
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(id) = name
            .strip_prefix(".cairn-capture-")
            .and_then(|s| s.strip_suffix(".lock"))
        else {
            continue;
        };
        if id.len() != 32
            || !id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            continue;
        }
        ensure!(
            entry.file_type()?.is_file(),
            "snapshot marker is not a regular file: {}",
            entry.path().display()
        );
        let marker = entry.path();
        let mut lock = OpenOptions::new().read(true).write(true).open(&marker)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                stats.active += 1;
                continue;
            }
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let mut contents = Vec::new();
        (&mut lock).take(128).read_to_end(&mut contents)?;
        ensure!(
            contents == MARKER,
            "invalid snapshot marker: {}",
            marker.display()
        );
        let temporary = directory.join(format!(".cairn-capture-{id}"));
        if temporary.try_exists()? {
            let metadata = fs::symlink_metadata(&temporary)?;
            ensure!(
                metadata.is_dir() && metadata.ino() == 256,
                "not a Btrfs subvolume: {}",
                temporary.display()
            );
            let fs_type = Command::new("stat")
                .args(["-f", "-c", "%T"])
                .arg(&temporary)
                .output()?;
            ensure!(
                fs_type.status.success() && fs_type.stdout == b"btrfs\n",
                "snapshot is not on Btrfs"
            );
        }
        let source = Source {
            path: temporary.clone(),
            temporary: Some(temporary),
            marker: Some(marker),
            lock: Some(lock),
        };
        source.finish()?;
        stats.removed += 1;
    }
    Ok(stats)
}
impl Drop for Source {
    fn drop(&mut self) {
        if let Err(e) = self.cleanup() {
            eprintln!("temporary snapshot cleanup failed: {e:#}");
        }
    }
}
