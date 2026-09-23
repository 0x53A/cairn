use crate::objects::{MAX_OBJECT, envelope, valid_id};
use anyhow::{Context, Result, bail, ensure};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone)]
pub enum Store {
    Local(PathBuf),
    Ssh(std::sync::Arc<crate::native_ssh::NativeSsh>),
    Http {
        base: String,
        client: Client,
        token: Option<String>,
        tunnel: Option<std::sync::Arc<crate::ssh::Tunnel>>,
    },
}
impl Store {
    pub fn connect(location: &str, domain: &str, token: Option<String>) -> Result<Self> {
        ensure!(domain == "plain" || valid_id(domain), "invalid key domain");
        if location.starts_with("ssh://") {
            Ok(Self::Ssh(std::sync::Arc::new(
                crate::native_ssh::NativeSsh::connect(location, domain, token)?,
            )))
        } else if location.starts_with("ssh-openssh://") {
            let tunnel = std::sync::Arc::new(crate::ssh::Tunnel::start(location)?);
            Ok(Self::Http {
                base: format!("http://localhost/v1/{domain}"),
                client: Client::builder()
                    .unix_socket(tunnel.socket())
                    .no_proxy()
                    .timeout(Duration::from_secs(3600))
                    .redirect(reqwest::redirect::Policy::none())
                    .build()?,
                token,
                tunnel: Some(tunnel),
            })
        } else if let Some(path) = location.strip_prefix("unix://") {
            ensure!(
                Path::new(path).is_absolute(),
                "Unix socket path must be absolute: unix:///path/to/socket"
            );
            Ok(Self::Http {
                base: format!("http://localhost/v1/{domain}"),
                client: Client::builder()
                    .unix_socket(PathBuf::from(path))
                    .no_proxy()
                    .timeout(Duration::from_secs(3600))
                    .redirect(reqwest::redirect::Policy::none())
                    .build()?,
                token,
                tunnel: None,
            })
        } else if location.starts_with("http://") || location.starts_with("https://") {
            let url = reqwest::Url::parse(location)?;
            ensure!(
                url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "URL must not contain credentials, query, or fragment"
            );
            Ok(Self::Http {
                base: format!("{}/v1/{domain}", location.trim_end_matches('/')),
                client: Client::builder()
                    .timeout(Duration::from_secs(3600))
                    .redirect(reqwest::redirect::Policy::none())
                    .build()?,
                token,
                tunnel: None,
            })
        } else {
            ensure!(!location.contains("://"), "unsupported repository scheme");
            Ok(Self::Local(Path::new(location).join(domain)))
        }
    }
    fn request(&self, method: reqwest::Method, key: &str) -> reqwest::blocking::RequestBuilder {
        let Self::Http {
            base,
            client,
            token,
            ..
        } = self
        else {
            unreachable!()
        };
        let r = client.request(method, format!("{base}/{key}"));
        match token {
            Some(t) => r.bearer_auth(t),
            None => r,
        }
    }
    pub fn exists(&self, key: &str) -> Result<bool> {
        valid_key(key)?;
        match self {
            Self::Ssh(client) => {
                let (status, _) = client.request(reqwest::Method::HEAD, key, vec![])?;
                if status == reqwest::StatusCode::NOT_FOUND {
                    return Ok(false);
                }
                ensure!(status.is_success(), "SSH HTTP request failed: {status}");
                Ok(true)
            }
            Self::Local(root) => {
                if !root.join(key).try_exists()? {
                    return Ok(false);
                }
                // A damaged object is not a usable deduplication hit. Fail
                // explicitly rather than publishing another snapshot using it.
                self.get(key)?;
                Ok(true)
            }
            Self::Http { .. } => {
                let r = self.request(reqwest::Method::HEAD, key).send()?;
                if r.status() == reqwest::StatusCode::NOT_FOUND {
                    return Ok(false);
                }
                r.error_for_status()?;
                Ok(true)
            }
        }
    }
    pub fn get(&self, key: &str) -> Result<Vec<u8>> {
        valid_key(key)?;
        let reader: Box<dyn Read> = match self {
            Self::Ssh(_) => Box::new(std::io::Cursor::new(
                self.ssh_request(reqwest::Method::GET, key, vec![])?.1,
            )),
            Self::Local(root) => {
                Box::new(File::open(root.join(key)).with_context(|| format!("read {key}"))?)
            }
            Self::Http { .. } => Box::new(
                self.request(reqwest::Method::GET, key)
                    .send()?
                    .error_for_status()?,
            ),
        };
        let mut bytes = Vec::new();
        reader
            .take((MAX_OBJECT + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_OBJECT,
            "stored object exceeds size limit"
        );
        validate_value(key, &bytes).with_context(|| format!("invalid stored {key}"))?;
        Ok(bytes)
    }
    /// Atomic create-if-absent; true means newly installed. Never overwrites.
    pub fn put(&self, key: &str, bytes: &[u8]) -> Result<bool> {
        valid_key(key)?;
        ensure!(bytes.len() <= MAX_OBJECT, "object too large");
        validate_value(key, bytes)?;
        match self {
            Self::Ssh(_) => Ok(self
                .ssh_request(reqwest::Method::PUT, key, bytes.to_vec())?
                .0
                == reqwest::StatusCode::CREATED),
            Self::Local(root) => {
                if key.starts_with("snapshots/") {
                    complete(self, envelope(bytes)?.0.refs)?;
                }
                let path = root.join(key);
                let dir = path.parent().unwrap();
                // Sync creation of namespace/bucket directories, not just file contents.
                durable_mkdir(dir)?;
                let mut temp = tempfile::NamedTempFile::new_in(dir)?;
                #[cfg(test)]
                storage_fault::before_write(temp.as_file_mut(), bytes)?;
                temp.write_all(bytes)?;
                temp.as_file().sync_all()?;
                match temp.persist_noclobber(&path) {
                    Ok(_) => {
                        File::open(dir)?.sync_all()?;
                        Ok(true)
                    }
                    Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let existing = self.get(key)?;
                        if key.starts_with("snapshots/") {
                            // The winner may use different chunk boundaries.
                            complete(self, envelope(&existing)?.0.refs)?;
                        }
                        File::open(&path)?.sync_all()?;
                        File::open(dir)?.sync_all()?;
                        Ok(false)
                    }
                    Err(e) => Err(e.error.into()),
                }
            }
            Self::Http { .. } => {
                let r = self
                    .request(reqwest::Method::PUT, key)
                    .body(bytes.to_vec())
                    .send()?
                    .error_for_status()?;
                Ok(r.status() == reqwest::StatusCode::CREATED)
            }
        }
    }
    pub fn list(&self, bucket: &str) -> Result<Vec<String>> {
        ensure!(matches!(bucket, "snapshots" | "tags"), "invalid listing");
        match self {
            Self::Ssh(_) => Ok(serde_json::from_slice(
                &self.ssh_request(reqwest::Method::GET, bucket, vec![])?.1,
            )?),
            Self::Local(root) => {
                let dir = root.join(bucket);
                if !dir.try_exists()? {
                    return Ok(vec![]);
                }
                let mut entries = Vec::new();
                for e in fs::read_dir(dir)? {
                    let e = e?;
                    let name = e.file_name().to_string_lossy().to_string();
                    if valid_key(&format!("{bucket}/{name}")).is_ok() {
                        entries.push(name);
                    }
                }
                entries.sort();
                Ok(entries)
            }
            Self::Http { .. } => Ok(serde_json::from_slice(&self.get_list(bucket)?)?),
        }
    }
    fn get_list(&self, bucket: &str) -> Result<Vec<u8>> {
        let r = self
            .request(reqwest::Method::GET, bucket)
            .send()?
            .error_for_status()?;
        let mut bytes = Vec::new();
        r.take((MAX_OBJECT + 1) as u64).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_OBJECT,
            "listing too large; pagination not implemented"
        );
        Ok(bytes)
    }
    pub fn tag(&self, name: &str, id: &str) -> Result<()> {
        ensure!(
            !name.is_empty() && name.len() <= 120,
            "tag must be 1–120 UTF-8 bytes"
        );
        ensure!(
            valid_id(id) && self.exists(&format!("snapshots/{id}"))?,
            "snapshot does not exist"
        );
        let key = format!("tags/{}", hex::encode(name));
        if !self.put(&key, id.as_bytes())? {
            ensure!(
                self.get(&key)? == id.as_bytes(),
                "tag already identifies another snapshot"
            );
        }
        Ok(())
    }
    pub fn resolve(&self, name: &str) -> Result<String> {
        if valid_id(name) && self.exists(&format!("snapshots/{name}"))? {
            return Ok(name.into());
        }
        let id = String::from_utf8(self.get(&format!("tags/{}", hex::encode(name)))?)?;
        ensure!(valid_id(&id), "invalid tag target");
        Ok(id)
    }
    pub fn remote_import(&self, request: &Import) -> Result<TransferStats> {
        if matches!(self, Self::Ssh(_)) {
            return Ok(serde_json::from_slice(
                &self
                    .ssh_request(
                        reqwest::Method::POST,
                        "import",
                        serde_json::to_vec(request)?,
                    )?
                    .1,
            )?);
        }
        ensure!(matches!(self, Self::Http { .. }), "destination is not HTTP");
        Ok(self
            .request(reqwest::Method::POST, "import")
            .json(request)
            .send()?
            .error_for_status()?
            .json()?)
    }

    fn ssh_request(
        &self,
        method: reqwest::Method,
        key: &str,
        body: Vec<u8>,
    ) -> Result<(reqwest::StatusCode, Vec<u8>)> {
        let Self::Ssh(client) = self else {
            unreachable!()
        };
        let (status, bytes) = client.request(method, key, body)?;
        ensure!(status.is_success(), "SSH HTTP request failed: {status}");
        Ok((status, bytes))
    }
}

