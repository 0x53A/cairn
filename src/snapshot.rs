use crate::{
    backing::{self, BtrfsBacking, Lease},
    chunking::Chunker,
    objects::{Keys, valid_id},
    store::Store,
};
use anyhow::{Context, Result, bail, ensure};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

const PAGE_ENTRIES: usize = 128;
pub use crate::chunking::{DEFAULT_CHUNK, MAX_CHUNK};
pub(crate) const MAX_DEPTH: usize = 256;

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct Logical {
    pub kind: String,
    pub mode: u32,
    pub size: u64,
    pub digest: String,
    pub target: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Node {
    pub logical: Logical,
    pub recipe: Option<String>,
}

pub(crate) fn validate_node(node: &Node) -> Result<()> {
    let logical = &node.logical;
    ensure!(logical.mode <= 0o777, "unsupported permission bits");
    ensure!(valid_id(&logical.digest), "invalid logical digest");
    ensure!(
        node.recipe.as_deref().is_none_or(valid_id),
        "invalid recipe ID"
    );
    match logical.kind.as_str() {
        "directory" => ensure!(
            logical.size == 0 && logical.target.is_none(),
            "invalid directory metadata"
        ),
        "file" => {
            ensure!(logical.target.is_none(), "file has a symlink target");
            ensure!(
                (logical.size == 0) == node.recipe.is_none(),
                "file size and recipe disagree"
            );
            if logical.size == 0 {
                ensure!(
                    logical.digest == blake3::hash(b"").to_hex().as_str(),
                    "invalid empty file digest"
                );
            }
        }
        "symlink" => {
            let target = logical
                .target
                .as_deref()
                .context("missing symlink target")?;
            ensure!(
                node.recipe.is_none()
                    && logical.mode == 0o777
                    && !target.is_empty()
                    && !target.contains('\0')
                    && target.len() as u64 == logical.size
                    && blake3::hash(target.as_bytes()).to_hex().as_str() == logical.digest,
                "symlink identity mismatch"
            );
        }
        _ => bail!("unsupported entry kind"),
    }
    Ok(())
}
#[derive(Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub node: Node,
}
#[derive(Serialize, Deserialize)]
pub struct Chunk {
    pub id: String,
    pub offset: u64,
    pub len: usize,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Page {
    BtrfsFile {
        path: String,
    },
    File {
        previous: Option<String>,
        chunks: Vec<Chunk>,
    },
    Directory {
        previous: Option<String>,
        entries: Vec<Entry>,
    },
}
impl Page {
    fn refs(&self) -> Vec<String> {
        let mut result = Vec::new();
        match self {
            Self::BtrfsFile { .. } => {}
            Self::File { previous, chunks } => {
                result.extend(previous.iter().cloned());
                result.extend(chunks.iter().map(|c| c.id.clone()));
            }
            Self::Directory { previous, entries } => {
                result.extend(previous.iter().cloned());
                result.extend(entries.iter().filter_map(|e| e.node.recipe.clone()));
            }
        }
        result
    }
}
#[derive(Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub id: String,
    pub root: Node,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backing: Option<BtrfsBacking>,
    #[serde(skip)]
    pub(crate) lease: Option<Lease>,
}

fn snapshot_id(root: &Logical) -> Result<String> {
    let mut h = blake3::Hasher::new();
    h.update(b"cairn logical snapshot v1\0");
    h.update(&serde_json::to_vec(root)?);
    Ok(h.finalize().to_hex().to_string())
}
fn directory_hasher(mode: u32) -> blake3::Hasher {
    let mut h = blake3::Hasher::new();
    h.update(b"cairn logical directory v1\0");
    h.update(&mode.to_le_bytes());
    h
}
fn hash_entry(h: &mut blake3::Hasher, name: &str, logical: &Logical) -> Result<()> {
    let bytes = serde_json::to_vec(&(name, logical))?;
    h.update(&(bytes.len() as u64).to_le_bytes());
    h.update(&bytes);
    Ok(())
}

/// Retained snapshots hold a read lease until the returned value is dropped.
/// Drop that value before materializing the same snapshot in this process.
pub fn load_snapshot(store: &Store, keys: &Keys, id: &str) -> Result<Snapshot> {
    ensure!(valid_id(id), "invalid snapshot ID");
    // Portable records never change backing again. Keep ordinary repositories
    // readable without write permission or creating lock files on every read.
    let encoded = store.get(&format!("snapshots/{id}"))?;
    if !crate::objects::envelope(&encoded)?.0.local {
        return decode_snapshot(store, keys, id, &encoded);
    }
    let lock = backing::lock(store, id, false)?;
    let mut snap = load_unlocked(store, keys, id)?;
    snap.lease.as_mut().unwrap()._lock = lock;
    Ok(snap)
}

