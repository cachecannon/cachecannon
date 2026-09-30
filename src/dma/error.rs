use crate::dma::advertisement::AdvertisementError;

/// Errors from a DMA command or the fabric carrying its payload.
#[derive(Debug, thiserror::Error)]
pub enum DmaError {
    /// The server's reply violated the DMA protocol.
    #[error("dma protocol error: {0}")]
    Protocol(String),
    /// A transfer's byte count or checksum did not match.
    #[error("dma integrity check failed: {0}")]
    Integrity(String),
    /// The value does not fit in the registered buffer.
    #[error("value exceeds the registered buffer")]
    PayloadTooLarge,
    /// The fabric failed.
    #[error("fabric error: {0}")]
    Fabric(String),
    /// The fabric could not be configured as requested.
    #[error("fabric configuration error: {0}")]
    Configuration(String),
}

impl From<AdvertisementError> for DmaError {
    fn from(error: AdvertisementError) -> Self {
        DmaError::Protocol(error.to_string())
    }
}
