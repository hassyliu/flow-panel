//! TLS client configuration.

use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use rustls::{ClientConfig, RootCertStore};
use std::path::Path;
use std::sync::Arc;

/// Mozilla roots built into the binary, plus an optional PEM bundle for private CAs.
pub fn client_config(extra_ca: Option<&Path>) -> Result<Arc<ClientConfig>, String> {
    let mut roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = extra_ca {
        let error = |e: &dyn std::fmt::Display| format!("读取 CA 证书 {} 失败：{e}", path.display());
        let mut added = 0;
        for cert in CertificateDer::pem_file_iter(path).map_err(|e| error(&e))? {
            roots.add(cert.map_err(|e| error(&e))?).map_err(|e| error(&e))?;
            added += 1;
        }
        if added == 0 {
            return Err(format!("CA 证书文件 {} 中没有证书", path.display()));
        }
    }
    Ok(with_roots(roots))
}

pub fn with_roots(roots: RootCertStore) -> Arc<ClientConfig> {
    let mut provider = rustls::crypto::ring::default_provider();
    if !has_aes_hardware() {
        // CPUs such as the Celeron J1900 lack AES-NI. ChaCha20-Poly1305 is several
        // times faster in software, so offer it first.
        provider.cipher_suites.sort_by_key(|suite| !format!("{:?}", suite.suite()).contains("CHACHA20"));
    }
    let config = ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .expect("ring provider supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

pub fn has_aes_hardware() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("aes") && std::arch::is_x86_feature_detected!("pclmulqdq")
    }
    #[cfg(target_arch = "aarch64")]
    {
        std::arch::is_aarch64_feature_detected!("aes")
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        false
    }
}
