//! `protocol = "dma"`: RESP carries the commands, libfabric carries the payload.

pub mod advertisement;
pub mod checksum;
mod client;
pub mod command;
mod connection;
pub mod error;
pub mod fabric;

pub use advertisement::{Advertisement, AdvertisementError, decode_hex, encode_hex};
pub use checksum::checksum;
pub use command::{
    Dialect, DmaCommand, DmaGetOptions, DmaSetOptions, TransferReceipt, TransferReply,
};
pub(crate) use connection::{dma_connection_task, open_fabric};
pub use error::DmaError;
pub use fabric::{
    DmaBuffer, DmaFabric, FabricConfig, ProgressGuard, Provider, RegionWindow, discover_domains,
};
