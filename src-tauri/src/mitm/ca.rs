//! Persistent root CA used by hudsucker to mint per-host leaf certs on the
//! fly. Stored at `~/Library/Application Support/Pouch/ca/pouch-ca.{pem,key}`
//! so the user can install it into the macOS System keychain once and have it
//! trusted by every subsequent Pouch launch.
//!
//! ECDSA P-256 / SHA-256 — small, fast, universally accepted. CA validity is
//! 10 years from generation (much longer than the leaf certs hudsucker mints
//! on the fly, which carry their own short validity).

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use hudsucker::certificate_authority::RcgenAuthority;
use hudsucker::rustls::crypto::aws_lc_rs;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};

use super::MitmError;

/// LRU cache size for hudsucker's per-host leaf cert cache. Bumped from
/// the previous 1_000 to 10_000 so a long browsing session that touches
/// thousands of distinct subdomains (analytics, ad networks, embedded
/// widgets, CDN sharded hosts) doesn't evict a leaf and re-mint it under
/// a *different* serial number — that would change the cert thumbprint
/// mid-session and break any in-page pinning / fingerprint logic the
/// site applies to its own assets. Memory cost is still trivial: each
/// cached entry is a small ECDSA leaf cert + key.
const LEAF_CACHE_SIZE: u64 = 10_000;

/// CA validity window. `time` crate accepts `OffsetDateTime`; we compute via
/// `SystemTime + Duration` which both rcgen 0.14 helpers accept.
const CA_VALIDITY_SECS: u64 = 60 * 60 * 24 * 365 * 10;

/// Load `~/Library/Application Support/Pouch/ca/pouch-ca.{pem,key}` if both
/// files exist; otherwise generate a fresh ECDSA P-256 root CA and persist
/// it. Returns the hudsucker [`RcgenAuthority`] that mints leaf certs.
pub fn load_or_create_authority() -> Result<RcgenAuthority, MitmError> {
    let dir = ca_directory()?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| MitmError::Ca(format!("create_dir_all({}): {e}", dir.display())))?;

    let pem_path = dir.join("pouch-ca.pem");
    let key_path = dir.join("pouch-ca.key");

    let (cert_pem, key_pem) = if pem_path.exists() && key_path.exists() {
        let cert_pem = std::fs::read_to_string(&pem_path)
            .map_err(|e| MitmError::Ca(format!("read {}: {e}", pem_path.display())))?;
        let key_pem = std::fs::read_to_string(&key_path)
            .map_err(|e| MitmError::Ca(format!("read {}: {e}", key_path.display())))?;
        tracing::info!(
            target: "hook",
            "[mitm] loaded existing CA from {}",
            dir.display()
        );
        (cert_pem, key_pem)
    } else {
        let (cert_pem, key_pem) = generate_self_signed_ca()?;
        std::fs::write(&pem_path, &cert_pem)
            .map_err(|e| MitmError::Ca(format!("write {}: {e}", pem_path.display())))?;
        std::fs::write(&key_path, &key_pem)
            .map_err(|e| MitmError::Ca(format!("write {}: {e}", key_path.display())))?;
        tracing::info!(
            target: "hook",
            "[mitm] generated new CA at {}",
            dir.display()
        );
        (cert_pem, key_pem)
    };

    let key = KeyPair::from_pem(&key_pem)
        .map_err(|e| MitmError::Ca(format!("parse CA key PEM: {e}")))?;
    let issuer = Issuer::from_ca_cert_pem(&cert_pem, key)
        .map_err(|e| MitmError::Ca(format!("parse CA cert PEM: {e}")))?;

    Ok(RcgenAuthority::new(
        issuer,
        LEAF_CACHE_SIZE,
        aws_lc_rs::default_provider(),
    ))
}

/// Generate a fresh ECDSA P-256 self-signed root CA. Returns `(cert_pem,
/// key_pem)` — the caller persists both to disk and re-parses them through
/// [`Issuer::from_ca_cert_pem`] so the on-disk and in-memory representations
/// always agree.
fn generate_self_signed_ca() -> Result<(String, String), MitmError> {
    let mut params = CertificateParams::new(Vec::<String>::new())
        .map_err(|e| MitmError::Ca(format!("CertificateParams::new: {e}")))?;

    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "Pouch Root CA");
    dn.push(DnType::OrganizationName, "Pouch");
    params.distinguished_name = dn;

    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];

    let now = SystemTime::now();
    params.not_before = (now - Duration::from_secs(60)).into();
    params.not_after = (now + Duration::from_secs(CA_VALIDITY_SECS)).into();

    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| MitmError::Ca(format!("KeyPair::generate_for: {e}")))?;
    let cert = params
        .self_signed(&key)
        .map_err(|e| MitmError::Ca(format!("self_signed: {e}")))?;

    Ok((cert.pem(), key.serialize_pem()))
}

/// Resolve `<app data>/ca/`. Reuses [`crate::util::macos_app_support_dir`] so
/// dev and prod both land at `~/Library/Application Support/Pouch/ca/` —
/// matches the convention used by storage.db / cache_root.
fn ca_directory() -> Result<PathBuf, MitmError> {
    let root = crate::util::macos_app_support_dir()
        .ok_or_else(|| MitmError::Ca("macos_app_support_dir unavailable ($HOME unset?)".into()))?;
    Ok(root.join("ca"))
}

/// Absolute path of the persisted CA's PEM file. Phase 2b's trust-detection
/// / install path passes this to the macOS `security` CLI. Returns `None`
/// when `$HOME` is unset (same fallthrough as [`ca_directory`]); callers
/// treat that as "trust check inconclusive" and skip the install prompt.
pub fn ca_pem_path() -> Option<PathBuf> {
    ca_directory().ok().map(|d| d.join("pouch-ca.pem"))
}
