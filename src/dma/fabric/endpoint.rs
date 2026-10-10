use std::ffi::{CStr, CString};
use std::os::raw::{c_int, c_void};
use std::ptr;

use ofi_libfabric_sys::bindgen::{
    FI_CONTEXT2, FI_HMEM, FI_MR_ALLOCATED, FI_MR_DMABUF, FI_MR_HMEM, FI_MR_LOCAL, FI_MR_PROV_KEY,
    FI_MR_VIRT_ADDR, FI_MSG, FI_READ, FI_RECV, FI_REMOTE_READ, FI_REMOTE_WRITE, FI_RMA, FI_SOURCE,
    FI_TRANSMIT, FI_WRITE, fi_addr_t, fi_allocinfo, fi_av_attr, fi_av_insert, fi_av_open,
    fi_av_type_FI_AV_MAP, fi_close, fi_cq_attr, fi_cq_format_FI_CQ_FORMAT_CONTEXT, fi_cq_open,
    fi_domain, fi_dupinfo, fi_enable, fi_endpoint, fi_ep_bind, fi_ep_type_FI_EP_RDM, fi_fabric,
    fi_freeinfo, fi_getinfo, fi_getname, fi_hmem_iface_FI_HMEM_CUDA, fi_info, fi_mr_attr,
    fi_mr_dmabuf, fi_mr_key, fi_mr_reg, fi_mr_regattr, fi_strerror, fi_version, fid_av, fid_cq,
    fid_domain, fid_ep, fid_fabric, fid_mr,
};

use crate::dma::error::DmaError;
use crate::dma::fabric::DmaFabric;
use crate::dma::fabric::config::FabricConfig;

/// Turn a libfabric return code into a `Result`. 0 for ok, negative `-errno` otherwise
pub(crate) fn check(code: i32, what: &'static str) -> Result<(), DmaError> {
    if code == 0 {
        Ok(())
    } else {
        // libfabric returns negative errno; fi_strerror wants the positive one.
        let message = unsafe { CStr::from_ptr(fi_strerror(-code)) }.to_string_lossy();
        Err(DmaError::Fabric(format!("{what}: {message} ({code})")))
    }
}

/// A registered memory region. Deregistered on drop under the lock of its domain.
/// Let the registered memory outlive this or you'll be in for exciting UB.
#[derive(Debug)]
pub(crate) struct Registration {
    memory_region: *mut fid_mr,
    remote_key: u64,
    /// The domain this region belongs to, for deregistration. Holding it also keeps
    /// the domain alive for at least as long as the registration.
    fabric: DmaFabric,
}

impl Registration {
    pub(crate) fn new(memory_region: *mut fid_mr, fabric: DmaFabric) -> Self {
        Self {
            remote_key: unsafe { fi_mr_key(memory_region) },
            memory_region,
            fabric,
        }
    }

    pub(crate) fn remote_key(&self) -> u64 {
        self.remote_key
    }
}

// SAFETY: a `Registration`'s only operation after construction is `fi_close` in `Drop`, which
// synchronizes with the domain like `fi_mr_reg`. Deregistration cannot run concurrently with
// any other call into the domain. libfabric allows its objects to move between threads
// provided they are not used concurrently.
unsafe impl Send for Registration {}

impl Drop for Registration {
    fn drop(&mut self) {
        if self.memory_region.is_null() {
            return;
        }
        // SAFETY: this registration owns the region, which is live until this drop closes it.
        let fid = unsafe { &raw mut (*self.memory_region).fid };
        self.fabric.fi_close(fid);
    }
}

/// A `FI_EP_RDM` endpoint owning its fabric, domain, address vector and completion queue.
#[derive(Debug)]
pub(crate) struct LibfabricEndpoint {
    endpoint: *mut fid_ep,
    completion_queue: *mut fid_cq,
    address_vector: *mut fid_av,
    domain: *mut fid_domain,
    fabric: *mut fid_fabric,
    info: *mut fi_info,
    /// `FI_MR_VIRT_ADDR`, as efa does: RMA initiator targets virtual addresses, not offsets.
    uses_virtual_addressing: bool,
    remote_keys: RKeySource,
}

/// Where a registration's remote key comes from, per `FI_MR_PROV_KEY`.
#[derive(Debug)]
enum RKeySource {
    /// The provider assigns the key, so registration must not request one.
    ProviderSelected,
    /// Requesting one key twice on a domain fails the second registration with `-FI_ENOKEY`.
    ApplicationSelected { next_remote_key: u64 },
}

