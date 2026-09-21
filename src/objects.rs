use anyhow::{Context, Result, bail, ensure};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const MAX_OBJECT: usize = 5 * 1024 * 1024;
const MAX_HEADER: usize = 64 * 1024;

#[derive(Clone, Default)]
pub struct Keys {
    secret: Option<[u8; 32]>,
}

impl Keys {
    pub fn from_file(path: Option<&Path>) -> Result<Self> {
        match path {
            None => Ok(Self::default()),
            Some(p) => {
                let s = std::fs::read_to_string(p).context("read encryption key")?;
                let key: [u8; 32] = hex::decode(s.trim())?
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("key must contain 64 hexadecimal characters"))?;
                // The explicitly requested null/zero-key mode means unencrypted.
                Ok(Self {
                    secret: (key != [0; 32]).then_some(key),
                })
            }
        }
    }
    pub fn from_bytes(key: [u8; 32]) -> Self {
        Self {
            secret: (key != [0; 32]).then_some(key),
        }
    }
    fn derived(&self, purpose: &str) -> Option<[u8; 32]> {
        self.secret.map(|key| blake3::derive_key(purpose, &key))
    }
    pub fn domain(&self) -> String {
        self.derived("cairn v1 repository namespace")
            .map(hex::encode)
            .unwrap_or_else(|| "plain".into())
    }
    pub fn object_id(&self, bytes: &[u8]) -> String {
        match self.derived("cairn v1 object identity") {
            Some(key) => blake3::keyed_hash(&key, bytes).to_hex().to_string(),
            None => blake3::hash(bytes).to_hex().to_string(),
        }
    }
    pub fn seal(&self, bytes: &[u8], refs: Vec<String>) -> Result<Vec<u8>> {
        let mut nonce = [0; 24];
        let key = self.derived("cairn v1 authenticated encryption");
        if key.is_some() {
            rand::rngs::OsRng.fill_bytes(&mut nonce);
        }
        let header = Header {
            refs,
            nonce: key.map(|_| hex::encode(nonce)),
        };
        let hdr = serde_json::to_vec(&header)?;
        ensure!(hdr.len() <= MAX_HEADER, "object has too many references");
        let body = if let Some(k) = key {
            XChaCha20Poly1305::new_from_slice(&k)
                .unwrap()
                .encrypt(
                    &XNonce::from(nonce),
                    Payload {
                        msg: bytes,
                        aad: &hdr,
                    },
                )
                .map_err(|_| anyhow::anyhow!("encryption failed"))?
        } else {
            bytes.to_vec()
        };
        let mut result = Vec::with_capacity(40 + hdr.len() + body.len());
        result.extend_from_slice(b"CRN2");
        result.extend_from_slice(&(hdr.len() as u32).to_le_bytes());
        result.extend_from_slice(&hdr);
        result.extend_from_slice(&body);
        // Transport/storage integrity is checkable without the E2E key. This is
        // not authentication: keyed readers still verify AEAD and object IDs.
        let checksum = blake3::hash(&result);
        result.extend_from_slice(checksum.as_bytes());
        ensure!(result.len() <= MAX_OBJECT, "object exceeds format limit");
        Ok(result)
    }
    pub fn open(&self, encoded: &[u8]) -> Result<(Vec<u8>, Vec<String>)> {
        let (header, hdr, body) = envelope(encoded)?;
        let bytes = match (
            self.derived("cairn v1 authenticated encryption"),
            &header.nonce,
        ) {
            (Some(k), Some(n)) => {
                let nonce: [u8; 24] = hex::decode(n)?
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("bad nonce"))?;
                XChaCha20Poly1305::new_from_slice(&k)
                    .unwrap()
                    .decrypt(
                        &XNonce::from(nonce),
                        Payload {
                            msg: body,
                            aad: hdr,
                        },
                    )
                    .map_err(|_| {
                        anyhow::anyhow!("object authentication failed (wrong key or damaged data)")
                    })?
            }
            (None, None) => body.to_vec(),
            _ => bail!("encryption mode mismatch"),
        };
        Ok((bytes, header.refs))
    }
}

#[derive(Serialize, Deserialize)]
pub struct Header {
    pub refs: Vec<String>,
    nonce: Option<String>,
}

pub fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
pub fn envelope(bytes: &[u8]) -> Result<(Header, &[u8], &[u8])> {
    ensure!(
        bytes.len() >= 8 && bytes.len() <= MAX_OBJECT,
        "invalid object envelope"
    );
    let bytes = match &bytes[..4] {
        b"CRN2" => {
            ensure!(bytes.len() >= 40, "truncated object checksum");
            let (encoded, checksum) = bytes.split_at(bytes.len() - 32);
            ensure!(
                blake3::hash(encoded).as_bytes() == checksum,
                "stored object checksum mismatch"
            );
            encoded
        }
        // Read compatibility only; legacy envelopes have no keyless checksum.
        b"CRN1" => bytes,
        _ => bail!("invalid object envelope"),
    };
    let n = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    ensure!(
        n <= MAX_HEADER && n <= bytes.len() - 8,
        "invalid header length"
    );
    let hdr = &bytes[8..8 + n];
    let header: Header = serde_json::from_slice(hdr)?;
    ensure!(
        header.refs.len() <= 512 && header.refs.iter().all(|r| valid_id(r)),
        "invalid references"
    );
    Ok((header, hdr, &bytes[8 + n..]))
}
