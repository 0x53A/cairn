//! Local, retained Btrfs storage. UUIDs locate data; Cairn hashes verify bytes.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    process::Command,
};

use crate::{
    objects::valid_id,
    store::{Store, durable_mkdir},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BtrfsBacking {
    pub path: PathBuf,
    pub subtree: PathBuf,
    pub filesystem_uuid: String,
    pub subvolume_uuid: String,
    pub subvolume_id: u64,
}

/// Held for the complete lifetime of a reader, not merely while resolving paths.
pub(crate) struct Lease {
    pub root: Option<File>,
    pub _lock: Option<File>,
}

pub(crate) fn lock(store: &Store, id: &str, exclusive: bool) -> Result<Option<File>> {
    ensure!(valid_id(id), "invalid snapshot ID");
    let Store::Local(root) = store else {
        return Ok(None);
    };
    let dir = root.join("backing-locks");
    durable_mkdir(&dir)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join(id))?;
    if exclusive {
        file.lock()?;
    } else {
        file.lock_shared()?;
    }
    // Never unlink lock files: waiters must continue locking the same inode.
    Ok(Some(file))
}

pub(crate) fn relative(path: &Path, allow_empty: bool) -> Result<()> {
    ensure!(
        (allow_empty || !path.as_os_str().is_empty())
            && path.components().all(|c| matches!(c, Component::Normal(_))),
        "invalid backing-relative path"
    );
    Ok(())
}

fn open_directory(path: &Path) -> Result<File> {
    use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, openat2};
    Ok(File::from(
        openat2(
            CWD,
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS,
        )
        .context("open retained Btrfs snapshot (Linux openat2 required)")?,
    ))
}

pub(crate) fn open_beneath(root: &File, path: &Path, directory: bool) -> Result<File> {
    use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
    relative(path, directory)?;
    let name = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };
    let mut flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK;
    if directory {
        flags |= OFlags::DIRECTORY;
    }
    let file = File::from(openat2(
        root,
        name,
        flags,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
    )?);
    ensure!(
        directory || file.metadata()?.is_file(),
        "backing is not a regular file"
    );
    Ok(file)
}

impl BtrfsBacking {
    pub(crate) fn record(path: &Path, subtree: PathBuf) -> Result<Self> {
        relative(&subtree, true)?;
        let path = fs::canonicalize(path)?;
        let root = open_directory(&path)?;
        let identity = identity(&root)?;
        ensure!(identity.read_only, "backing subvolume must be read-only");
        rustix::fs::syncfs(&root).context("synchronize retained Btrfs snapshot")?;
        Ok(Self {
            path,
            subtree,
            filesystem_uuid: identity.filesystem_uuid,
            subvolume_uuid: identity.subvolume_uuid,
            subvolume_id: identity.subvolume_id,
        })
    }

    fn open_subvolume(&self) -> Result<File> {
        ensure!(self.path.is_absolute(), "backing path must be absolute");
        relative(&self.subtree, true)?;
        let root = open_directory(&self.path)?;
        let actual = identity(&root)?;
        ensure!(
            actual.filesystem_uuid == self.filesystem_uuid
                && actual.subvolume_uuid == self.subvolume_uuid
                && actual.subvolume_id == self.subvolume_id,
            "retained Btrfs snapshot identity mismatch: {}",
            self.path.display()
        );
        ensure!(
            actual.read_only,
            "retained Btrfs snapshot is no longer read-only"
        );
        Ok(root)
    }

    pub(crate) fn open(&self) -> Result<File> {
        open_beneath(&self.open_subvolume()?, &self.subtree, true)
    }

    /// Call only after switching durable metadata, under the exclusive Cairn lock.
    /// The backing parent is trusted, as for capture's snapshot directory.
    pub(crate) fn release(&self) -> Result<()> {
        let _root = self.open_subvolume()?;
        ensure!(
            self.path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(".cairn-retained-")),
            "refusing to delete an unmanaged backing subvolume"
        );
        command(&["property", "set", "-ts"], &self.path, &["ro", "false"])?;
        command(&["subvolume", "delete", "--commit-after"], &self.path, &[])?;
        File::open(self.path.parent().context("backing has no parent")?)?.sync_all()?;
        Ok(())
    }
}

