//! Real process termination at deterministic HTTP boundaries.
use anyhow::{Result, ensure};
use cairn::{
    objects::Keys,
    snapshot::{self, CaptureOptions},
    store::{Import, Store},
};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn server(root: &Path) -> Result<(Process, String)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    drop(listener);
    let child = Process(
        Command::new(env!("CARGO_BIN_EXE_cairn"))
            .arg("--repo")
            .arg(root)
            .args(["serve", "--listen", &address.to_string()])
            .env_remove("CAIRN_TOKEN")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    for _ in 0..500 {
        if TcpStream::connect(address).is_ok() {
            return Ok((child, format!("http://{address}")));
        }
        thread::sleep(Duration::from_millis(10));
    }
    anyhow::bail!("server failed to start")
}

struct Gate {
    url: String,
    reached: mpsc::Receiver<()>,
    resume: mpsc::Sender<()>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Gate {
    fn new(upstream: String, method_to_hold: &'static str) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}", listener.local_addr()?);
        listener.set_nonblocking(true)?;
        let (signal, reached) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = thread::spawn(move || {
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap();
            let mut count = 0;
            while !stopping.load(Ordering::Relaxed) {
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(_) => break,
                };
                let handle = || -> Result<()> {
                    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    reader.read_line(&mut line)?;
                    let parts: Vec<_> = line.split_whitespace().collect();
                    ensure!(parts.len() >= 2, "missing request");
                    let method = parts[0].to_owned();
                    let path = parts[1].to_owned();
                    let mut length = 0usize;
                    loop {
                        let mut header = String::new();
                        ensure!(reader.read_line(&mut header)? > 0, "truncated request");
                        if header == "\r\n" {
                            break;
                        }
                        if let Some((name, value)) = header.split_once(':')
                            && name.eq_ignore_ascii_case("content-length")
                        {
                            length = value.trim().parse()?;
                        }
                    }
                    ensure!(length <= 5 * 1024 * 1024, "test request too large");
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body)?;
                    let response = client
                        .request(method.parse()?, format!("{upstream}{path}"))
                        .body(body)
                        .send()?;
                    let status = response.status().as_u16();
                    let bytes = response.bytes()?;
                    if method == method_to_hold && path.contains("/objects/") {
                        count += 1;
                        if count == 2 {
                            signal.send(())?;
                            resumed.recv_timeout(Duration::from_secs(15))?;
                        }
                    }
                    let stream = reader.get_mut();
                    write!(
                        stream,
                        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    )?;
                    stream.write_all(&bytes)?;
                    Ok(())
                };
                // Disconnection is expected when a process is killed at the gate.
                let _ = handle();
            }
        });
        Ok(Self {
            url,
            reached,
            resume,
            stop,
            worker: Some(worker),
        })
    }
    fn wait(&self) -> Result<()> {
        Ok(self.reached.recv_timeout(Duration::from_secs(10))?)
    }
    fn release(&self) -> Result<()> {
        Ok(self.resume.send(())?)
    }
}
impl Drop for Gate {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.resume.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn fixture(root: &Path) -> Result<PathBuf> {
    let source = root.join("source");
    fs::create_dir(&source)?;
    for i in 0..6 {
        fs::write(source.join(format!("file-{i}")), vec![i; 9000])?;
    }
    Ok(source)
}
fn saved(root: &Path) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    Ok(fs::read_dir(root.join("objects"))?
        .map(|entry| {
            let path = entry?.path();
            Ok((path.clone(), fs::read(path)?))
        })
        .collect::<std::io::Result<_>>()?)
}

#[test]
fn killed_upload_reuses_committed_chunks_on_retry() -> Result<()> {
    for chunker in ["fixed", "fastcdc"] {
        killed_upload(chunker)?;
    }
    Ok(())
}