fn load_unlocked(store: &Store, keys: &Keys, id: &str) -> Result<Snapshot> {
    ensure!(valid_id(id), "invalid snapshot ID");
    let encoded = store.get(&format!("snapshots/{id}"))?;
    decode_snapshot(store, keys, id, &encoded)
}

fn decode_snapshot(store: &Store, keys: &Keys, id: &str, encoded: &[u8]) -> Result<Snapshot> {
    let local = crate::objects::envelope(encoded)?.0.local;
    let (bytes, refs) = keys.open(encoded)?;
    let mut snap: Snapshot = serde_json::from_slice(&bytes)?;
    ensure!(
        snap.version == if local { 2 } else { 1 }
            && local == snap.backing.is_some()
            && snap.id == id
            && snapshot_id(&snap.root.logical)? == id,
        "snapshot identity mismatch"
    );
    ensure!(
        refs == snap.root.recipe.iter().cloned().collect::<Vec<_>>(),
        "snapshot references mismatch"
    );
    ensure!(
        snap.root.logical.kind == "directory",
        "snapshot root is not a directory"
    );
    validate_node(&snap.root)?;
    let root = if let Some(backing) = &snap.backing {
        ensure!(
            matches!(store, Store::Local(_)),
            "Btrfs backing is local-only"
        );
        Some(backing.open()?)
    } else {
        None
    };
    snap.lease = Some(Lease { root, _lock: None });
    Ok(snap)
}

/// Authenticate every reachable object without expanding files to disk.
/// Restore additionally verifies each reconstructed whole-file digest.
pub fn verify(store: &Store, keys: &Keys, id: &str) -> Result<usize> {
    let snapshot = load_snapshot(store, keys, id)?;
    verify_loaded(store, keys, &snapshot)
}

fn verify_loaded(store: &Store, keys: &Keys, snapshot: &Snapshot) -> Result<usize> {
    // Validate directory identities and names as well as the opaque graph.
    index_loaded(store, keys, snapshot)?;
    if snapshot.backing.is_some() {
        verify_backed_tree(store, keys, snapshot, &snapshot.root, Path::new(""), 0)?;
    }
    let mut pending: Vec<_> = snapshot.root.recipe.iter().cloned().collect();
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        ensure!(
            seen.len() <= 10_000_000,
            "verification object-count limit exceeded"
        );
        let (payload, refs) =
            get_object(store, keys, &id).with_context(|| format!("verify object {id}"))?;
        match payload.first() {
            Some(b'M') => {
                let page = decode_page(&payload, &refs)?;
                ensure!(
                    snapshot.backing.is_some() || !matches!(page, Page::BtrfsFile { .. }),
                    "portable snapshot contains a local backing recipe"
                );
            }
            Some(b'C') => ensure!(refs.is_empty(), "chunk has references"),
            _ => bail!("invalid object type: {id}"),
        }
        pending.extend(refs);
    }
    Ok(seen.len())
}

pub(crate) fn get_object(store: &Store, keys: &Keys, id: &str) -> Result<(Vec<u8>, Vec<String>)> {
    let (payload, refs) = keys.open(&store.get(&format!("objects/{id}"))?)?;
    ensure!(
        keys.object_id(&payload) == id,
        "object content hash mismatch: {id}"
    );
    Ok((payload, refs))
}
pub(crate) fn get_page(store: &Store, keys: &Keys, id: &str) -> Result<Page> {
    let (bytes, refs) = get_object(store, keys, id)?;
    decode_page(&bytes, &refs)
}
fn decode_page(bytes: &[u8], refs: &[String]) -> Result<Page> {
    ensure!(bytes.first() == Some(&b'M'), "not a metadata object");
    let page: Page = serde_json::from_slice(&bytes[1..])?;
    ensure!(page.refs() == refs, "metadata references mismatch");
    match &page {
        Page::BtrfsFile { path } => backing::relative(Path::new(path), false)?,
        Page::File { chunks, .. } => ensure!(
            !chunks.is_empty() && chunks.len() <= PAGE_ENTRIES,
            "invalid file page size"
        ),
        Page::Directory { entries, .. } => ensure!(
            !entries.is_empty() && entries.len() <= PAGE_ENTRIES,
            "invalid directory page size"
        ),
    }
    Ok(page)
}

pub struct CaptureOptions {
    pub chunker: Chunker,
    pub chunk_size: usize,
    pub ignore_file: Option<PathBuf>,
}
impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            chunker: Chunker::Fixed,
            chunk_size: DEFAULT_CHUNK,
            ignore_file: None,
        }
    }
}

