//! Momento connection setup for benchmarking via ringline-momento.
//!
//! The `ringline_momento::Client` handles TLS, authentication, and protosocket
//! framing internally. This module only provides `MomentoSetup` which resolves
//! the credential once per worker.

use crate::config::Config;

use ringline_momento::Credential;
use std::io;

/// Shared Momento setup resolved once per worker.
pub struct MomentoSetup {
    /// Resolved credential.
    pub credential: Credential,
}

impl MomentoSetup {
    /// Resolve Momento configuration from the benchmark config.
    pub fn from_config(config: &Config) -> io::Result<Self> {
        let credential = Credential::from_env()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;

        let credential = if let Some(ref endpoint) = config.momento.endpoint {
            Credential::with_endpoint(credential.token().to_string(), endpoint.clone())
        } else {
            credential
        };

        Ok(Self { credential })
    }

    /// Resolve the Momento endpoint hostname for display purposes.
    /// Returns the host portion of the endpoint (e.g. "cache.cell-us-west-2-1.prod.a.momentohq.com").
    pub fn resolve_endpoint_display(config: &Config) -> Option<String> {
        let credential = Credential::from_env().ok()?;
        let credential = if let Some(ref endpoint) = config.momento.endpoint {
            Credential::with_endpoint(credential.token().to_string(), endpoint.clone())
        } else {
            credential
        };
        Some(credential.endpoint().to_string())
    }
}
