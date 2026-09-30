//! TLS certificates.
//!
//! On first start Strymek creates a small private certificate authority and a
//! server certificate signed by it, valid for every name/IP in `tls_names`.
//! Trust the CA once on the Mac (download it from `/ca.pem`) and the browser
//! never shows a warning, even when the server certificate is re-issued.

use crate::config::{config_dir, ensure_private_dir, Config};
use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use time::{Duration, OffsetDateTime};

pub struct TlsFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
    pub ca: Option<PathBuf>,
}

fn tls_dir() -> PathBuf {
    config_dir().join("tls")
}

pub fn ca_path() -> PathBuf {
    tls_dir().join("ca.pem")
}

fn write_private(path: &PathBuf, data: &str) -> Result<()> {
    std::fs::write(path, data)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn ca_params(host: &str) -> Result<CertificateParams> {
    let mut p = CertificateParams::new(Vec::<String>::new())?;
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.distinguished_name
        .push(DnType::CommonName, format!("Strymek local CA ({host})"));
    p.distinguished_name.push(DnType::OrganizationName, "Strymek");
    p.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let now = OffsetDateTime::now_utc();
    p.not_before = now - Duration::days(1);
    p.not_after = now + Duration::days(3650);
    Ok(p)
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "workstation".into())
}

/// Make sure a usable certificate exists and return the file paths.
pub fn ensure(cfg: &Config) -> Result<TlsFiles> {
    if let (Some(cert), Some(key)) = (&cfg.tls_cert, &cfg.tls_key) {
        return Ok(TlsFiles { cert: cert.clone(), key: key.clone(), ca: None });
    }
    let dir = tls_dir();
    ensure_private_dir(&dir)?;
    let ca_key_path = dir.join("ca.key");
    let ca_pem_path = ca_path();
    let ca_name_path = dir.join("ca.name");
    let host = std::fs::read_to_string(&ca_name_path)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| hostname());

    // 1. CA (created once, kept for 10 years).
    let ca_key = if ca_key_path.exists() && ca_pem_path.exists() {
        KeyPair::from_pem(&std::fs::read_to_string(&ca_key_path)?)
            .context("reading CA key")?
    } else {
        let key = KeyPair::generate()?;
        let cert = ca_params(&host)?.self_signed(&key)?;
        write_private(&ca_key_path, &key.serialize_pem())?;
        std::fs::write(&ca_pem_path, cert.pem())?;
        std::fs::write(&ca_name_path, &host)?;
        tracing::info!("created local CA at {}", ca_pem_path.display());
        key
    };
    // Re-creating the issuer object from the same key and name yields the same
    // issuer identity, so leaf certificates chain to the stored CA file.
    let ca_cert = ca_params(&host)?.self_signed(&ca_key)?;

    // 2. Server certificate, re-issued whenever the name list changes or it nears expiry.
    let names = if cfg.tls_names.is_empty() { Config::default_tls_names() } else { cfg.tls_names.clone() };
    let names_path = dir.join("server.names");
    let cert_path = dir.join("server.pem");
    let key_path = dir.join("server.key");
    let stamp = format!("{}\n", names.join("\n"));
    let fresh = std::fs::read_to_string(&names_path).ok().as_deref() == Some(stamp.as_str())
        && cert_path.exists()
        && key_path.exists()
        && std::fs::metadata(&cert_path)
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().map(|e| e.as_secs() < 700 * 86400).unwrap_or(false))
            .unwrap_or(false);
    if !fresh {
        let key = KeyPair::generate()?;
        let mut p = CertificateParams::new(names.clone())?;
        p.distinguished_name.push(DnType::CommonName, format!("Strymek on {host}"));
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        let now = OffsetDateTime::now_utc();
        p.not_before = now - Duration::days(1);
        // macOS rejects TLS server certificates valid for more than 825 days.
        p.not_after = now + Duration::days(800);
        let cert = p.signed_by(&key, &ca_cert, &ca_key)?;
        let chain = format!("{}{}", cert.pem(), std::fs::read_to_string(&ca_pem_path)?);
        std::fs::write(&cert_path, chain)?;
        write_private(&key_path, &key.serialize_pem())?;
        std::fs::write(&names_path, stamp)?;
        tracing::info!("issued server certificate for: {}", names.join(", "));
    }
    Ok(TlsFiles { cert: cert_path, key: key_path, ca: Some(ca_pem_path) })
}