fn killed_upload(chunker: &str) -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = fixture(temp.path())?;
    let root = temp.path().join("server");
    let (_server, url) = server(&root)?;
    let gate = Gate::new(url, "PUT")?;
    let keys = Keys::from_bytes([51; 32]);
    let key = temp.path().join("key");
    fs::write(&key, hex::encode([51; 32]))?;
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cairn"));
        command
            .args(["--repo", &gate.url])
            .arg("--key-file")
            .arg(&key)
            .arg("capture")
            .arg(&source)
            .args(["--live", "--tag", "complete"])
            .args(["--chunker", chunker, "--chunk-size", "4096"])
            .env_remove("CAIRN_TOKEN");
        command
    };
    let mut child = Process(command().stdout(Stdio::null()).spawn()?);
    gate.wait()?;
    child.0.kill()?;
    child.0.wait()?;
    gate.release()?;
    let local = Store::Local(root.join(keys.domain()));
    assert!(local.list("snapshots")?.is_empty());
    assert!(local.list("tags")?.is_empty());
    let before = saved(&root.join(keys.domain()))?;
    assert_eq!(before.len(), 2);
    let result = command().output()?;
    ensure!(
        result.status.success(),
        "retry failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let id = String::from_utf8(result.stdout)?.trim().to_owned();
    for (path, bytes) in before {
        assert_eq!(
            fs::read(path)?,
            bytes,
            "retry re-encrypted an existing chunk"
        );
    }
    assert_eq!(local.resolve("complete")?, id);
    snapshot::verify(&local, &keys, &id)?;
    snapshot::restore(&local, &keys, &id, &temp.path().join("restored"))?;
    Ok(())
}

#[test]
fn killed_destination_server_resumes_keyless_import_after_restart() -> Result<()> {
    for chunker in [
        cairn::chunking::Chunker::Fixed,
        cairn::chunking::Chunker::FastCdc,
    ] {
        killed_destination(chunker)?;
    }
    Ok(())
}

fn killed_destination(chunker: cairn::chunking::Chunker) -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = fixture(temp.path())?;
    let keys = Keys::from_bytes([52; 32]);
    let root_a = temp.path().join("a");
    let root_b = temp.path().join("b");
    let a = Store::Local(root_a.join(keys.domain()));
    let id = snapshot::capture(
        &a,
        &keys,
        &source,
        &CaptureOptions {
            chunker,
            chunk_size: 4096,
            ..Default::default()
        },
    )?;
    let (_a, url_a) = server(&root_a)?;
    let (mut b, url_b) = server(&root_b)?;
    let gate = Gate::new(url_a, "GET")?;
    let request = Import {
        source: gate.url.clone(),
        snapshot: id.clone(),
        source_token: None,
    };
    let destination = Store::connect(&url_b, &keys.domain(), None)?;
    let first = thread::spawn(move || destination.remote_import(&request));
    gate.wait()?;
    b.0.kill()?;
    b.0.wait()?;
    gate.release()?;
    assert!(first.join().unwrap().is_err());
    let local_b = Store::Local(root_b.join(keys.domain()));
    assert!(local_b.list("snapshots")?.is_empty());
    let before = saved(&root_b.join(keys.domain()))?;
    assert_eq!(before.len(), 1);
    let (_restarted, url_b) = server(&root_b)?;
    let destination = Store::connect(&url_b, &keys.domain(), None)?;
    let stats = destination.remote_import(&Import {
        source: gate.url.clone(),
        snapshot: id.clone(),
        source_token: None,
    })?;
    assert!(stats.objects_present >= 1);
    assert!(stats.objects_copied > 0);
    for (path, bytes) in before {
        assert_eq!(fs::read(path)?, bytes);
    }
    assert_eq!(
        destination
            .remote_import(&Import {
                source: gate.url.clone(),
                snapshot: id.clone(),
                source_token: None
            })?
            .bytes_copied,
        0
    );
    snapshot::verify(&destination, &keys, &id)?;
    snapshot::restore(&destination, &keys, &id, &temp.path().join("restored"))?;
    Ok(())
}
