//! The libfabric side: the endpoint, registered memory, and completion-queue progress.

mod buffer;
mod config;
mod endpoint;
mod handle;
mod progress;

pub use buffer::{DmaBuffer, RegionWindow};
pub use config::{FabricConfig, Provider};
pub use handle::{DmaFabric, discover_domains};
pub use progress::ProgressGuard;
