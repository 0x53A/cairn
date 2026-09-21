use crate::{
    chunking::Chunker,
    objects::{Keys, valid_id},
    store::Store,
};
use anyhow::{Context, Result, bail, ensure};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
};

const PAGE_ENTRIES: usize = 128;
pub use crate::chunking::{DEFAULT_CHUNK, MAX_CHUNK};
const MAX_DEPTH: usize = 256;

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

pub fn load_snapshot(store: &Store, keys: &Keys, id: &str) -> Result<Snapshot> {
    ensure!(valid_id(id), "invalid snapshot ID");
    let (bytes, refs) = keys.open(&store.get(&format!("snapshots/{id}"))?)?;
    let snap: Snapshot = serde_json::from_slice(&bytes)?;
    ensure!(
        snap.version == 1 && snap.id == id && snapshot_id(&snap.root.logical)? == id,
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
    Ok(snap)
}

/// Authenticate every reachable object without expanding files to disk.
/// Restore additionally verifies each reconstructed whole-file digest.
pub fn verify(store: &Store, keys: &Keys, id: &str) -> Result<usize> {
    let snapshot = load_snapshot(store, keys, id)?;
    // Validate directory identities and names as well as the opaque graph.
    snapshot_index(store, keys, id)?;
    let mut pending: Vec<_> = snapshot.root.recipe.into_iter().collect();
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
                get_page(store, keys, &id)?;
            }
            Some(b'C') => ensure!(refs.is_empty(), "chunk has references"),
            _ => bail!("invalid object type: {id}"),
        }
        pending.extend(refs);
    }
    Ok(seen.len())
}

fn get_object(store: &Store, keys: &Keys, id: &str) -> Result<(Vec<u8>, Vec<String>)> {
    let (payload, refs) = keys.open(&store.get(&format!("objects/{id}"))?)?;
    ensure!(
        keys.object_id(&payload) == id,
        "object content hash mismatch: {id}"
    );
    Ok((payload, refs))
}
fn get_page(store: &Store, keys: &Keys, id: &str) -> Result<Page> {
    let (bytes, refs) = get_object(store, keys, id)?;
    ensure!(bytes.first() == Some(&b'M'), "not a metadata object");
    let page: Page = serde_json::from_slice(&bytes[1..])?;
    ensure!(page.refs() == refs, "metadata references mismatch");
    match &page {
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
    };
    store.put(
        &format!("snapshots/{id}"),
        &keys.seal(&serde_json::to_vec(&snap)?, refs)?,
    )?;
    Ok(id)
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
        let btrfs = std::process::Command::new("stat")
            .args(["-f", "-c", "%T"])
            .arg(&root)
            .output()
            .is_ok_and(|o| {
                o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "btrfs"
            });
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
        self.object(&payload, page.refs())
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
            let read_mode = if self.store.is_some() {
                self.chunker
            } else {
                Chunker::Fixed
            };
            for buffer in read_mode.chunks_for_file(&mut file, self.chunk_size, Some(meta.len()))? {
                let buffer = buffer?;
                let n = buffer.len();
                hasher.update(&buffer);
                if self.store.is_some() {
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

fn directory_entries(store: &Store, keys: &Keys, node: &Node) -> Result<Vec<Entry>> {
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
    let snapshot = load_snapshot(store, keys, id)?;
    ensure!(
        !destination.try_exists()? && fs::symlink_metadata(destination).is_err(),
        "destination must not exist"
    );
    restore_node(store, keys, &snapshot.root, destination, 0)?;
    if let Some(parent) = destination.parent().filter(|p| !p.as_os_str().is_empty()) {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}
fn restore_node(store: &Store, keys: &Keys, node: &Node, path: &Path, depth: usize) -> Result<()> {
    ensure!(depth <= MAX_DEPTH, "directory nesting limit exceeded");
    ensure!(node.logical.mode <= 0o777, "unsupported permission bits");
    match node.logical.kind.as_str() {
        "directory" => {
            let entries = directory_entries(store, keys, node)?;
            fs::create_dir(path)?;
            for e in entries {
                restore_node(store, keys, &e.node, &path.join(e.name), depth + 1)?;
            }
            let dir = File::open(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(node.logical.mode))?;
            dir.sync_all()?;
        }
        "file" => {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(path)?;
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
                        c.len > 0
                            && c.len <= MAX_CHUNK
                            && c.offset.checked_add(c.len as u64) == Some(end),
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
            file.seek(SeekFrom::Start(0))?;
            let mut hasher = blake3::Hasher::new();
            let mut buffer = [0; 64 * 1024];
            loop {
                let n = file.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buffer[..n]);
            }
            ensure!(
                hasher.finalize().to_hex().as_str() == node.logical.digest,
                "restored file hash mismatch"
            );
            file.set_permissions(fs::Permissions::from_mode(node.logical.mode))?;
            file.sync_all()?;
        }
        "symlink" => {
            let target = node
                .logical
                .target
                .as_ref()
                .context("missing symlink target")?;
            ensure!(
                blake3::hash(target.as_bytes()).to_hex().as_str() == node.logical.digest
                    && target.len() as u64 == node.logical.size,
                "symlink identity mismatch"
            );
            symlink(target, path)?;
        }
        _ => bail!("unsupported entry kind"),
    }
    Ok(())
}

pub fn snapshot_index(store: &Store, keys: &Keys, id: &str) -> Result<BTreeMap<String, Logical>> {
    let snap = load_snapshot(store, keys, id)?;
    fn walk(
        store: &Store,
        keys: &Keys,
        node: &Node,
        path: String,
        depth: usize,
        index: &mut BTreeMap<String, Logical>,
    ) -> Result<()> {
        ensure!(depth <= MAX_DEPTH, "directory nesting limit exceeded");
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
