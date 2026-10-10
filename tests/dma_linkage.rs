#![cfg(feature = "dma")]

use std::ffi::CStr;
use std::ptr;

use ofi_libfabric_sys::bindgen::{
    FI_MAJOR_VERSION, FI_MINOR_VERSION, fi_freeinfo, fi_getinfo, fi_info,
};

fn api_version() -> u32 {
    (FI_MAJOR_VERSION << 16) | FI_MINOR_VERSION
}

fn discovered_providers() -> Vec<String> {
    let mut info: *mut fi_info = ptr::null_mut();
    let status = unsafe {
        fi_getinfo(
            api_version(),
            ptr::null(),
            ptr::null(),
            0,
            ptr::null(),
            &mut info,
        )
    };
    assert_eq!(status, 0, "fi_getinfo returned {status}");
    assert!(!info.is_null());

    let mut providers = Vec::new();
    let mut current = info;
    while !current.is_null() {
        let fabric_attr = unsafe { (*current).fabric_attr };
        if !fabric_attr.is_null() {
            let name = unsafe { (*fabric_attr).prov_name };
            if !name.is_null() {
                let name = unsafe { CStr::from_ptr(name) };
                providers.push(name.to_string_lossy().into_owned());
            }
        }
        current = unsafe { (*current).next };
    }
    unsafe { fi_freeinfo(info) };
    providers
}

#[test]
fn links_against_libfabric_and_finds_tcp() {
    let providers = discovered_providers();
    assert!(
        providers.iter().any(|provider| provider == "tcp"),
        "discovered {providers:?}"
    );
}
