//! Shared TLS setup for every outbound HTTP client.
//!
//! Codexify builds `reqwest` with the `rustls-no-provider` feature so the
//! whole process shares a single rustls crypto provider — ring — rather than
//! bundling aws-lc-rs. This also keeps us on one `reqwest` major (the same one
//! RMCP's Streamable HTTP client transport uses), so there is exactly one TLS
//! stack and one set of trust roots in the binary.
//!
//! The catch is that `reqwest` resolves the process-default [`rustls`]
//! `CryptoProvider` when a client is *built* and panics if none is installed
//! (aws-lc-rs, its usual fallback, is compiled out). Every client must therefore
//! be constructed through [`client_builder`], which installs ring exactly once
//! beforehand.

use std::sync::Once;

static INSTALL: Once = Once::new();

/// Install the ring crypto provider as the process-wide rustls default, exactly
/// once. Idempotent and cheap to call from any client factory.
pub fn ensure_crypto_provider() {
    INSTALL.call_once(|| {
        // A competing default may already be installed (e.g. by another caller
        // in the same process); we only need *some* provider present before a
        // client is built, so a failed install is fine to ignore.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// A `reqwest` client builder with the ring crypto provider guaranteed
/// installed. Use this instead of `reqwest::Client::builder()` so the client
/// can be built without panicking under the `rustls-no-provider` feature.
pub fn client_builder() -> reqwest::ClientBuilder {
    ensure_crypto_provider();
    reqwest::Client::builder()
}
