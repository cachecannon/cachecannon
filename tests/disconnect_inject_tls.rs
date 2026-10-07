//! `connection.disconnect_rate` over TLS: the same close path as plain TCP,
//! with close_notify sent ahead of the FIN.
//!
//! One test per file: the benchmark uses process-global metrics and a global
//! config channel, so it must not share a process with another run.

mod common;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// Generate a self-signed cert and key with openssl. `None` when openssl is
/// unavailable, so the suite still passes on a machine without it.
fn gen_cert(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let crt = dir.join("c.crt");
    let key = dir.join("c.key");
    let out = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-sha256",
            "-days",
            "3650",
            "-nodes",
            "-keyout",
            key.to_str()?,
            "-out",
            crt.to_str()?,
            "-subj",
            "/CN=cachecannon-test",
            "-addext",
            "subjectAltName=DNS:localhost,IP:127.0.0.1",
        ])
        .output()
        .ok()?;
    out.status.success().then_some((crt, key))
}

#[test]
fn injected_disconnects_work_over_tls() {
    let dir = std::env::temp_dir().join(format!("cc-disconnect-tls-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let Some((crt, key)) = gen_cert(&dir) else {
        eprintln!("openssl unavailable; skipping");
        return;
    };
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&crt)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_file(&key).unwrap();
    let server_config = Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap(),
    );

    let (addr, counters) = common::start_stub(move |tcp, counters| {
        let Ok(session) = rustls::ServerConnection::new(Arc::clone(&server_config)) else {
            return;
        };
        common::serve(rustls::StreamOwned::new(session, tcp), counters);
    });
    common::run_and_check(addr, &counters, "tls = true\ntls_verify = false");
    let _ = std::fs::remove_dir_all(&dir);
}