pub fn capture(
    store: &Store,
    keys: &Keys,
    source: &Path,
    options: &CaptureOptions,
) -> Result<String> {
    let mut builder = Builder::new(Some(store), keys, source, options, false)?;
    let root = builder.visit(&builder.root.clone(), 0)?;
    let id = snapshot_id(&root.logical)?;
    let refs = root.recipe.iter().cloned().collect();
    let snap = Snapshot {
        version: 1,
        id: id.clone(),
        root,
        backing: None,
        lease: None,
    };
    store.put(
        &format!("snapshots/{id}"),
        &keys.seal(&serde_json::to_vec(&snap)?, refs)?,
    )?;
    Ok(id)
}

/// Capture logical metadata and retain the read-only Btrfs subvolume as payload.
pub fn capture_btrfs(
    store: &Store,
    keys: &Keys,
    source: &Path,
    snapshot_dir: Option<&Path>,
    options: &CaptureOptions,
) -> Result<String> {
    let Store::Local(repo) = store else {
        bail!("Btrfs backing requires a local repository")
    };
    options.chunker.validate(options.chunk_size)?;
    crate::store::durable_mkdir(repo)?;
    ensure!(
        !fs::canonicalize(repo)?.starts_with(fs::canonicalize(source)?),
        "repository must be outside the captured directory"
    );
    let default_dir = repo.join("backings");
    let snapshot_dir = match snapshot_dir {
        Some(dir) => dir,
        None => {
            crate::store::durable_mkdir(&default_dir)?;
            &default_dir
        }
    };
    let source = crate::capture::Source::open(source, false, Some(snapshot_dir))?;
    let mut builder = Builder::new(Some(store), keys, &source.path, options, false)?;
    builder.retained = true;
    let root = builder.visit(&builder.root.clone(), 0)?;
    let id = snapshot_id(&root.logical)?;
    let _lock = backing::lock(store, &id, true)?;
    if store.exists(&format!("snapshots/{id}"))? {
        verify_loaded(store, keys, &load_unlocked(store, keys, &id)?)?;
        source.finish()?;
        return Ok(id);
    }
    let backing = source.retain()?;
    let snapshot = Snapshot {
        version: 2,
        id: id.clone(),
        root,
        backing: Some(backing),
        lease: None,
    };
    let refs = snapshot.root.recipe.iter().cloned().collect();
    store.put_local(
        &format!("snapshots/{id}"),
        &keys.seal_local(&serde_json::to_vec(&snapshot)?, refs)?,
    )?;
    Ok(id)
}

impl Snapshot {
    pub(crate) fn backed_file(
        &self,
        store: &Store,
        keys: &Keys,
        node: &Node,
        path: &Path,
    ) -> Result<File> {
        let root = self
            .lease
            .as_ref()
            .and_then(|l| l.root.as_ref())
            .context("file requires a retained Btrfs backing")?;
        validate_node(node)?;
        if let Some(id) = &node.recipe {
            let Page::BtrfsFile { path: stored } = get_page(store, keys, id)? else {
                bail!("not a Btrfs file recipe");
            };
            ensure!(
                Path::new(&stored) == path,
                "Btrfs file recipe path mismatch"
            );
        }
        let file = backing::open_beneath(root, path, false)?;
        ensure!(
            file.metadata()?.len() == node.logical.size,
            "backing file size mismatch"
        );
        Ok(file)
    }
}

fn verify_backed_tree(
    store: &Store,
    keys: &Keys,
    snapshot: &Snapshot,
    node: &Node,
    path: &Path,
    depth: usize,
) -> Result<()> {
    ensure!(depth <= MAX_DEPTH, "directory nesting limit exceeded");
    match node.logical.kind.as_str() {
        "directory" => {
            for entry in directory_entries(store, keys, node)? {
                verify_backed_tree(
                    store,
                    keys,
                    snapshot,
                    &entry.node,
                    &path.join(entry.name),
                    depth + 1,
                )?;
            }
        }
        "file" => {
            let mut file = snapshot.backed_file(store, keys, node, path)?;
            let mut h = blake3::Hasher::new();
            let mut size = 0u64;
            for bytes in
                Chunker::Fixed.chunks_for_file(&mut file, DEFAULT_CHUNK, Some(node.logical.size))?
            {
                let bytes = bytes?;
                size += bytes.len() as u64;
                h.update(&bytes);
            }
            ensure!(
                size == node.logical.size && h.finalize().to_hex().as_str() == node.logical.digest,
                "backing file hash mismatch: {}",
                path.display()
            );
        }
        "symlink" => {}
        _ => bail!("unsupported entry kind"),
    }
    Ok(())
}

