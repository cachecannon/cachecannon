use std::fmt;
use std::sync::Arc;

use crate::dma::advertisement::Advertisement;
use crate::dma::fabric::DmaFabric;
use crate::dma::fabric::endpoint::Registration;

/// A window of a registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionWindow {
    advertisement: Advertisement,
    length: usize,
}

impl RegionWindow {
    /// Where the window starts.
    pub fn advertisement(&self) -> &Advertisement {
        &self.advertisement
    }

    /// How many bytes of the region it covers.
    pub fn length(&self) -> usize {
        self.length
    }
}

/// What backs a buffer's bytes.
enum Backing {
    /// Host memory owned here and writable, so a transfer can land in it.
    Owned(Box<dyn AsMut<[u8]> + Send>),
    /// Host memory shared with the caller, read-only.
    Shared(#[allow(dead_code)] Arc<dyn AsRef<[u8]> + Send + Sync>),
    /// A dmabuf region. The caller owns the allocation and there is no host mapping.
    Device,
}

impl Backing {
    fn describe(&self) -> &'static str {
        match self {
            Self::Owned(_) => "owned",
            Self::Shared(_) => "shared",
            Self::Device => "device",
        }
    }
}

/// Memory registered with a [`DmaFabric`] for the server to RMA against. Owned host memory is
/// writable through [`Self::as_host_mut`]; shared memory is a read-only source; a dmabuf region has
/// no host mapping.
pub struct DmaBuffer {
    /// Declared first so the region deregisters before the domain that owns it closes.
    registration: Registration,
    advertisement: Advertisement,
    length: usize,
    backing: Backing,
    /// Keeps the domain alive for as long as the registration.
    fabric: DmaFabric,
}

impl DmaBuffer {
    pub(crate) fn host(
        registration: Registration,
        advertisement: Advertisement,
        length: usize,
        memory: Box<dyn AsMut<[u8]> + Send>,
        fabric: DmaFabric,
    ) -> Self {
        Self {
            registration,
            advertisement,
            length,
            backing: Backing::Owned(memory),
            fabric,
        }
    }

    /// A read-only source registered on this fabric. The `Arc` keeps the storage mapped for as long
    /// as the registration lives.
    pub(crate) fn shared(
        registration: Registration,
        advertisement: Advertisement,
        length: usize,
        memory: Arc<dyn AsRef<[u8]> + Send + Sync>,
        fabric: DmaFabric,
    ) -> Self {
        Self {
            registration,
            advertisement,
            length,
            backing: Backing::Shared(memory),
            fabric,
        }
    }

    pub(crate) fn device(
        registration: Registration,
        advertisement: Advertisement,
        length: usize,
        fabric: DmaFabric,
    ) -> Self {
        Self {
            registration,
            advertisement,
            length,
            backing: Backing::Device,
            fabric,
        }
    }

    /// Bytes registered, capping what one transfer can move through this buffer.
    pub fn capacity(&self) -> usize {
        self.length
    }

    /// Where this buffer lives, for a transfer command.
    pub fn advertisement(&self) -> &Advertisement {
        &self.advertisement
    }

    /// The advertisement for `[at, at + length)`, or `None` if that runs past the end.
    pub fn slice(&self, at: usize, length: usize) -> Option<RegionWindow> {
        if self.length < at.checked_add(length)? {
            return None;
        }
        Some(RegionWindow {
            advertisement: Advertisement {
                address: self.advertisement.address.clone(),
                remote_key: self.advertisement.remote_key,
                // Works under both addressing modes: a `FI_MR_VIRT_ADDR` provider advertised the
                // buffer's virtual address, an offset-addressed one advertised 0.
                remote_address: self
                    .advertisement
                    .remote_address
                    .checked_add(u64::try_from(at).ok()?)?,
            },
            length,
        })
    }

    /// The fabric this buffer is registered with.
    pub fn fabric(&self) -> &DmaFabric {
        &self.fabric
    }

    /// The host mapping, or `None` for a shared source or a dmabuf region.
    pub fn as_host_mut(&mut self) -> Option<&mut [u8]> {
        match &mut self.backing {
            Backing::Owned(memory) => Some((**memory).as_mut()),
            Backing::Shared(_) | Backing::Device => None,
        }
    }

    /// The host mapping, or `None`.
    pub fn as_host(&mut self) -> Option<&[u8]> {
        self.as_host_mut().map(|memory| &*memory)
    }

    /// Copy `value` into the front of the buffer, returning the bytes staged.
    /// Returns None without staging when there is no writable host mapping.
    pub fn copy_from(&mut self, value: &[u8]) -> Option<usize> {
        let destination = self.as_host_mut()?.get_mut(..value.len())?;
        destination.copy_from_slice(value);
        Some(value.len())
    }
}

impl fmt::Debug for DmaBuffer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DmaBuffer")
            .field("length", &self.length)
            .field("backing", &self.backing.describe())
            .field("advertisement", &self.advertisement)
            .field("registration", &self.registration)
            .finish()
    }
}
