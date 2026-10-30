//! TLS for listeners reachable from other machines.
//!
//! By default the bridge generates a self-signed certificate once, keeps it in the state
//! directory, and prints the pin of its public key. Clients authenticate the server by that pin
//! (`curl --pinnedpubkey "sha256//..."`), so no certificate authority is involved. A certificate
//! from a CA (e.g. Let's Encrypt) can be configured instead.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine;
use rcgen::{CertificateParams, DnType, KeyPair, PublicKeyData};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};

use crate::config::TlsSettings;

pub struct TlsMaterial {
    pub config: Arc<rustls::ServerConfig>,
    /// `sha256//<base64>` pin of the public key; only for the self-signed certificate.
    pub pin: Option<String>,
    pub cert_path: PathBuf,
}

/// Loads the configured certificate, or the self-signed one from `state_dir` (creating it on
/// first use).
pub fn load_or_create(settings: &TlsSettings, state_dir: &Path, host: &str) -> Result<TlsMaterial> {
    let (cert_path, key_path, self_signed) = match (&settings.cert, &settings.key) {
        (Some(cert), Some(key)) => (cert.clone(), key.clone(), false),
        _ => {
            let dir = state_dir.join("tls");
            (dir.join("cert.pem"), dir.join("key.pem"), true)
        }
    };
    if self_signed && !(cert_path.exists() && key_path.exists()) {
        let mut names = settings.hostnames.clone();
        if names.is_empty() {
            names = vec![host.to_string(), "localhost".into()];
        }
        generate(&cert_path, &key_path, &names)?;
        tracing::info!(
            "generated a self-signed TLS certificate in {}",
            cert_path.display()
        );
    }
    let cert_pem = std::fs::read(&cert_path)
        .with_context(|| format!("cannot read TLS certificate {}", cert_path.display()))?;
    let key_pem = std::fs::read(&key_path)
        .with_context(|| format!("cannot read TLS private key {}", key_path.display()))?;
    let certs = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("invalid PEM certificate {}", cert_path.display()))?;
    let key = PrivateKeyDer::from_pem_slice(&key_pem)
        .with_context(|| format!("invalid PEM private key {}", key_path.display()))?;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("the TLS certificate and private key do not match")?;
    // HTTP/1.1 only: nothing here benefits from HTTP/2, and it keeps connection handling simple.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    let pin = if self_signed {
        let pem = String::from_utf8_lossy(&key_pem);
        let key = KeyPair::from_pem(&pem).context("cannot read the self-signed key")?;
        Some(spki_pin(&key.subject_public_key_info()))
    } else {
        None
    };
    Ok(TlsMaterial {
        config: Arc::new(config),
        pin,
        cert_path,
    })
}

/// The pin format of `curl --pinnedpubkey`: SHA-256 of the DER SubjectPublicKeyInfo, base64.
pub fn spki_pin(spki_der: &[u8]) -> String {
    format!(
        "sha256//{}",
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(spki_der))
    )
}

fn generate(cert_path: &Path, key_path: &Path, names: &[String]) -> Result<()> {
    let mut params = CertificateParams::new(names.to_vec()).context("invalid TLS hostname")?;
    params
        .distinguished_name
        .push(DnType::CommonName, "safe-grokbot-bridge");
    // Clients pin the key, so a long validity avoids pointless expiry.
    params.not_before = rcgen::date_time_ymd(2025, 1, 1);
    params.not_after = rcgen::date_time_ymd(2125, 1, 1);
    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    if let Some(dir) = cert_path.parent() {
        create_private_dir(dir)?;
    }
    std::fs::write(cert_path, cert.pem())
        .with_context(|| format!("cannot write {}", cert_path.display()))?;
    write_private(key_path, key.serialize_pem().as_bytes())
}

/// Creates a directory readable only by the current user (on Unix).
pub fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("cannot write {}", path.display()))?;
        file.write_all(data)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, data).with_context(|| format!("cannot write {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_signed_certificate_is_persisted_with_a_stable_pin() {
        let dir = tempfile::tempdir().unwrap();
        let settings = TlsSettings {
            hostnames: vec![],
            cert: None,
            key: None,
        };
        let first = load_or_create(&settings, dir.path(), "bridge.example").unwrap();
        let second = load_or_create(&settings, dir.path(), "bridge.example").unwrap();
        let pin = first.pin.unwrap();
        assert!(
            pin.starts_with("sha256//") && pin.len() == "sha256//".len() + 44,
            "{pin}"
        );
        assert_eq!(Some(pin), second.pin);

        // A user-provided certificate has no pin.
        let user = TlsSettings {
            hostnames: vec![],
            cert: Some(first.cert_path.clone()),
            key: Some(dir.path().join("tls/key.pem")),
        };
        assert!(
            load_or_create(&user, dir.path(), "x")
                .unwrap()
                .pin
                .is_none()
        );
    }
}