/// Convert a local backing to ordinary objects, keeping the logical ID and tags.
pub fn materialize(store: &Store, keys: &Keys, id: &str, options: &CaptureOptions) -> Result<()> {
    ensure!(
        matches!(store, Store::Local(_)),
        "materialize requires a local repository"
    );
    options.chunker.validate(options.chunk_size)?;
    let _lock = backing::lock(store, id, true)?;
    let snapshot = load_unlocked(store, keys, id)?;
    let Some(backing) = snapshot.backing.clone() else {
        verify_loaded(store, keys, &snapshot)?;
        return Ok(());
    };
    let (portable, _) = export_tree(store, store, keys, &snapshot, options)?;
    let refs = portable.root.recipe.iter().cloned().collect();
    store.replace_snapshot(id, &keys.seal(&serde_json::to_vec(&portable)?, refs)?)?;
    drop(snapshot);
    backing.release().with_context(|| {
        format!(
            "snapshot {id} is materialized; old backing remains at {} (release failed)",
            backing.path.display()
        )
    })?;
    Ok(())
}

/// Stream a retained snapshot into the portable format, encrypting before upload.
/// No payload spool and no mutation of the source backing are required.
pub fn copy_btrfs(
    source: &Store,
    destination: &Store,
    keys: &Keys,
    id: &str,
    options: &CaptureOptions,
) -> Result<crate::store::TransferStats> {
    options.chunker.validate(options.chunk_size)?;
    let snapshot = load_snapshot(source, keys, id)?;
    if snapshot.backing.is_none() {
        drop(snapshot);
        return crate::store::replicate(source, destination, id);
    }
    let (portable, stats) = export_tree(source, destination, keys, &snapshot, options)?;
    let refs = portable.root.recipe.iter().cloned().collect();
    // No source data is needed after this point. Release the read lock before
    // publication, including when a remote endpoint serves the source repository.
    drop(snapshot);
    destination.put(
        &format!("snapshots/{id}"),
        &keys.seal(&serde_json::to_vec(&portable)?, refs)?,
    )?;
    Ok(stats)
}

fn export_tree(
    source: &Store,
    destination: &Store,
    keys: &Keys,
    snapshot: &Snapshot,
    options: &CaptureOptions,
) -> Result<(Snapshot, crate::store::TransferStats)> {
    let mut exporter = Exporter {
        source,
        destination,
        keys,
        snapshot,
        options,
        stats: crate::store::TransferStats::default(),
    };
    let root = exporter.node(&snapshot.root, Path::new(""), 0)?;
    ensure!(
        snapshot_id(&root.logical)? == snapshot.id,
        "conversion changed snapshot identity"
    );
    Ok((
        Snapshot {
            version: 1,
            id: snapshot.id.clone(),
            root,
            backing: None,
            lease: None,
        },
        exporter.stats,
    ))
}

struct Exporter<'a> {
    source: &'a Store,
    destination: &'a Store,
    keys: &'a Keys,
    snapshot: &'a Snapshot,
    options: &'a CaptureOptions,
    stats: crate::store::TransferStats,
}
impl Exporter<'_> {
    fn object(&mut self, payload: &[u8], refs: Vec<String>) -> Result<String> {
        let id = self.keys.object_id(payload);
        let key = format!("objects/{id}");
        if self.destination.exists(&key)? {
            self.stats.objects_present += 1;
        } else {
            let encoded = self.keys.seal(payload, refs)?;
            if self.destination.put(&key, &encoded)? {
                self.stats.objects_copied += 1;
                self.stats.bytes_copied += encoded.len() as u64;
            } else {
                self.stats.objects_present += 1;
            }
        }
        Ok(id)
    }
    fn page(&mut self, page: &Page) -> Result<String> {
        let mut payload = vec![b'M'];
        payload.extend(serde_json::to_vec(page)?);
        self.object(&payload, page.refs())
    }
    fn node(&mut self, node: &Node, path: &Path, depth: usize) -> Result<Node> {
        ensure!(depth <= MAX_DEPTH, "directory nesting limit exceeded");
        validate_node(node)?;
        let mut previous = None;
        match node.logical.kind.as_str() {
            "directory" => {
                let mut entries = Vec::new();
                for entry in directory_entries(self.source, self.keys, node)? {
                    entries.push(Entry {
                        node: self.node(&entry.node, &path.join(&entry.name), depth + 1)?,
                        name: entry.name,
                    });
                    if entries.len() == PAGE_ENTRIES {
                        previous = Some(self.page(&Page::Directory {
                            previous: previous.take(),
                            entries: std::mem::take(&mut entries),
                        })?);
                    }
                }
                if !entries.is_empty() {
                    previous = Some(self.page(&Page::Directory {
                        previous: previous.take(),
                        entries,
                    })?);
                }
            }
            "file" => {
                let mut file = self
                    .snapshot
                    .backed_file(self.source, self.keys, node, path)?;
                let mut h = blake3::Hasher::new();
                let mut size = 0u64;
                let mut chunks = Vec::new();
                for bytes in self.options.chunker.chunks_for_file(
                    &mut file,
                    self.options.chunk_size,
                    Some(node.logical.size),
                )? {
                    let bytes = bytes?;
                    h.update(&bytes);
                    let mut payload = Vec::with_capacity(bytes.len() + 1);
                    payload.push(b'C');
                    payload.extend_from_slice(&bytes);
                    chunks.push(Chunk {
                        id: self.object(&payload, vec![])?,
                        offset: size,
                        len: bytes.len(),
                    });
                    size += bytes.len() as u64;
                    if chunks.len() == PAGE_ENTRIES {
                        previous = Some(self.page(&Page::File {
                            previous: previous.take(),
                            chunks: std::mem::take(&mut chunks),
                        })?);
                    }
                }
                ensure!(
                    size == node.logical.size
                        && h.finalize().to_hex().as_str() == node.logical.digest,
                    "backing file hash mismatch: {}",
                    path.display()
                );
                if !chunks.is_empty() {
                    previous = Some(self.page(&Page::File {
                        previous: previous.take(),
                        chunks,
                    })?);
                }
            }
            "symlink" => {}
            _ => bail!("unsupported entry kind"),
        }
        Ok(Node {
            logical: node.logical.clone(),
            recipe: previous,
        })
    }
}

