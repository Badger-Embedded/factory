//! Helpers shared by this crate's test binaries.

#![allow(dead_code, reason = "each test binary uses a part of the helpers")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Writes `body` as the executable script `dir/name` and returns its path.
///
/// Tests run in parallel threads, and a child that another thread forks
/// inherits every open file until it calls exec. If this process had the
/// script open for writing at that moment, running the script fails with
/// "Text file busy" until the child execs. So the text goes to a sibling file
/// that nothing runs, and a `cp` child creates the script, which this process
/// never opens.
pub fn executable(dir: &Path, name: &str, body: &str) -> PathBuf {
    let source = dir.join(format!("{name}.src"));
    fs::write(&source, body).unwrap();
    let path = dir.join(name);
    let status = Command::new("cp").arg(&source).arg(&path).status().unwrap();
    assert!(status.success(), "cp {} failed", source.display());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A throwaway CA and a TLS listener for `127.0.0.1` whose certificate it
/// signs. The keys exist only in memory, so no private key is committed.
pub struct TlsServer {
    pub ca_pem: String,
    pub base: String,
}

/// Serves `app` over TLS on an ephemeral port. A client trusts it only with
/// the returned `ca_pem` as an extra root.
pub async fn serve_tls(app: axum::Router) -> TlsServer {
    use std::sync::Arc;

    use hyper_util::rt::{TokioExecutor, TokioIo};
    use hyper_util::server::conn::auto::Builder;
    use hyper_util::service::TowerToHyperService;
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca = ca_params.self_signed(&ca_key).unwrap();

    let leaf_key = KeyPair::generate().unwrap();
    let leaf_params =
        CertificateParams::new(vec!["127.0.0.1".to_owned(), "localhost".to_owned()]).unwrap();
    let leaf = leaf_params.signed_by(&leaf_key, &ca, &ca_key).unwrap();

    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(leaf.der().to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
        )
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            let service = TowerToHyperService::new(app.clone());
            tokio::spawn(async move {
                // A client that rejects the certificate aborts the handshake.
                let Ok(tls) = acceptor.accept(stream).await else {
                    return;
                };
                let _ = Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(tls), service)
                    .await;
            });
        }
    });
    TlsServer {
        ca_pem: ca.pem(),
        base: format!("https://{addr}"),
    }
}