impl RKeySource {
    fn take(&mut self) -> u64 {
        match self {
            RKeySource::ProviderSelected => 0,
            RKeySource::ApplicationSelected { next_remote_key } => {
                let key = *next_remote_key;
                *next_remote_key += 1;
                key
            }
        }
    }
}

// SAFETY: libfabric permits its objects to move between threads so long as they are not used
// concurrently. `DmaFabric` keeps the endpoint in a `Mutex`, so every call through these handles is
// exclusive, and the completion queue the progress thread polls is read through its own pointer.
unsafe impl Send for LibfabricEndpoint {}

/// Build the configured provider's hints and run `fi_getinfo`. The caller selects an entry and frees
/// the list with `fi_freeinfo`.
pub(crate) fn query_info(config: &FabricConfig) -> Result<*mut fi_info, DmaError> {
    let provider = CString::new(config.provider().as_str())
        .map_err(|_| DmaError::Configuration("invalid provider name".into()))?;
    let fabric_name = match config.provider().fabric_name() {
        Some(name) => Some(
            CString::new(name)
                .map_err(|_| DmaError::Configuration("invalid fabric name".into()))?,
        ),
        None => None,
    };
    let node = match config.bind() {
        Some(node) => Some(
            CString::new(node).map_err(|_| DmaError::Configuration("invalid bind node".into()))?,
        ),
        None => None,
    };

    let mut info: *mut fi_info = ptr::null_mut();
    unsafe {
        let hints = fi_allocinfo();
        if hints.is_null() {
            return Err(DmaError::Fabric("fi_allocinfo failed".into()));
        }
        let mut caps =
            u64::from(FI_MSG | FI_RMA | FI_READ | FI_WRITE | FI_REMOTE_READ | FI_REMOTE_WRITE);
        // Leaving mr_mode 0 makes efa hand back a variant whose RMA still needs virtual addresses
        // while reporting mr_mode=0, which then fails with REMOTE_BAD_ADDRESS.
        let mut mr_mode = FI_MR_LOCAL | FI_MR_ALLOCATED | FI_MR_PROV_KEY | FI_MR_VIRT_ADDR;
        if config.hmem() {
            caps |= FI_HMEM;
            mr_mode |= FI_MR_HMEM;
        }
        (*hints).caps = caps;
        (*(*hints).ep_attr).type_ = fi_ep_type_FI_EP_RDM;
        (*(*hints).domain_attr).mr_mode = mr_mode as i32;
        // addr_format stays unspecified so each provider picks its native format; addresses are
        // exchanged opaquely.
        (*(*hints).fabric_attr).prov_name = provider.as_ptr().cast_mut();
        if let Some(name) = &fabric_name {
            (*(*hints).fabric_attr).name = name.as_ptr().cast_mut();
        }
        if config.provider().requires_context2() {
            (*hints).mode |= FI_CONTEXT2;
        }

        let (node_pointer, flags) = match &node {
            Some(node) => (node.as_ptr(), FI_SOURCE),
            None => (ptr::null(), 0),
        };
        let code = fi_getinfo(
            fi_version(),
            node_pointer,
            ptr::null(),
            flags,
            hints,
            &mut info,
        );
        // Detach these so fi_freeinfo doesn't free the Rust-owned CStrings.
        (*(*hints).fabric_attr).prov_name = ptr::null_mut();
        (*(*hints).fabric_attr).name = ptr::null_mut();
        fi_freeinfo(hints);
        check(code, "fi_getinfo")?;
    }
    if info.is_null() {
        return Err(DmaError::Fabric("no provider matched".into()));
    }
    Ok(info)
}

/// The first entry in `list` whose fabric domain is named `want`, or null.
fn select_domain(list: *mut fi_info, want: &str) -> *mut fi_info {
    let mut node = list;
    // SAFETY: a valid fi_info list from fi_getinfo, only read and walked via `next`.
    unsafe {
        while !node.is_null() {
            let name = (*(*node).domain_attr).name;
            if !name.is_null() && CStr::from_ptr(name).to_str() == Ok(want) {
                return node;
            }
            node = (*node).next;
        }
    }
    ptr::null_mut()
}