struct Builder<'a> {
    store: Option<&'a Store>,
    keys: &'a Keys,
    root: PathBuf,
    device: u64,
    chunk_size: usize,
    chunker: Chunker,
    ignore: Gitignore,
    collect: bool,
    index: BTreeMap<String, Logical>,
    btrfs: bool,
    retained: bool,
}
impl<'a> Builder<'a> {
    fn new(
        store: Option<&'a Store>,
        keys: &'a Keys,
        root: &Path,
        options: &CaptureOptions,
        collect: bool,
    ) -> Result<Self> {
        options.chunker.validate(options.chunk_size)?;
        let root = fs::canonicalize(root)?;
        ensure!(root.is_dir(), "source must be a directory");
        let mut ignore = GitignoreBuilder::new(&root);
        if let Some(path) = &options.ignore_file
            && let Some(error) = ignore.add(path)
        {
            return Err(error.into());
        }
        let btrfs = rustix::fs::fstatfs(File::open(&root)?)?.f_type == 0x9123683e;
        Ok(Self {
            store,
            keys,
            device: root.metadata()?.dev(),
            root,
            chunk_size: options.chunk_size,
            chunker: options.chunker,
            ignore: ignore.build()?,
            collect,
            index: BTreeMap::new(),
            btrfs,
            retained: false,
        })
    }
    fn object(&self, payload: &[u8], refs: Vec<String>) -> Result<String> {
        let id = self.keys.object_id(payload);
        if let Some(store) = self.store {
            let key = format!("objects/{id}");
            if !store.exists(&key)? {
                store.put(&key, &self.keys.seal(payload, refs)?)?;
            }
        }
        Ok(id)
    }
    fn page(&self, page: &Page) -> Result<String> {
        let mut payload = vec![b'M'];
        payload.extend(serde_json::to_vec(page)?);
        if matches!(page, Page::BtrfsFile { .. }) {
            let id = self.keys.object_id(&payload);
            self.store
                .context("backing page requires storage")?
                .put_local(
                    &format!("objects/{id}"),
                    &self.keys.seal_local(&payload, page.refs())?,
                )?;
            Ok(id)
        } else {
            self.object(&payload, page.refs())
        }
    }
    fn visit(&mut self, path: &Path, depth: usize) -> Result<Node> {
        ensure!(depth <= MAX_DEPTH, "directory nesting exceeds {MAX_DEPTH}");
        let meta = fs::symlink_metadata(path)?;
        let mode = meta.permissions().mode() & 0o777;
        let node = if meta.is_file() {
            let mut file = File::open(path)?;
            let mut hasher = blake3::Hasher::new();
            let mut size = 0u64;
            let mut chunks = Vec::new();
            let mut previous = None;
            // Comparison only needs the whole-file hash, so it need not scan
            // for chunk boundaries or know the stored snapshot's chunker.
            let read_mode = if self.store.is_some() && !self.retained {
                self.chunker
            } else {
                Chunker::Fixed
            };
            for buffer in read_mode.chunks_for_file(&mut file, self.chunk_size, Some(meta.len()))? {
                let buffer = buffer?;
                let n = buffer.len();
                hasher.update(&buffer);
                if self.store.is_some() && !self.retained {
                    let mut payload = Vec::with_capacity(n + 1);
                    payload.push(b'C');
                    payload.extend_from_slice(&buffer);
                    chunks.push(Chunk {
                        id: self.object(&payload, vec![])?,
                        offset: size,
                        len: n,
                    });
                    if chunks.len() == PAGE_ENTRIES {
                        previous = Some(self.page(&Page::File {
                            previous: previous.take(),
                            chunks: std::mem::take(&mut chunks),
                        })?);
                    }
                }
                size += n as u64;
            }
            if !chunks.is_empty() {
                previous = Some(self.page(&Page::File {
                    previous: previous.take(),
                    chunks,
                })?);
            }
            let after = file.metadata()?;
            ensure!(
                size == meta.len()
                    && after.len() == meta.len()
                    && after.mtime() == meta.mtime()
                    && after.mtime_nsec() == meta.mtime_nsec(),
                "source changed while reading {}",
                path.display()
            );
            if self.retained && size != 0 {
                previous = Some(
                    self.page(&Page::BtrfsFile {
                        path: path
                            .strip_prefix(&self.root)?
                            .to_str()
                            .context("non-UTF-8 backing path")?
                            .into(),
                    })?,
                );
            }
            Node {
                logical: Logical {
                    kind: "file".into(),
                    mode,
                    size,
                    digest: hasher.finalize().to_hex().to_string(),
                    target: None,
                },
                recipe: previous,
            }
        } else if meta.is_dir() {
            ensure!(
                meta.dev() == self.device,
                "nested filesystem needs a separate capture: {}",
                path.display()
            );
            ensure!(
                !self.btrfs || depth == 0 || !matches!(meta.ino(), 2 | 256),
                "nested Btrfs subvolume needs a separate capture: {}",
                path.display()
            );
            let mut paths = fs::read_dir(path)?
                .map(|r| r.map(|e| e.path()))
                .collect::<std::io::Result<Vec<_>>>()?;
            paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
            let mut h = directory_hasher(mode);
            let mut entries = Vec::new();
            let mut previous = None;
            for child in paths {
                let is_dir = fs::symlink_metadata(&child)?.is_dir();
                if self
                    .ignore
                    .matched_path_or_any_parents(&child, is_dir)
                    .is_ignore()
                {
                    continue;
                }
                let name = child
                    .file_name()
                    .unwrap()
                    .to_str()
                    .context("non-UTF-8 names are not supported in format v1")?
                    .to_owned();
                let child_node = self.visit(&child, depth + 1)?;
                hash_entry(&mut h, &name, &child_node.logical)?;
                if self.store.is_some() {
                    entries.push(Entry {
                        name,
                        node: child_node,
                    });
                    if entries.len() == PAGE_ENTRIES {
                        previous = Some(self.page(&Page::Directory {
                            previous: previous.take(),
                            entries: std::mem::take(&mut entries),
                        })?);
                    }
                }
            }
            if !entries.is_empty() {
                previous = Some(self.page(&Page::Directory {
                    previous: previous.take(),
                    entries,
                })?);
            }
            Node {
                logical: Logical {
                    kind: "directory".into(),
                    mode,
                    size: 0,
                    digest: h.finalize().to_hex().to_string(),
                    target: None,
                },
                recipe: previous,
            }
        } else if meta.file_type().is_symlink() {
            let target = fs::read_link(path)?
                .to_str()
                .context("non-UTF-8 symlink target")?
                .to_owned();
            Node {
                logical: Logical {
                    kind: "symlink".into(),
                    mode: 0o777,
                    size: target.len() as u64,
                    digest: blake3::hash(target.as_bytes()).to_hex().to_string(),
                    target: Some(target),
                },
                recipe: None,
            }
        } else {
            bail!("unsupported special file: {}", path.display());
        };
        if self.collect {
            self.index.insert(
                path.strip_prefix(&self.root)?.to_string_lossy().to_string(),
                node.logical.clone(),
            );
        }
        Ok(node)
    }
}

