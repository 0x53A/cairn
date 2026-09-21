//! Client-owned OpenSSH forwarding to a remote Cairn Unix socket.
use anyhow::{Context, Result, bail, ensure};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub struct Tunnel {
    child: Child,
    directory: tempfile::TempDir,
}

pub(crate) struct Address {
    pub host: String,
    pub user: String,
    pub port: Option<u16>,
    pub socket: String,
}

impl Address {
    pub(crate) fn parse(location: &str) -> Result<Self> {
        let location = location
            .strip_prefix("ssh-openssh://")
            .map(|rest| format!("ssh://{rest}"))
            .unwrap_or_else(|| location.to_owned());
        // Reject ambiguous forwarding syntax and URL transformations. Paths are
        // literal, absolute Unix paths, as with the unix:// transport.
        ensure!(
            !location
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
                && !location.contains(['%', '\\', '?', '#']),
            "SSH address must use a literal path without whitespace, %, backslash, query or fragment"
        );
        let url = reqwest::Url::parse(&location)?;
        ensure!(
            url.scheme() == "ssh" && url.password().is_none(),
            "expected ssh://[user@]host[:port]/absolute/socket (no password)"
        );
        let host = url.host_str().context("SSH host is required")?;
        ensure!(!host.starts_with('-'), "invalid SSH host");
        let (_, socket) = location
            .strip_prefix("ssh://")
            .context("expected ssh://")?
            .split_once('/')
            .context("remote Unix socket path is required")?;
        ensure!(
            !socket.is_empty() && !socket.contains([':', '[', ']']),
            "invalid remote Unix socket path"
        );
        Ok(Self {
            host: host.trim_start_matches('[').trim_end_matches(']').into(),
            user: url.username().into(),
            port: url.port(),
            socket: format!("/{socket}"),
        })
    }
}

impl Tunnel {
    pub fn start(location: &str) -> Result<Self> {
        let address = Address::parse(location)?;
        // A short private directory also avoids sockaddr_un length limits and
        // forwarding delimiters in an arbitrary TMPDIR.
        let directory = tempfile::Builder::new()
            .prefix("cairn-ssh-")
            .tempdir_in("/tmp")?;
        let socket = directory.path().join("socket");
        let mut command = Command::new("ssh");
        command.args([
            "-N",
            "-T",
            "-a",
            "-x",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "-o",
            "ControlPersist=no",
            "-o",
            "ForkAfterAuthentication=no",
            "-o",
            "PermitLocalCommand=no",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
        ]);
        if let Some(port) = address.port {
            command.arg("-p").arg(port.to_string());
        }
        if !address.user.is_empty() {
            command.arg("-l").arg(&address.user);
        }
        let child = command
            .arg("-L")
            .arg(format!("{}:{}", socket.display(), address.socket))
            .arg("--")
            .arg(&address.host)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .context("start OpenSSH (ssh must be installed)")?;
        let mut tunnel = Self { child, directory };
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Some(status) = tunnel.child.try_wait()? {
                bail!("SSH tunnel exited with {status}; see SSH diagnostics above");
            }
            if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                return Ok(tunnel);
            }
            ensure!(
                Instant::now() < deadline,
                "SSH tunnel did not become ready within 120 seconds"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.directory.path().join(Path::new("socket"))
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // TempDir subsequently removes our socket and private directory.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_alias_user_port_ipv6_and_literal_path() -> Result<()> {
        let a = Address::parse("ssh://lukas@mini:2222/run/user/1000/cairn.sock")?;
        assert_eq!(
            (a.host.as_str(), a.user.as_str(), a.port, a.socket.as_str()),
            ("mini", "lukas", Some(2222), "/run/user/1000/cairn.sock")
        );
        assert_eq!(Address::parse("ssh://[::1]/tmp/cairn.sock")?.host, "::1");
        assert_eq!(
            Address::parse("ssh://mini/tmp/a/../socket")?.socket,
            "/tmp/a/../socket"
        );
        Ok(())
    }

    #[test]
    fn rejects_ambiguous_addresses_before_launching_ssh() {
        for address in [
            "ssh://mini",
            "ssh://mini/",
            "ssh://user:password@mini/tmp/socket",
            "ssh://-option/tmp/socket",
            "ssh://mini/tmp/a:b",
            "ssh://mini/tmp/a%20b",
            "ssh://mini/tmp/a b",
            "ssh://mini/tmp/a?b",
            "ssh://mini/tmp/a#b",
            "ssh://mini/tmp/a\nb",
            "http://mini/tmp/socket",
        ] {
            assert!(Address::parse(address).is_err(), "{address:?}");
        }
    }
}
