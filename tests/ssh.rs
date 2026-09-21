//! Explicit loopback OpenSSH integration using disposable keys for the current user.
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::Duration,
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn executable(name: &str) -> Result<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").context("PATH")?)
        .map(|p| p.join(name))
        .find(|p| p.is_file())
        .context("OpenSSH tools must be installed")
}
fn success(output: Output) -> Result<String> {
    ensure!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}
fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[test]
#[ignore = "starts an isolated loopback sshd; requires OpenSSH tools"]
fn real_ssh_encrypted_roundtrip_and_tunnel_cleanup() -> Result<()> {
    roundtrip(false)?;
    roundtrip(true)
}

fn roundtrip(native: bool) -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let ssh = executable("ssh")?;
    let sshd = executable("sshd")?;
    for name in ["host", "identity"] {
        success(
            Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(root.join(name))
                .output()?,
        )?;
    }
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let user = success(Command::new("id").arg("-un").output()?)?
        .trim()
        .to_owned();
    let config = root.join("sshd_config");
    fs::write(
        &config,
        format!(
            "ListenAddress 127.0.0.1\nPort {port}\nHostKey {}/host\nAuthorizedKeysFile {}/identity.pub\nPidFile {}/sshd.pid\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nStrictModes no\nAllowUsers {user}\nAllowStreamLocalForwarding yes\n",
            root.display(),
            root.display(),
            root.display()
        ),
    )?;
    let log = root.join("sshd.log");
    let mut daemon = Process(
        Command::new(sshd)
            .arg("-D")
            .arg("-f")
            .arg(&config)
            .arg("-E")
            .arg(&log)
            .spawn()?,
    );
    for _ in 0..200 {
        if let Some(status) = daemon.0.try_wait()? {
            anyhow::bail!("sshd {status}: {}", fs::read_to_string(&log)?);
        }
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let known_hosts = root.join("known_hosts");
    fs::write(
        &known_hosts,
        format!(
            "cairn-test,[127.0.0.1]:{port} {}",
            fs::read_to_string(root.join("host.pub"))?
        ),
    )?;
    let client_config = root.join("ssh_config");
    fs::write(
        &client_config,
        format!(
            "Host cairn-test\n HostName 127.0.0.1\n User {user}\n Port {port}\n IdentityFile {}/identity\n IdentitiesOnly yes\n BatchMode yes\n HostKeyAlias cairn-test\n StrictHostKeyChecking yes\n UserKnownHostsFile {}\n GlobalKnownHostsFile /dev/null\n",
            root.display(),
            known_hosts.display()
        ),
    )?;
    let bin = root.join("bin");
    fs::create_dir(&bin)?;
    let wrapper = bin.join("ssh");
    let pidfile = root.join("tunnel.pid");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\necho $$ > {}\nexec {} -F {} \"$@\"\n",
            quote(&pidfile),
            quote(&ssh),
            quote(&client_config)
        ),
    )?;
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700))?;
    let socket = root.join("server.sock");
    let _server = Process(
        Command::new(env!("CARGO_BIN_EXE_cairn"))
            .arg("--repo")
            .arg(root.join("repo"))
            .args(["serve", "--unix-socket"])
            .arg(&socket)
            .env_remove("CAIRN_TOKEN")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    for _ in 0..200 {
        if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let source = root.join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("data"), vec![71; 1024 * 1024 + 17])?;
    let key = root.join("key");
    fs::write(&key, hex::encode([12; 32]))?;
    let location = if native {
        format!("ssh://{user}@127.0.0.1:{port}{}", socket.display())
    } else {
        format!("ssh-openssh://cairn-test{}", socket.display())
    };
    let run = |args: &[&str]| -> Result<String> {
        let result = success(
            Command::new(env!("CARGO_BIN_EXE_cairn"))
                .args(["--repo", &location, "--key-file"])
                .arg(&key)
                .args(args)
                .env("PATH", if native { Path::new("") } else { &bin })
                .env("CAIRN_SSH_IDENTITY", root.join("identity"))
                .env("CAIRN_SSH_KNOWN_HOSTS", &known_hosts)
                .env_remove("CAIRN_TOKEN")
                .env_remove("CAIRN_DEST_TOKEN")
                .output()?,
        )?;
        if native {
            assert!(!pidfile.exists());
        } else {
            let pid = fs::read_to_string(&pidfile)?;
            assert!(
                !Path::new("/proc").join(pid.trim()).exists(),
                "SSH child must be reaped before CLI exits"
            );
        }
        Ok(result)
    };
    run(&[
        "capture",
        source.to_str().unwrap(),
        "--live",
        "--chunker",
        "fastcdc",
        "--tag",
        "first",
    ])?;
    run(&["verify", "first"])?;
    let restored = root.join("restored");
    run(&["restore", "first", restored.to_str().unwrap()])?;
    assert_eq!(
        fs::read(source.join("data"))?,
        fs::read(restored.join("data"))?
    );
    let copy = format!("unix://{}", socket.display());
    run(&["copy", "first", "--to", &copy])?;
    if native {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        drop(listener);
        let _http = Process(
            Command::new(env!("CARGO_BIN_EXE_cairn"))
                .arg("--repo")
                .arg(root.join("repo"))
                .args(["serve", "--listen", &address.to_string()])
                .env_remove("CAIRN_TOKEN")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        for _ in 0..200 {
            if std::net::TcpStream::connect(address).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        success(
            Command::new(env!("CARGO_BIN_EXE_cairn"))
                .args(["--repo", &format!("http://{address}"), "--key-file"])
                .arg(&key)
                .args(["copy", "first", "--to", &location])
                .env("PATH", "")
                .env("CAIRN_SSH_IDENTITY", root.join("identity"))
                .env("CAIRN_SSH_KNOWN_HOSTS", &known_hosts)
                .env_remove("CAIRN_TOKEN")
                .env_remove("CAIRN_DEST_TOKEN")
                .output()?,
        )?;
        let agent_socket = root.join("agent.sock");
        let _agent = Process(
            Command::new(executable("ssh-agent")?)
                .args(["-D", "-a"])
                .arg(&agent_socket)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        for _ in 0..200 {
            if std::os::unix::net::UnixStream::connect(&agent_socket).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        success(
            Command::new("ssh-add")
                .arg(root.join("identity"))
                .env("SSH_AUTH_SOCK", &agent_socket)
                .output()?,
        )?;
        success(
            Command::new(env!("CARGO_BIN_EXE_cairn"))
                .args(["--repo", &location, "--key-file"])
                .arg(&key)
                .args(["verify", "first"])
                .env("PATH", "")
                .env_remove("CAIRN_SSH_IDENTITY")
                .env_remove("CAIRN_TOKEN")
                .env("CAIRN_SSH_KNOWN_HOSTS", &known_hosts)
                .env("SSH_AUTH_SOCK", &agent_socket)
                .output()?,
        )?;
        assert!(!pidfile.exists());
    }
    // Both an application error and failed SSH host verification must reap the
    // child. The latter must never silently accept an unrecognized host key.
    for unknown_host in [false, true] {
        if unknown_host {
            fs::write(&known_hosts, "")?;
        }
        let failed = Command::new(env!("CARGO_BIN_EXE_cairn"))
            .args(["--repo", &location, "--key-file"])
            .arg(&key)
            .args(["verify", if unknown_host { "first" } else { "missing" }])
            .env("PATH", if native { Path::new("") } else { &bin })
            .env("CAIRN_SSH_IDENTITY", root.join("identity"))
            .env("CAIRN_SSH_KNOWN_HOSTS", &known_hosts)
            .env_remove("CAIRN_TOKEN")
            .output()?;
        assert!(!failed.status.success());
        if unknown_host && !native {
            assert!(
                String::from_utf8_lossy(&failed.stderr).contains("Host key verification failed")
            );
        }
        if native {
            assert!(!pidfile.exists());
        } else {
            let pid = fs::read_to_string(&pidfile)?;
            assert!(!Path::new("/proc").join(pid.trim()).exists());
        }
    }
    Ok(())
}