impl LibfabricEndpoint {
    pub(crate) fn open(config: &FabricConfig) -> Result<Self, DmaError> {
        let mut endpoint = LibfabricEndpoint {
            endpoint: ptr::null_mut(),
            completion_queue: ptr::null_mut(),
            address_vector: ptr::null_mut(),
            domain: ptr::null_mut(),
            fabric: ptr::null_mut(),
            info: ptr::null_mut(),
            uses_virtual_addressing: false,
            remote_keys: RKeySource::ProviderSelected,
        };

        let list = query_info(config)?;
        unsafe {
            let wanted = config.interface();
            let chosen = match wanted {
                Some(name) => select_domain(list, name),
                None => list,
            };
            if chosen.is_null() {
                fi_freeinfo(list);
                return Err(DmaError::Configuration(format!(
                    "no fabric domain named {:?}",
                    wanted.unwrap_or_default()
                )));
            }
            // Copy the chosen domain out standalone, so nothing downstream selects again.
            endpoint.info = fi_dupinfo(chosen);
            fi_freeinfo(list);
            if endpoint.info.is_null() {
                return Err(DmaError::Fabric("fi_dupinfo failed".into()));
            }

            let mr_mode = (*(*endpoint.info).domain_attr).mr_mode as u32;
            endpoint.uses_virtual_addressing = mr_mode & FI_MR_VIRT_ADDR != 0;
            endpoint.remote_keys = if mr_mode & FI_MR_PROV_KEY != 0 {
                RKeySource::ProviderSelected
            } else {
                RKeySource::ApplicationSelected { next_remote_key: 1 }
            };

            check(
                fi_fabric(
                    (*endpoint.info).fabric_attr,
                    &mut endpoint.fabric,
                    ptr::null_mut(),
                ),
                "fi_fabric",
            )?;
            check(
                fi_domain(
                    endpoint.fabric,
                    endpoint.info,
                    &mut endpoint.domain,
                    ptr::null_mut(),
                ),
                "fi_domain",
            )?;

            let mut av_attr: fi_av_attr = std::mem::zeroed();
            av_attr.type_ = fi_av_type_FI_AV_MAP;
            check(
                fi_av_open(
                    endpoint.domain,
                    &mut av_attr,
                    &mut endpoint.address_vector,
                    ptr::null_mut(),
                ),
                "fi_av_open",
            )?;

            let mut cq_attr: fi_cq_attr = std::mem::zeroed();
            cq_attr.format = fi_cq_format_FI_CQ_FORMAT_CONTEXT;
            check(
                fi_cq_open(
                    endpoint.domain,
                    &mut cq_attr,
                    &mut endpoint.completion_queue,
                    ptr::null_mut(),
                ),
                "fi_cq_open",
            )?;

            check(
                fi_endpoint(
                    endpoint.domain,
                    endpoint.info,
                    &mut endpoint.endpoint,
                    ptr::null_mut(),
                ),
                "fi_endpoint",
            )?;
            check(
                fi_ep_bind(endpoint.endpoint, &mut (*endpoint.address_vector).fid, 0),
                "fi_ep_bind(av)",
            )?;
            check(
                fi_ep_bind(
                    endpoint.endpoint,
                    &mut (*endpoint.completion_queue).fid,
                    u64::from(FI_TRANSMIT | FI_RECV),
                ),
                "fi_ep_bind(cq)",
            )?;
            check(fi_enable(endpoint.endpoint), "fi_enable")?;
        }

        Ok(endpoint)
    }

    pub(crate) fn completion_queue(&self) -> *mut fid_cq {
        self.completion_queue
    }

    pub(crate) fn uses_virtual_addressing(&self) -> bool {
        self.uses_virtual_addressing
    }

    /// The endpoint's local fabric address, to advertise to the server.
    pub(crate) fn local_address(&self) -> Result<Vec<u8>, DmaError> {
        let mut length: usize = 0;
        // The first call discovers the length, returning -FI_ETOOSMALL.
        unsafe {
            fi_getname(&mut (*self.endpoint).fid, ptr::null_mut(), &mut length);
        }
        if length == 0 {
            return Err(DmaError::Fabric("fi_getname returned zero length".into()));
        }
        let mut address = vec![0u8; length];
        check(
            unsafe {
                fi_getname(
                    &mut (*self.endpoint).fid,
                    address.as_mut_ptr().cast(),
                    &mut length,
                )
            },
            "fi_getname",
        )?;
        address.truncate(length);
        Ok(address)
    }

