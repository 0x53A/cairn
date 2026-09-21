//! HTTP directly over a russh channel: no local listener or subprocess.
use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use russh::{
    client,
    keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate},
};
use std::{path::PathBuf, sync::Arc, time::Duration};

struct HostCheck {
    host: String,
    port: u16,
    known_hosts: PathBuf,
}
impl client::Handler for HostCheck {
    type Error = anyhow::Error;
    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            anyhow::bail!("native SSH host certificates are not supported; use ssh-openssh://");
        };
        // Fail closed on unsupported marker semantics (notably revocation).
        let text = std::fs::read_to_string(&self.known_hosts)
            .context("read SSH known_hosts; set CAIRN_SSH_KNOWN_HOSTS if needed")?;
        ensure!(
            !text.lines().any(|line| line.trim_start().starts_with('@')),
            "native SSH does not support known_hosts markers; use ssh-openssh:// for this file"
        );
        ensure!(
            russh::keys::check_known_hosts_path(&self.host, self.port, key, &self.known_hosts)?,
            "unknown SSH host key for {}:{}; provision a verified known_hosts entry first",
            self.host,
            self.port
        );
        Ok(true)
    }
}

pub struct NativeSsh {
    session: client::Handle<HostCheck>,
    runtime: tokio::runtime::Runtime,
    socket: String,
    domain: String,
    token: Option<String>,
}

impl NativeSsh {
    pub fn connect(location: &str, domain: &str, token: Option<String>) -> Result<Self> {
        let address = crate::ssh::Address::parse(location)?;
        let home = std::env::home_dir().context("cannot find home directory")?;
        let known_hosts = std::env::var_os("CAIRN_SSH_KNOWN_HOSTS")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".ssh/known_hosts"));
        let identity = std::env::var_os("CAIRN_SSH_IDENTITY").map(PathBuf::from);
        let user = if address.user.is_empty() {
            std::env::var("USER").context("specify SSH user in ssh://user@host/path")?
        } else {
            address.user.clone()
        };
        let port = address.port.unwrap_or(22);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        let establish = async {
            let handler = HostCheck {
                host: address.host.clone(),
                port,
                known_hosts,
            };
            let config = client::Config {
                keepalive_interval: Some(Duration::from_secs(15)),
                keepalive_max: 3,
                ..Default::default()
            };
            let mut session =
                client::connect(Arc::new(config), (address.host.as_str(), port), handler).await?;
            let hash = session.best_supported_rsa_hash().await?.flatten();
            if let Some(path) = identity {
                let passphrase = std::env::var("CAIRN_SSH_KEY_PASSPHRASE").ok();
                let key = russh::keys::load_secret_key(&path, passphrase.as_deref())
                    .context("load SSH identity")?;
                ensure!(
                    session
                        .authenticate_publickey(
                            &user,
                            PrivateKeyWithHashAlg::new(Arc::new(key), hash)
                        )
                        .await?
                        .success(),
                    "SSH public-key authentication failed"
                );
                return Ok::<_, anyhow::Error>(session);
            }
            if std::env::var_os("SSH_AUTH_SOCK").is_some() {
                let mut agent = russh::keys::agent::client::AgentClient::connect_env().await?;
                for identity in agent.request_identities().await? {
                    let russh::keys::agent::AgentIdentity::PublicKey { key, .. } = identity else {
                        continue;
                    };
                    if session
                        .authenticate_publickey_with(&user, key, hash, &mut agent)
                        .await?
                        .success()
                    {
                        return Ok(session);
                    }
                }
            }
            for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
                let path = home.join(".ssh").join(name);
                if path.is_file() {
                    let key = russh::keys::load_secret_key(&path, None)
                        .context("load SSH identity; use an agent for encrypted keys, or set CAIRN_SSH_IDENTITY and CAIRN_SSH_KEY_PASSPHRASE")?;
                    if session
                        .authenticate_publickey(
                            &user,
                            PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                        )
                        .await?
                        .success()
                    {
                        return Ok(session);
                    }
                }
            }
            anyhow::bail!(
                "SSH authentication failed; set CAIRN_SSH_IDENTITY or load an SSH agent key"
            )
        };
        let session = runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(120), establish).await })
            .context("SSH setup timed out")??;
        Ok(Self {
            session,
            runtime,
            socket: address.socket,
            domain: domain.into(),
            token,
        })
    }

    pub fn request(
        &self,
        method: Method,
        key: &str,
        body: Vec<u8>,
    ) -> Result<(StatusCode, Vec<u8>)> {
        self.runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(3600), async {
                let channel = self
                    .session
                    .channel_open_direct_streamlocal(&self.socket)
                    .await?;
                let (mut sender, connection) =
                    hyper::client::conn::http1::handshake(TokioIo::new(channel.into_stream()))
                        .await?;
                let task = tokio::spawn(connection);
                // Abort the driver on every return path, including timeout/cancellation.
                struct Driver(tokio::task::JoinHandle<Result<(), hyper::Error>>);
                impl Drop for Driver {
                    fn drop(&mut self) {
                        self.0.abort();
                    }
                }
                let _driver = Driver(task);
                let mut request = Request::builder()
                    .method(method)
                    .uri(format!("/v1/{}/{key}", self.domain))
                    .header("Host", "localhost");
                if let Some(token) = &self.token {
                    request = request.header("Authorization", format!("Bearer {token}"));
                }
                if key == "import" {
                    request = request.header("Content-Type", "application/json");
                }
                let response = sender
                    .send_request(request.body(Full::new(Bytes::from(body)))?)
                    .await?;
                let status = response.status();
                let mut body = response.into_body();
                let mut bytes = Vec::new();
                while let Some(frame) = body.frame().await {
                    if let Ok(data) = frame?.into_data() {
                        ensure!(
                            bytes.len() + data.len() <= crate::objects::MAX_OBJECT,
                            "SSH HTTP response exceeds size limit"
                        );
                        bytes.extend_from_slice(&data);
                    }
                }
                Ok::<_, anyhow::Error>((status, bytes))
            })
            .await
            .context("SSH HTTP request timed out")?
        })
    }
}

impl Drop for NativeSsh {
    fn drop(&mut self) {
        let _ = self.runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(2),
                self.session
                    .disconnect(russh::Disconnect::ByApplication, "", ""),
            )
            .await
        });
    }
}
