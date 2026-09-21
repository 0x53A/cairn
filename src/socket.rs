//! Ownership and crash recovery for pathname Unix listeners.
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::{self, File, OpenOptions},
    io::ErrorKind,
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

pub struct SocketPath {
    path: PathBuf,
    identity: (u64, u64),
    _lock: File,
}

impl SocketPath {
    pub async fn bind(path: &Path) -> Result<(tokio::net::UnixListener, Self)> {
        let mut lock_name = path.as_os_str().to_owned();
        lock_name.push(".lock");
        let lock_path = PathBuf::from(lock_name);
        if let Ok(meta) = fs::symlink_metadata(&lock_path) {
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "socket lock must be a regular file"
            );
        }
        // Parent must be trusted. Keep this file permanently: unlinking a lock
        // file can let two processes hold locks on different inodes.
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)?;
        lock.try_lock()
            .context("another Cairn server owns this socket")?;
        match fs::symlink_metadata(path) {
            Ok(meta) => {
                ensure!(
                    meta.file_type().is_socket(),
                    "refusing to replace a non-socket path"
                );
                match tokio::time::timeout(
                    Duration::from_secs(1),
                    tokio::net::UnixStream::connect(path),
                )
                .await
                {
                    Ok(Err(error)) if error.kind() == ErrorKind::ConnectionRefused => {
                        let current = fs::symlink_metadata(path)?;
                        ensure!(
                            (current.dev(), current.ino()) == (meta.dev(), meta.ino()),
                            "socket changed during recovery"
                        );
                        fs::remove_file(path)?;
                    }
                    _ => {
                        bail!("socket is active or its state is uncertain; refusing to replace it")
                    }
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = tokio::net::UnixListener::bind(path)?;
        let meta = fs::symlink_metadata(path)?;
        let guard = Self {
            path: path.into(),
            identity: (meta.dev(), meta.ino()),
            _lock: lock,
        };
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok((listener, guard))
    }
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        if let Ok(meta) = fs::symlink_metadata(&self.path)
            && meta.file_type().is_socket()
            && (meta.dev(), meta.ino()) == self.identity
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn preserves_foreign_listeners_symlinks_and_replacements() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("socket");
        let foreign = std::os::unix::net::UnixListener::bind(&path)?;
        assert!(SocketPath::bind(&path).await.is_err());
        assert!(std::os::unix::net::UnixStream::connect(&path).is_ok());
        drop(foreign);
        fs::remove_file(&path)?;
        let target = temp.path().join("target");
        fs::write(&target, "keep")?;
        std::os::unix::fs::symlink(&target, &path)?;
        assert!(SocketPath::bind(&path).await.is_err());
        assert!(fs::symlink_metadata(&path)?.file_type().is_symlink());
        fs::remove_file(&path)?;
        let (listener, guard) = SocketPath::bind(&path).await?;
        fs::remove_file(&path)?;
        fs::write(&path, "replacement")?;
        drop(listener);
        drop(guard);
        assert_eq!(fs::read_to_string(&path)?, "replacement");
        assert_eq!(fs::read_to_string(&target)?, "keep");
        Ok(())
    }
}
