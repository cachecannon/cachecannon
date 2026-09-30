/// A libfabric provider the client can drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Provider {
    /// Software transport, for development on hosts without RDMA hardware
    #[default]
    Tcp,
    /// EFA's `efa-direct` fabric
    EfaDirect,
}

impl Provider {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Provider::Tcp => "tcp",
            Provider::EfaDirect => "efa",
        }
    }

    pub(crate) fn fabric_name(self) -> Option<&'static str> {
        match self {
            Provider::EfaDirect => Some("efa-direct"),
            Provider::Tcp => None,
        }
    }

    /// `FI_CONTEXT2`: each operation's `op_context` must be a provider-owned `fi_context2`.
    pub(crate) fn requires_context2(self) -> bool {
        matches!(self, Provider::EfaDirect)
    }

    /// Whether a passive target must poll its completion queue.
    pub(crate) fn needs_manual_progress(self) -> bool {
        !matches!(self, Provider::EfaDirect)
    }
}

/// How to open the local fabric endpoint.
#[derive(Debug, Clone, Default)]
pub struct FabricConfig {
    /// Provider to open.
    provider: Provider,
    /// Fabric domain to pin to, from [`discover_domains`](crate::dma::fabric::discover_domains).
    /// `None` takes the first.
    interface: Option<String>,
    /// Source address to bind the endpoint to. `None` lets the provider choose.
    bind: Option<String>,
    /// Negotiate `FI_HMEM`, without which a dmabuf region cannot be registered.
    hmem: bool,
}

impl FabricConfig {
    /// A config for `provider` with nothing pinned or bound.
    pub fn new(provider: Provider) -> Self {
        Self {
            provider,
            interface: None,
            bind: None,
            hmem: false,
        }
    }

    /// Pin the endpoint to the fabric domain named `interface`.
    pub fn with_interface(mut self, interface: impl Into<String>) -> Self {
        self.interface = Some(interface.into());
        self
    }

    /// Bind the endpoint's source address to `node`.
    pub fn with_bind(mut self, node: impl Into<String>) -> Self {
        self.bind = Some(node.into());
        self
    }

    /// Negotiate `FI_HMEM`, required before [`DmaFabric::register_dmabuf`](crate::dma::fabric::DmaFabric::register_dmabuf).
    pub fn with_hmem(mut self) -> Self {
        self.hmem = true;
        self
    }

    /// The provider to open.
    pub fn provider(&self) -> Provider {
        self.provider
    }

    /// The fabric domain to pin to.
    pub fn interface(&self) -> Option<&str> {
        self.interface.as_deref()
    }

    /// The source address to bind to.
    pub fn bind(&self) -> Option<&str> {
        self.bind.as_deref()
    }

    /// Whether `FI_HMEM` is negotiated.
    pub fn hmem(&self) -> bool {
        self.hmem
    }
}
