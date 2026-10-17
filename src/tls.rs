//! HTTPS on the remote's own port, for the page: WebGPU — which the cce-ui
//! app draws with — exists only on a secure page, so over plain HTTP a phone
//! gets the classic page and nothing else.
//!
//! The certificate is lego's (`cce-remote-cert`, renewed by
//! `cce-remote-cert.timer`): a Let's Encrypt certificate for a name that
//! resolves to this machine's tailnet address, proven over Cloudflare DNS, so
//! nothing has to be reachable from the internet. It is read from
//! `$XDG_DATA_HOME/cce-remote/lego/certificates/` — the first `<name>.crt`
//! with a `<name>.key` beside it — and read again whenever either file
//! changes, so a renewal needs no restart. No certificate there: no HTTPS,
//! and the server is plain HTTP as before.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;

pub fn lego_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".local/share")
    });
    base.join("cce-remote").join("lego")
}

/// lego's certificate (the chain: lego bundles the issuer) and key. Its
/// `<name>.issuer.crt` is the issuer alone, not a certificate to serve.
fn cert_files() -> Option<(PathBuf, PathBuf)> {
    let dir = lego_dir().join("certificates");
    let mut crts: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "crt") && !p.to_string_lossy().ends_with(".issuer.crt"))
        .collect();
    crts.sort();
    crts.into_iter().find_map(|crt| {
        let key = crt.with_extension("key");
        key.is_file().then_some((crt, key))
    })
}

/// The newest change to either file: a renewal rewrites both.
fn stamp(crt: &PathBuf, key: &PathBuf) -> Option<SystemTime> {
    let m = |p: &PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    m(crt).max(m(key))
}

type Loaded = Option<(PathBuf, SystemTime, Arc<ServerConfig>)>;
static LOADED: Mutex<Loaded> = Mutex::new(None);

/// The TLS configuration to answer a ClientHello with, if there is a
/// certificate. Cheap to call per connection: two `stat`s, and a reload only
/// when the files changed.
pub fn config() -> Option<Arc<ServerConfig>> {
    let (crt, key) = cert_files()?;
    let stamp = stamp(&crt, &key)?;
    let mut loaded = LOADED.lock().unwrap();
    if let Some((path, at, cfg)) = loaded.as_ref() {
        if *path == crt && *at == stamp {
            return Some(cfg.clone());
        }
    }
    match load(&crt, &key) {
        Ok(cfg) => {
            println!("[cce-remote] https: serving {}", crt.display());
            let cfg = Arc::new(cfg);
            *loaded = Some((crt, stamp, cfg.clone()));
            Some(cfg)
        }
        Err(e) => {
            eprintln!("[cce-remote] https: cannot load {}: {e}", crt.display());
            // A half-written renewal: keep serving the last good one.
            loaded.as_ref().map(|(_, _, cfg)| cfg.clone())
        }
    }
}

fn load(crt: &PathBuf, key: &PathBuf) -> Result<ServerConfig, String> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(crt)
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let key = PrivateKeyDer::from_pem_file(key).map_err(|e| e.to_string())?;
    ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|e| e.to_string())
}
