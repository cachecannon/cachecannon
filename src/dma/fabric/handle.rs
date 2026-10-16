use std::ffi::CStr;
use std::os::raw::c_int;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ofi_libfabric_sys::bindgen::fi_freeinfo;

use crate::dma::advertisement::Advertisement;
use crate::dma::error::DmaError;
use crate::dma::fabric::buffer::DmaBuffer;
use crate::dma::fabric::config::FabricConfig;
use crate::dma::fabric::endpoint::{LibfabricEndpoint, Registration, query_info};
use crate::dma::fabric::progress::{ProgressDriver, ProgressGuard};

/// An opened endpoint. Every [`DmaBuffer`] holds a clone, so the domain outlives its registrations.
#[derive(Clone, Debug)]
pub struct DmaFabric {
    inner: Arc<FabricInner>,
}

#[derive(Debug)]
struct FabricInner {
    /// Must be declared first so its thread joins before the endpoint closes the queue it
    /// polls. `None` for efa-direct, which generates no completions.
    progress: Option<ProgressDriver>,
    endpoint: Mutex<LibfabricEndpoint>,
    address: Vec<u8>,
    uses_virtual_addressing: bool,
}

impl DmaFabric {
    /// Open an endpoint for `config`.
    pub fn open(config: &FabricConfig) -> Result<Self, DmaError> {
        let endpoint = LibfabricEndpoint::open(config)?;
        let address = endpoint.local_address()?;
        let uses_virtual_addressing = endpoint.uses_virtual_addressing();
        let progress = config
            .provider()
            .needs_manual_progress()
            .then(|| ProgressDriver::new(endpoint.completion_queue()));
        Ok(Self {
            inner: Arc::new(FabricInner {
                progress,
                endpoint: Mutex::new(endpoint),
                address,
                uses_virtual_addressing,
            }),
        })
    }

    /// Register host memory for the server to RMA against. Reuse registrations: they are
    /// expensive, and the server retains the advertised address.
    pub fn register(
        &self,
        memory: impl AsMut<[u8]> + Send + 'static,
    ) -> Result<DmaBuffer, DmaError> {
        let mut memory: Box<dyn AsMut<[u8]> + Send> = Box::new(memory);
        let buffer = (*memory).as_mut();
        let (pointer, length) = (buffer.as_ptr() as u64, buffer.len());
        if 0 == length {
            return Err(DmaError::Configuration("cannot register 0 bytes".into()));
        }
        let memory_region = self.endpoint().fi_mr_reg(buffer)?;
        let registration = Registration::new(memory_region, self.clone());
        let advertisement = self.advertisement(pointer, &registration);
        Ok(DmaBuffer::host(
            registration,
            advertisement,
            length,
            memory,
            self.clone(),
        ))
    }

    /// Register shared memory as a read-only (`FI_REMOTE_READ`) source. One allocation can be
    /// registered on every fabric; each registration counts against `RLIMIT_MEMLOCK`.
    pub fn register_shared<S>(&self, memory: Arc<S>) -> Result<DmaBuffer, DmaError>
    where
        S: AsRef<[u8]> + Send + Sync + 'static,
    {
        let bytes: &[u8] = (*memory).as_ref();
        let (pointer, length) = (bytes.as_ptr() as u64, bytes.len());
        if 0 == length {
            return Err(DmaError::Configuration("cannot register 0 bytes".into()));
        }
        let memory_region = self.endpoint().fi_mr_reg_source(bytes)?;
        let registration = Registration::new(memory_region, self.clone());
        let advertisement = self.advertisement(pointer, &registration);
        Ok(DmaBuffer::shared(
            registration,
            advertisement,
            length,
            memory,
            self.clone(),
        ))
    }

    /// Register a dmabuf-exported region for the server to RMA against, so a transfer lands in
    /// device memory. The caller owns the allocation and the descriptor.
    ///
    /// Requires [`FabricConfig::with_hmem`].
    ///
    /// # Safety
    /// The region must stay allocated, mapped and unmoved until the returned [`DmaBuffer`] drops.
    pub unsafe fn register_dmabuf(
        &self,
        device_pointer: u64,
        length: usize,
        fd: c_int,
        device_ordinal: i32,
    ) -> Result<DmaBuffer, DmaError> {
        if 0 == length {
            return Err(DmaError::Configuration("cannot register 0 bytes".into()));
        }
        let memory_region = unsafe {
            self.endpoint()
                .fi_mr_regattr(device_pointer, length, fd, device_ordinal)?
        };
        let registration = Registration::new(memory_region, self.clone());
        let advertisement = self.advertisement(device_pointer, &registration);
        Ok(DmaBuffer::device(
            registration,
            advertisement,
            length,
            self.clone(),
        ))
    }

    /// Insert a server address into the address vector. efa-direct requires the target to hold the
    /// initiator's address before any RMA.
    pub fn insert_peer(&self, address: &[u8]) -> Result<(), DmaError> {
        self.endpoint().fi_av_insert(address)
    }

    /// Drive progress until the guard drops. `None` when the provider needs no polling.
    #[must_use]
    pub fn drive_progress(&self) -> Option<ProgressGuard> {
        self.inner.progress.as_ref().map(ProgressDriver::drive)
    }

    /// This endpoint's local fabric address.
    pub fn local_address(&self) -> &[u8] {
        &self.inner.address
    }

    /// Close a fid belonging to this domain.
    pub(crate) fn fi_close(&self, fid: *mut ofi_libfabric_sys::bindgen::fid) {
        // this is the domain synchronization lock
        let _domain = self.endpoint();
        // SAFETY: the caller owns `fid`, which is live, and is closing it exactly once. The guard
        // above excludes every other call into this domain for the duration.
        unsafe { ofi_libfabric_sys::bindgen::fi_close(fid) };
    }

    fn endpoint(&self) -> MutexGuard<'_, LibfabricEndpoint> {
        self.inner
            .endpoint
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn advertisement(&self, remote_address: u64, registration: &Registration) -> Advertisement {
        Advertisement {
            address: self.inner.address.clone(),
            remote_key: registration.remote_key(),
            remote_address: if self.inner.uses_virtual_addressing {
                remote_address
            } else {
                // for offset-addressed providers like tcp
                0
            },
        }
    }
}

/// Every fabric domain the provider exposes, one per card, in libfabric's order.
pub fn discover_domains(config: &FabricConfig) -> Result<Vec<String>, DmaError> {
    let list = query_info(config)?;
    let mut names: Vec<String> = Vec::new();
    // SAFETY: a valid fi_info list from fi_getinfo, read, then freed once.
    unsafe {
        let mut node = list;
        while !node.is_null() {
            let current = node;
            node = (*current).next;
            let name = (*(*current).domain_attr).name;
            if name.is_null() {
                continue;
            }
            let Ok(name) = CStr::from_ptr(name).to_str() else {
                continue;
            };
            if !names.iter().any(|existing| existing == name) {
                names.push(name.to_string());
            }
        }
        fi_freeinfo(list);
    }
    Ok(names)
}
