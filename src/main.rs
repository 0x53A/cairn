use anyhow::{Context, Result, ensure};
use cairn::{
    capture::Source,
    chunking::Chunker,
    objects::Keys,
    snapshot::{self, CaptureOptions},
    store::{Import, Store, replicate},
};
use clap::{Parser, Subcommand};
use rand::RngCore;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    net::SocketAddr,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

#[derive(Parser)]
#[command(
    version,
    about = "Independent content-addressed snapshots (experimental format)"
)]
struct Cli {
    /// Local directory, http(s)://server, unix:///socket, ssh:// or ssh-openssh://[user@]host[:port]/socket.
    #[arg(long, global = true, default_value = "./cairn-store")]
    repo: String,
    /// File containing a 32-byte hexadecimal key. Omit, or use all-zero key, for plaintext.
    #[arg(long, global = true)]
    key_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    /// Create a private encryption key file (never overwrite).
    Keygen {
        path: PathBuf,
    },
    /// Show the key domain used to address an encrypted repository.
    Domain,
    /// Capture through a temporary read-only Btrfs snapshot by default.
    Capture {
        source: PathBuf,
        #[arg(long)]
        tag: Vec<String>,
        /// Read files directly on any supported filesystem; no atomic directory view.
        #[arg(long, conflicts_with = "snapshot_dir")]
        live: bool,
        /// Existing writable directory on the same Btrfs filesystem.
        #[arg(long)]
        snapshot_dir: Option<PathBuf>,
        #[arg(long)]
        ignore_file: Option<PathBuf>,
        /// Storage splitter; logical snapshot IDs do not depend on this choice.
        #[arg(long, value_enum, default_value = "fixed")]
        chunker: Chunker,
        /// Fixed block size, or FastCDC target (power of two, 4 KiB–1 MiB).
        #[arg(long,default_value_t=snapshot::DEFAULT_CHUNK)]
        chunk_size: usize,
    },
    /// Restore into a directory which must not exist.
    Restore {
        snapshot: String,
        destination: PathBuf,
    },
    /// Authenticate every reachable object without restoring files.
    Verify {
        snapshot: String,
    },
    /// Remove abandoned Cairn Btrfs snapshots; active captures are skipped.
    CleanupSnapshots {
        #[arg(long)]
        snapshot_dir: PathBuf,
    },
    /// Compare snapshots. Prints M, +, -; content is not downloaded.
    Diff {
        before: String,
        after: String,
    },
    /// Compare against a source without inserting it into the object store.
    DiffSource {
        snapshot: String,
        source: PathBuf,
        /// Read files directly on any supported filesystem; no atomic directory view.
        #[arg(long, conflicts_with = "snapshot_dir")]
        live: bool,
        #[arg(long)]
        snapshot_dir: Option<PathBuf>,
        #[arg(long)]
        ignore_file: Option<PathBuf>,
    },
    List,
    Tags,
    /// Add a unique tag; collisions fail. Tags never change snapshot IDs.
    Tag {
        snapshot: String,
        name: String,
    },
    /// Copy a snapshot. Remote destinations pull HTTP(S) sources directly; otherwise the CLI relays.
    Copy {
        snapshot: String,
        #[arg(long)]
        to: String,
        /// Opaque-copy domain; allows replication without an encryption key.
        #[arg(long)]
        domain: Option<String>,
        #[arg(long)]
        tag: Vec<String>,
    },
    /// Serve repository data. This process needs no E2E key.
    Serve {
        #[arg(long, conflicts_with = "unix_socket")]
        listen: Option<SocketAddr>,
        /// Serve HTTP on a Unix socket instead of TCP. Parent directory must exist.
        #[arg(long)]
        unix_socket: Option<PathBuf>,
    },
}
fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let cli = Cli::parse();
    if let Commands::CleanupSnapshots { snapshot_dir } = &cli.command {
        let stats = cairn::capture::cleanup_abandoned(snapshot_dir)?;
        println!("removed {}, active {}", stats.removed, stats.active);
        return Ok(());
    }
    let token = std::env::var("CAIRN_TOKEN").ok();
    if let Commands::Serve {
        listen,
        unix_socket,
    } = cli.command
    {
        ensure!(!cli.repo.contains("://"), "server repository must be local");
        let runtime = tokio::runtime::Runtime::new()?;
        if let Some(path) = unix_socket {
            return runtime.block_on(cairn::server::serve_unix(
                PathBuf::from(cli.repo),
                &path,
                token,
            ));
        }
        return runtime.block_on(cairn::server::serve(
            PathBuf::from(cli.repo),
            listen.unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 7443))),
            token,
        ));
    }
    if let Commands::Keygen { path } = cli.command {
        let mut key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut key);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        writeln!(file, "{}", hex::encode(key))?;
        file.sync_all()?;
        println!("{}", path.display());
        return Ok(());
    }
    let keys = Keys::from_file(cli.key_file.as_deref())?;
    let domain = match &cli.command {
        Commands::Copy {
            domain: Some(d), ..
        } => d.clone(),
        _ => keys.domain(),
    };
    let store = Store::connect(&cli.repo, &domain, token)?;
    match cli.command {
        Commands::Domain => println!("{}", keys.domain()),
        Commands::Capture {
            source,
            tag,
            live,
            snapshot_dir,
            ignore_file,
            chunk_size,
            chunker,
        } => {
            chunker.validate(chunk_size)?;
            // Prevent a local repository from recursively capturing itself.
            if let Store::Local(root) = &store {
                let source = fs::canonicalize(&source)?;
                let repo = absolute_existing_prefix(root)?;
                ensure!(
                    !repo.starts_with(&source),
                    "repository must be outside the captured directory"
                );
            }
            let source = Source::open(&source, live, snapshot_dir.as_deref())?;
            let id = snapshot::capture(
                &store,
                &keys,
                &source.path,
                &CaptureOptions {
                    chunker,
                    chunk_size,
                    ignore_file,
                },
            )?;
            println!("{id}");
            for name in tag {
                store.tag(&name, &id)?;
            }
            source.finish()?;
        }
        Commands::Restore {
            snapshot,
            destination,
        } => snapshot::restore(&store, &keys, &store.resolve(&snapshot)?, &destination)?,
        Commands::Verify { snapshot } => {
            let count = snapshot::verify(&store, &keys, &store.resolve(&snapshot)?)?;
            println!("verified {count} objects");
        }
        Commands::Diff { before, after } => {
            let a = snapshot::snapshot_index(&store, &keys, &store.resolve(&before)?)?;
            let b = snapshot::snapshot_index(&store, &keys, &store.resolve(&after)?)?;
            print_diff(snapshot::differences(&a, &b))?;
        }
        Commands::DiffSource {
            snapshot,
            source,
            live,
            snapshot_dir,
            ignore_file,
        } => {
            let a = snapshot::snapshot_index(&store, &keys, &store.resolve(&snapshot)?)?;
            let source = Source::open(&source, live, snapshot_dir.as_deref())?;
            let b = snapshot::source_index(
                &keys,
                &source.path,
                &CaptureOptions {
                    ignore_file,
                    ..Default::default()
                },
            )?;
            print_diff(snapshot::differences(&a, &b))?;
            source.finish()?;
        }
        Commands::List => {
            for id in store.list("snapshots")? {
                println!("{id}");
            }
        }
        Commands::Tags => {
            for tag in store.list("tags")? {
                let name = String::from_utf8(hex::decode(&tag)?)?;
                let id = String::from_utf8(store.get(&format!("tags/{tag}"))?)?;
                println!("{}\t{id}", serde_json::to_string(&name)?);
            }
        }
        Commands::Tag { snapshot, name } => store.tag(&name, &store.resolve(&snapshot)?)?,
        Commands::Copy {
            snapshot, to, tag, ..
        } => {
            let id = store.resolve(&snapshot)?;
            let destination = Store::connect(&to, &domain, std::env::var("CAIRN_DEST_TOKEN").ok())?;
            // Unix source paths belong to this client, not the destination host.
            let stats = if (cli.repo.starts_with("http://") || cli.repo.starts_with("https://"))
                && matches!(
                    (&store, &destination),
                    (Store::Http { .. }, Store::Http { .. } | Store::Ssh(_))
                ) {
                destination.remote_import(&Import {
                    source: cli.repo,
                    snapshot: id.clone(),
                    source_token: std::env::var("CAIRN_TOKEN").ok(),
                })?
            } else {
                replicate(&store, &destination, &id)?
            };
            for name in tag {
                destination.tag(&name, &id)?;
            }
            println!("{}", serde_json::to_string(&stats)?);
        }
        Commands::Keygen { .. } | Commands::Serve { .. } | Commands::CleanupSnapshots { .. } => {
            unreachable!()
        }
    }
    Ok(())
}
fn print_diff(changes: Vec<(char, String)>) -> Result<()> {
    for (kind, path) in changes {
        println!("{kind}\t{}", serde_json::to_string(&path)?);
    }
    Ok(())
}
fn absolute_existing_prefix(path: &std::path::Path) -> Result<PathBuf> {
    if path.exists() {
        return Ok(fs::canonicalize(path)?);
    }
    let parent = path.parent().context("repository has no parent")?;
    let base = if parent.as_os_str().is_empty() {
        std::env::current_dir()?
    } else {
        absolute_existing_prefix(parent)?
    };
    Ok(base.join(path.file_name().context("repository has no filename")?))
}