    /// Register host memory for remote RMA access. `buffer` must outlive the [`Registration`].
    pub(crate) fn fi_mr_reg(&mut self, buffer: &[u8]) -> Result<*mut fid_mr, DmaError> {
        self.register(buffer, remote_access())
    }

    /// Register host memory the server may only read. `buffer` must outlive the [`Registration`].
    pub(crate) fn fi_mr_reg_source(&mut self, buffer: &[u8]) -> Result<*mut fid_mr, DmaError> {
        self.register(buffer, source_access())
    }

    fn register(&mut self, buffer: &[u8], access: u64) -> Result<*mut fid_mr, DmaError> {
        let requested_key = self.remote_keys.take();
        let mut memory_region: *mut fid_mr = ptr::null_mut();
        check(
            unsafe {
                fi_mr_reg(
                    self.domain,
                    buffer.as_ptr().cast(),
                    buffer.len(),
                    access,
                    0,
                    requested_key,
                    0,
                    &mut memory_region,
                    ptr::null_mut(),
                )
            },
            "fi_mr_reg",
        )?;
        Ok(memory_region)
    }

    /// Register a dmabuf-exported region for remote RMA access.
    ///
    /// # Safety
    /// The region must stay allocated, mapped and unmoved until the [`Registration`] drops.
    pub(crate) unsafe fn fi_mr_regattr(
        &mut self,
        device_pointer: u64,
        length: usize,
        fd: c_int,
        device_ordinal: i32,
    ) -> Result<*mut fid_mr, DmaError> {
        // With FI_MR_DMABUF the attr's iov union is read as a `fi_mr_dmabuf` describing the exported
        // range, and iface/device tag its owner so the provider maps it for RMA.
        let dmabuf = fi_mr_dmabuf {
            fd,
            offset: 0,
            len: length,
            base_addr: device_pointer as *mut c_void,
        };
        let mut attr: fi_mr_attr = unsafe { std::mem::zeroed() };
        attr.__bindgen_anon_1.dmabuf = &dmabuf;
        attr.iov_count = 1;
        attr.access = remote_access();
        attr.requested_key = self.remote_keys.take();
        attr.iface = fi_hmem_iface_FI_HMEM_CUDA;
        attr.device.cuda = device_ordinal;

        let mut memory_region: *mut fid_mr = ptr::null_mut();
        check(
            unsafe { fi_mr_regattr(self.domain, &attr, FI_MR_DMABUF, &mut memory_region) },
            "fi_mr_regattr",
        )?;
        Ok(memory_region)
    }

    /// Insert the server's address into the address vector. efa-direct requires the target to hold
    /// the initiator's address before any RMA against it.
    pub(crate) fn fi_av_insert(&mut self, peer_address: &[u8]) -> Result<(), DmaError> {
        let mut peer: fi_addr_t = 0;
        let inserted = unsafe {
            fi_av_insert(
                self.address_vector,
                peer_address.as_ptr().cast(),
                1,
                &mut peer,
                0,
                ptr::null_mut(),
            )
        };
        if inserted != 1 {
            return Err(DmaError::Fabric(format!(
                "fi_av_insert inserted {inserted} of 1 addresses"
            )));
        }
        Ok(())
    }
}

fn remote_access() -> u64 {
    u64::from(FI_REMOTE_READ | FI_REMOTE_WRITE | FI_READ | FI_WRITE)
}

/// A source pool is read by the server and never written, so it grants exactly that.
fn source_access() -> u64 {
    u64::from(FI_REMOTE_READ | FI_READ)
}

impl Drop for LibfabricEndpoint {
    fn drop(&mut self) {
        unsafe {
            if !self.endpoint.is_null() {
                fi_close(&mut (*self.endpoint).fid);
            }
            if !self.completion_queue.is_null() {
                fi_close(&mut (*self.completion_queue).fid);
            }
            if !self.address_vector.is_null() {
                fi_close(&mut (*self.address_vector).fid);
            }
            if !self.domain.is_null() {
                fi_close(&mut (*self.domain).fid);
            }
            if !self.fabric.is_null() {
                fi_close(&mut (*self.fabric).fid);
            }
            if !self.info.is_null() {
                fi_freeinfo(self.info);
            }
        }
    }
}