fn validate_value(key: &str, bytes: &[u8]) -> Result<()> {
    if key.starts_with("tags/") {
        ensure!(
            std::str::from_utf8(bytes).is_ok_and(valid_id),
            "invalid tag target"
        );
    } else {
        envelope(bytes)?;
    }
    Ok(())
}

/// Check every reachable envelope before publishing a snapshot, without E2E keys.
pub fn complete(store: &Store, refs: Vec<String>) -> Result<()> {
    let mut pending = refs;
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        ensure!(seen.len() <= 10_000_000, "object-count limit exceeded");
        let data = store.get(&format!("objects/{id}"))?;
        pending.extend(envelope(&data)?.0.refs);
    }
    Ok(())
}

fn durable_mkdir(path: &Path) -> Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        durable_mkdir(parent)?;
    }
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {}
        Err(e) => return Err(e.into()),
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn valid_key(key: &str) -> Result<()> {
    let (bucket, id) = key.split_once('/').context("missing bucket")?;
    match bucket {
        "objects" | "snapshots" => ensure!(valid_id(id), "invalid object ID"),
        "tags" => ensure!(
            !id.is_empty()
                && id.len() <= 240
                && id.len() % 2 == 0
                && id
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "invalid tag"
        ),
        _ => bail!("invalid bucket"),
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
pub struct Import {
    pub source: String,
    pub snapshot: String,
    pub source_token: Option<String>,
}
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct TransferStats {
    pub objects_copied: u64,
    pub objects_present: u64,
    pub bytes_copied: u64,
}

/// Copy opaque objects. The coordinator and both servers need no encryption key.
pub fn replicate(source: &Store, destination: &Store, id: &str) -> Result<TransferStats> {
    ensure!(valid_id(id), "invalid snapshot ID");
    let snapkey = format!("snapshots/{id}");
    // Existing snapshot may use a different valid chunk recipe. Verify/copy that
    // recipe's graph separately; never overwrite an existing snapshot record.
    let snapshot = source.get(&snapkey)?;
    let (header, _, _) = envelope(&snapshot)?;
    let mut pending = header.refs;
    let mut seen = HashSet::new();
    let mut stats = TransferStats::default();
    while let Some(id) = pending.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        ensure!(
            seen.len() <= 10_000_000,
            "transfer object-count limit exceeded"
        );
        let key = format!("objects/{id}");
        let present = destination.exists(&key)?;
        let data = if present {
            destination.get(&key)?
        } else {
            source.get(&key)?
        };
        let (header, _, _) = envelope(&data)?;
        pending.extend(header.refs);
        if present {
            stats.objects_present += 1;
        } else if destination.put(&key, &data)? {
            stats.objects_copied += 1;
            stats.bytes_copied += data.len() as u64;
        }
    }
    destination.put(&snapkey, &snapshot)?;
    Ok(stats)
}

#[cfg(test)]
mod storage_fault {
    use super::*;
    use std::cell::Cell;
    // Thread-local, test-only injection: no environment switch in production.
    thread_local! { static REMAINING: Cell<Option<usize>> = const { Cell::new(None) }; }
    pub fn before_write(file: &mut File, bytes: &[u8]) -> Result<()> {
        REMAINING.with(|remaining| match remaining.get() {
            Some(0) => {
                remaining.set(None);
                file.write_all(&bytes[..bytes.len() / 2])?;
                Err(std::io::Error::from_raw_os_error(28).into())
            }
            Some(n) => {
                remaining.set(Some(n - 1));
                Ok(())
            }
            None => Ok(()),
        })
    }
    #[test]
    fn disk_full_mid_object_does_not_publish_and_retry_reuses_objects() -> Result<()> {
        use crate::{
            objects::Keys,
            snapshot::{self, CaptureOptions},
        };
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        fs::create_dir(&source)?;
        for i in 0..4 {
            fs::write(source.join(format!("file-{i}")), vec![i; 8192])?;
        }
        let root = temp.path().join("repo");
        let store = Store::Local(root.clone());
        let keys = Keys::from_bytes([2; 32]);
        REMAINING.with(|n| n.set(Some(2)));
        let error =
            snapshot::capture(&store, &keys, &source, &CaptureOptions::default()).unwrap_err();
        assert!(error.chain().any(|e| {
            e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.raw_os_error() == Some(28))
        }));
        assert!(store.list("snapshots")?.is_empty());
        let saved = fs::read_dir(root.join("objects"))?
            .map(|e| {
                let path = e?.path();
                Ok((path.clone(), fs::read(&path)?))
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        assert_eq!(saved.len(), 2, "partial temporary object must be removed");
        let id = snapshot::capture(&store, &keys, &source, &CaptureOptions::default())?;
        for (path, bytes) in saved {
            assert_eq!(fs::read(path)?, bytes);
        }
        snapshot::verify(&store, &keys, &id)?;
        snapshot::restore(&store, &keys, &id, &temp.path().join("restored"))?;
        Ok(())
    }
}