fn command(before: &[&str], path: &Path, after: &[&str]) -> Result<()> {
    let output = Command::new("btrfs")
        .args(before)
        .arg(path)
        .args(after)
        .output()?;
    ensure!(
        output.status.success(),
        "Btrfs operation on {} failed: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

struct Identity {
    filesystem_uuid: String,
    subvolume_uuid: String,
    subvolume_id: u64,
    read_only: bool,
}

// Linux UAPI linux/btrfs.h. All buffers start zeroed, including reserved fields.
#[repr(C)]
struct Timespec {
    sec: u64,
    nsec: u32,
}
#[repr(C)]
struct SubvolumeInfo {
    treeid: u64,
    name: [u8; 256],
    parent_id: u64,
    dirid: u64,
    generation: u64,
    flags: u64,
    uuid: [u8; 16],
    parent_uuid: [u8; 16],
    received_uuid: [u8; 16],
    ctransid: u64,
    otransid: u64,
    stransid: u64,
    rtransid: u64,
    ctime: Timespec,
    otime: Timespec,
    stime: Timespec,
    rtime: Timespec,
    reserved: [u64; 8],
}
#[repr(C)]
struct FsInfo {
    max_id: u64,
    num_devices: u64,
    fsid: [u8; 16],
    nodesize: u32,
    sectorsize: u32,
    clone_alignment: u32,
    csum_type: u16,
    csum_size: u16,
    flags: u64,
    generation: u64,
    metadata_uuid: [u8; 16],
    reserved: [u8; 944],
}

fn identity(root: &File) -> Result<Identity> {
    use rustix::ioctl::{Updater, ioctl, opcode};
    ensure!(
        rustix::fs::fstatfs(root)?.f_type == 0x9123683e && root.metadata()?.ino() == 256,
        "backing is not a Btrfs subvolume root"
    );
    // SAFETY: repr(C) mirrors the Linux UAPI structs, which contain only integers
    // and integer arrays. The opcodes specify those exact types and sizes. The
    // kernel writes only within these initialized, correctly aligned buffers.
    let (fs, sub, flags) = unsafe {
        let mut fs: FsInfo = std::mem::zeroed();
        let mut sub: SubvolumeInfo = std::mem::zeroed();
        let mut flags = 0u64;
        ioctl(
            root,
            Updater::<{ opcode::read::<FsInfo>(0x94, 31) }, _>::new(&mut fs),
        )?;
        ioctl(
            root,
            Updater::<{ opcode::read::<SubvolumeInfo>(0x94, 60) }, _>::new(&mut sub),
        )?;
        // GET_SUBVOL_INFO exposes on-disk root flags, whose bit assignments
        // differ from the public BTRFS_SUBVOL_RDONLY ioctl flag (1 << 1).
        ioctl(
            root,
            Updater::<{ opcode::read::<u64>(0x94, 25) }, _>::new(&mut flags),
        )?;
        (fs, sub, flags)
    };
    ensure!(
        sub.uuid != [0; 16] && sub.parent_id != 0,
        "invalid or deleted backing subvolume"
    );
    Ok(Identity {
        filesystem_uuid: hex::encode(fs.fsid),
        subvolume_uuid: hex::encode(sub.uuid),
        subvolume_id: sub.treeid,
        read_only: flags & 2 != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backing_reads_refuse_symlinks_and_path_escapes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("root");
        fs::create_dir(&root)?;
        fs::write(root.join("file"), "content")?;
        std::os::unix::fs::symlink("file", root.join("link"))?;
        std::os::unix::fs::symlink(&root, temp.path().join("alias"))?;
        assert!(open_directory(&temp.path().join("alias")).is_err());
        let fd = open_directory(&root)?;
        assert!(open_beneath(&fd, Path::new("file"), false).is_ok());
        for path in ["link", "../root/file", "/etc/passwd", ""] {
            assert!(open_beneath(&fd, Path::new(path), false).is_err());
        }
        assert!(BtrfsBacking::record(&root, PathBuf::new()).is_err());
        Ok(())
    }

    #[test]
    fn reader_lease_blocks_replacement_on_a_persistent_lock_inode() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::Local(temp.path().join("repo"));
        let id = "a".repeat(64);
        let held = lock(&store, &id, false)?;
        let lock_path = temp.path().join("repo/backing-locks").join(&id);
        let inode = fs::metadata(&lock_path)?.ino();
        let writer = OpenOptions::new().read(true).write(true).open(&lock_path)?;
        assert!(matches!(
            writer.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(held);
        writer.try_lock()?;
        drop(writer);
        drop(lock(&store, &id, false)?);
        assert_eq!(fs::metadata(lock_path)?.ino(), inode);
        Ok(())
    }
}