pub(crate) fn directory_entries(store: &Store, keys: &Keys, node: &Node) -> Result<Vec<Entry>> {
    let mut next = node.recipe.clone();
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    while let Some(id) = next {
        ensure!(seen.insert(id.clone()), "cyclic directory recipe");
        let Page::Directory {
            previous,
            entries: part,
        } = get_page(store, keys, &id)?
        else {
            bail!("not a directory page");
        };
        entries.extend(part);
        next = previous;
        ensure!(entries.len() <= 1_000_000, "directory entry limit exceeded");
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let mut h = directory_hasher(node.logical.mode);
    let mut last: Option<&str> = None;
    for e in &entries {
        validate_node(&e.node)?;
        ensure!(
            !e.name.is_empty()
                && !matches!(e.name.as_str(), "." | "..")
                && !e.name.contains('/')
                && !e.name.contains('\0'),
            "invalid entry name"
        );
        ensure!(last != Some(&e.name), "duplicate entry name");
        last = Some(&e.name);
        hash_entry(&mut h, &e.name, &e.node.logical)?;
    }
    ensure!(
        h.finalize().to_hex().as_str() == node.logical.digest,
        "directory identity mismatch"
    );
    Ok(entries)
}

pub fn restore(store: &Store, keys: &Keys, id: &str, destination: &Path) -> Result<()> {
    crate::restore::run(
        store,
        keys,
        id,
        destination,
        crate::restore::RestoreMode::New,
        None,
    )?;
    Ok(())
}

pub fn snapshot_index(store: &Store, keys: &Keys, id: &str) -> Result<BTreeMap<String, Logical>> {
    let snap = load_snapshot(store, keys, id)?;
    index_loaded(store, keys, &snap)
}

fn index_loaded(store: &Store, keys: &Keys, snap: &Snapshot) -> Result<BTreeMap<String, Logical>> {
    fn walk(
        store: &Store,
        keys: &Keys,
        node: &Node,
        path: String,
        depth: usize,
        index: &mut BTreeMap<String, Logical>,
    ) -> Result<()> {
        ensure!(depth <= MAX_DEPTH, "directory nesting limit exceeded");
        validate_node(node)?;
        index.insert(path.clone(), node.logical.clone());
        if node.logical.kind == "directory" {
            for e in directory_entries(store, keys, node)? {
                let child = if path.is_empty() {
                    e.name
                } else {
                    format!("{path}/{}", e.name)
                };
                walk(store, keys, &e.node, child, depth + 1, index)?;
            }
        }
        Ok(())
    }
    let mut result = BTreeMap::new();
    walk(store, keys, &snap.root, String::new(), 0, &mut result)?;
    Ok(result)
}
pub fn source_index(
    keys: &Keys,
    source: &Path,
    options: &CaptureOptions,
) -> Result<BTreeMap<String, Logical>> {
    let mut builder = Builder::new(None, keys, source, options, true)?;
    builder.visit(&builder.root.clone(), 0)?;
    Ok(builder.index)
}
pub fn differences(
    before: &BTreeMap<String, Logical>,
    after: &BTreeMap<String, Logical>,
) -> Vec<(char, String)> {
    let mut result = Vec::new();
    for (path, a) in before {
        match after.get(path) {
            None => result.push(('-', path.clone())),
            // Descendant content changes already have their own rows.
            Some(b)
                if a != b
                    && (a.kind != "directory" || b.kind != "directory" || a.mode != b.mode) =>
            {
                result.push(('M', path.clone()));
            }
            _ => {}
        }
    }
    for path in after.keys() {
        if !before.contains_key(path) {
            result.push(('+', path.clone()));
        }
    }
    result.sort_by(|a, b| a.1.cmp(&b.1));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_metadata_exports_filtered_content_and_rejects_changed_bytes() -> Result<()> {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        fs::create_dir(&source)?;
        fs::create_dir(source.join("empty-dir"))?;
        fs::write(source.join("empty"), [])?;
        fs::write(source.join("ignored"), "not exported")?;
        symlink("missing", source.join("link"))?;
        fs::write(source.join("large"), vec![47u8; 1024 * 1024])?;
        for i in 0..140 {
            fs::write(source.join(format!("file-{i:03}")), format!("file {i}"))?;
        }
        let ignore = temp.path().join("ignore");
        fs::write(&ignore, "ignored\n")?;
        for secret in [[0; 32], [19; 32]] {
            let keys = Keys::from_bytes(secret);
            let store = Store::Local(temp.path().join(keys.domain()).join("retained"));
            let options = CaptureOptions {
                ignore_file: Some(ignore.clone()),
                ..Default::default()
            };
            let mut builder = Builder::new(Some(&store), &keys, &source, &options, false)?;
            builder.retained = true;
            let root = builder.visit(&builder.root.clone(), 0)?;
            let id = snapshot_id(&root.logical)?;
            let snap = Snapshot {
                version: 2,
                id: id.clone(),
                root,
                // Exercise the representation independently of the Btrfs ioctl;
                // real identity checks are covered by tests/retained_btrfs.rs.
                backing: Some(BtrfsBacking {
                    path: source.clone(),
                    subtree: PathBuf::new(),
                    filesystem_uuid: String::new(),
                    subvolume_uuid: String::new(),
                    subvolume_id: 0,
                }),
                lease: Some(Lease {
                    root: Some(File::open(&source)?),
                    _lock: None,
                }),
            };
            let encoded = keys.seal_local(
                &serde_json::to_vec(&snap)?,
                snap.root.recipe.iter().cloned().collect(),
            )?;
            assert!(store.put(&format!("snapshots/{id}"), &encoded).is_err());
            store.put_local(&format!("snapshots/{id}"), &encoded)?;
            let opaque_dest = Store::Local(temp.path().join(keys.domain()).join("opaque"));
            assert!(crate::store::replicate(&store, &opaque_dest, &id).is_err());
            assert!(opaque_dest.list("snapshots")?.is_empty());
            for entry in fs::read_dir(match &store {
                Store::Local(p) => p.join("objects"),
                _ => unreachable!(),
            })? {
                let bytes = fs::read(entry?.path())?;
                assert_eq!(
                    keys.open(&bytes)?.0[0],
                    b'M',
                    "retention must not store payload chunks"
                );
            }
            verify_loaded(&store, &keys, &snap)?;
            let ordinary = Store::Local(temp.path().join(keys.domain()).join("ordinary"));
            assert_eq!(capture(&ordinary, &keys, &source, &options)?, id);
            for chunker in [Chunker::Fixed, Chunker::FastCdc] {
                let destination = Store::Local(
                    temp.path()
                        .join(keys.domain())
                        .join(format!("export-{chunker:?}")),
                );
                let conversion = CaptureOptions {
                    chunker,
                    chunk_size: 4096,
                    ignore_file: None,
                };
                let (exported, stats) =
                    export_tree(&store, &destination, &keys, &snap, &conversion)?;
                assert!(stats.objects_copied > 0);
                let refs = exported.root.recipe.iter().cloned().collect();
                destination.put(
                    &format!("snapshots/{id}"),
                    &keys.seal(&serde_json::to_vec(&exported)?, refs)?,
                )?;
                verify(&destination, &keys, &id)?;
                let restored = temp
                    .path()
                    .join(keys.domain())
                    .join(format!("restored-{chunker:?}"));
                restore(&destination, &keys, &id, &restored)?;
                assert!(!restored.join("ignored").exists());
                assert_eq!(
                    fs::read(restored.join("large"))?,
                    fs::read(source.join("large"))?
                );
                assert_eq!(fs::read_link(restored.join("link"))?, Path::new("missing"));
                assert_eq!(
                    snapshot_index(&ordinary, &keys, &id)?,
                    snapshot_index(&destination, &keys, &id)?
                );
            }
            fs::write(source.join("large"), vec![48u8; 1024 * 1024])?;
            assert!(verify_loaded(&store, &keys, &snap).is_err());
            let failed = Store::Local(temp.path().join(keys.domain()).join("failed"));
            assert!(export_tree(&store, &failed, &keys, &snap, &options).is_err());
            assert!(failed.list("snapshots")?.is_empty());
            fs::write(source.join("large"), vec![47u8; 1024 * 1024])?;
        }
        Ok(())
    }

    #[test]
    fn readers_reject_inconsistent_leaf_metadata_even_with_valid_object_hashes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let keys = Keys::from_bytes([37; 32]);
        let store = Store::Local(temp.path().join("repo"));
        let empty = Node {
            logical: Logical {
                kind: "file".into(),
                mode: 0o600,
                size: 0,
                digest: blake3::hash(b"").to_hex().to_string(),
                target: None,
            },
            recipe: None,
        };
        let mut inconsistent = Vec::new();
        let mut node = empty.clone();
        node.logical.kind = "unknown".into();
        inconsistent.push(node);
        let mut node = empty.clone();
        node.logical.target = Some("unexpected".into());
        inconsistent.push(node);
        let mut node = empty.clone();
        node.logical.mode = 0o4600;
        inconsistent.push(node);
        let mut node = empty.clone();
        node.logical.size = 1;
        inconsistent.push(node);
        let mut node = empty;
        node.logical.kind = "symlink".into();
        node.logical.mode = 0o777;
        node.logical.target = Some("target".into());
        node.logical.size = 6;
        // A valid directory hash cannot make an inconsistent symlink valid.
        inconsistent.push(node);
        for (i, node) in inconsistent.into_iter().enumerate() {
            let mut h = directory_hasher(0o700);
            hash_entry(&mut h, "leaf", &node.logical)?;
            let page = Page::Directory {
                previous: None,
                entries: vec![Entry {
                    name: "leaf".into(),
                    node,
                }],
            };
            let mut bytes = vec![b'M'];
            bytes.extend(serde_json::to_vec(&page)?);
            let recipe = keys.object_id(&bytes);
            store.put(
                &format!("objects/{recipe}"),
                &keys.seal(&bytes, page.refs())?,
            )?;
            let root = Node {
                logical: Logical {
                    kind: "directory".into(),
                    mode: 0o700,
                    size: 0,
                    digest: h.finalize().to_hex().to_string(),
                    target: None,
                },
                recipe: Some(recipe.clone()),
            };
            let id = snapshot_id(&root.logical)?;
            let snapshot = Snapshot {
                version: 1,
                id: id.clone(),
                root,
                backing: None,
                lease: None,
            };
            store.put(
                &format!("snapshots/{id}"),
                &keys.seal(&serde_json::to_vec(&snapshot)?, vec![recipe])?,
            )?;
            assert!(verify(&store, &keys, &id).is_err(), "case {i}");
            assert!(snapshot_index(&store, &keys, &id).is_err(), "case {i}");
            let destination = temp.path().join(format!("restore-{i}"));
            assert!(
                restore(&store, &keys, &id, &destination).is_err(),
                "case {i}"
            );
            assert!(!destination.exists());
        }
        Ok(())
    }
}
