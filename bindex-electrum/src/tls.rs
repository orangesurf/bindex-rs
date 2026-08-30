use std::{fs::File, io::BufReader, path::Path, sync::Arc};

use tokio_rustls::rustls::{
    pki_types::{CertificateDer, PrivateKeyDer},
    ServerConfig,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("no certificates found in {0}")]
    NoCertificates(String),

    #[error("no private key found in {0}")]
    NoPrivateKey(String),

    #[error("invalid TLS config: {0}")]
    Rustls(#[from] tokio_rustls::rustls::Error),
}

pub fn load_server_config(
    cert_path: impl AsRef<Path>,
    key_path: impl AsRef<Path>,
) -> Result<Arc<ServerConfig>, Error> {
    let cert_path = cert_path.as_ref();
    let key_path = key_path.as_ref();

    let mut cert_reader = BufReader::new(File::open(cert_path)?);
    let certs = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<CertificateDer<'static>>, _>>()?;
    if certs.is_empty() {
        return Err(Error::NoCertificates(cert_path.display().to_string()));
    }

    let key = load_private_key(key_path)?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    Ok(Arc::new(config))
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, Error> {
    let mut reader = BufReader::new(File::open(path)?);
    if let Some(key) = rustls_pemfile::pkcs8_private_keys(&mut reader).next() {
        return Ok(PrivateKeyDer::Pkcs8(key?));
    }

    let mut reader = BufReader::new(File::open(path)?);
    if let Some(key) = rustls_pemfile::rsa_private_keys(&mut reader).next() {
        return Ok(PrivateKeyDer::Pkcs1(key?));
    }

    Err(Error::NoPrivateKey(path.display().to_string()))
}
