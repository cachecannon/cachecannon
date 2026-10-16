#![cfg(feature = "dma")]

use cachecannon::dma::{DmaBuffer, DmaFabric, FabricConfig, Provider, discover_domains};

fn tcp() -> FabricConfig {
    FabricConfig::new(Provider::Tcp)
}

#[test]
fn opens_an_endpoint_with_a_local_address() {
    let fabric = DmaFabric::open(&tcp()).unwrap();
    assert!(!fabric.local_address().is_empty());
}

#[test]
fn registers_host_memory_and_advertises_it() {
    let fabric = DmaFabric::open(&tcp()).unwrap();
    let buffer = fabric.register(vec![0u8; 4096]).unwrap();

    assert_eq!(buffer.capacity(), 4096);
    assert!(!buffer.advertisement().address.is_empty());
}

#[test]
fn stages_and_reads_back_through_the_registered_buffer() {
    let fabric = DmaFabric::open(&tcp()).unwrap();
    let mut buffer = fabric.register(vec![0u8; 64]).unwrap();

    assert_eq!(buffer.copy_from(b"payload"), Some(7));
    assert_eq!(&buffer.as_host_mut().unwrap()[..7], b"payload");
}

#[test]
fn refuses_a_payload_larger_than_the_buffer() {
    let fabric = DmaFabric::open(&tcp()).unwrap();
    let mut buffer = fabric.register(vec![0u8; 4]).unwrap();

    assert_eq!(buffer.copy_from(b"too long"), None);
}

#[test]
fn refuses_an_empty_registration() {
    let fabric = DmaFabric::open(&tcp()).unwrap();
    assert!(fabric.register(Vec::new()).is_err());
}

#[test]
fn registers_more_than_one_buffer_on_one_fabric() {
    let fabric = DmaFabric::open(&tcp()).unwrap();
    let first = fabric.register(vec![0u8; 128]).unwrap();
    let second = fabric.register(vec![0u8; 256]).unwrap();

    assert_eq!(
        first.advertisement().address,
        second.advertisement().address
    );
    assert_eq!(first.capacity(), 128);
    assert_eq!(second.capacity(), 256);
}

#[test]
fn a_buffer_outlives_the_fabric_handle_it_came_from() {
    let buffer = {
        let fabric = DmaFabric::open(&tcp()).unwrap();
        fabric.register(vec![0u8; 32]).unwrap()
    };
    assert_eq!(buffer.capacity(), 32);
}

#[test]
fn discovers_at_least_one_domain() {
    assert!(!discover_domains(&tcp()).unwrap().is_empty());
}

#[test]
fn tcp_needs_a_progress_driver() {
    let fabric = DmaFabric::open(&tcp()).unwrap();
    assert!(fabric.drive_progress().is_some());
}

/// A `DmaBuffer` is `Send`, so it can be registered on one thread and used or dropped on another —
/// what any multi-threaded runtime needs to move one into a spawned task. Deregistration takes the
/// domain lock, so the close cannot race the registrations happening here alongside it.
#[test]
fn a_buffer_registered_on_one_thread_drops_on_another() {
    let fabric = DmaFabric::open(&tcp()).unwrap();
    let buffer = fabric.register(vec![0u8; 4096]).unwrap();

    // Register on this thread while the other thread closes, so the close is concurrent with live
    // domain calls rather than happening on a quiet domain.
    let closing = fabric.clone();
    let mover = std::thread::spawn(move || {
        let capacity = buffer.capacity();
        drop(buffer);
        // Keep the domain busy from this side too.
        drop(closing.register(vec![0u8; 512]).unwrap());
        capacity
    });

    let mut registered = Vec::new();
    for _ in 0..32 {
        registered.push(fabric.register(vec![0u8; 1024]).unwrap());
    }
    assert_eq!(4096, mover.join().unwrap());
    assert_eq!(32, registered.len());
}

/// The compile-time half of the above: losing `Send` would break every spawned-task caller.
#[test]
fn a_buffer_is_send() {
    fn require_send<T: Send>() {}
    require_send::<DmaBuffer>();
}

#[test]
fn a_slice_advertises_a_window_of_one_registration() {
    let fabric = DmaFabric::open(&tcp()).unwrap();
    let buffer = fabric.register(vec![0u8; 4096]).unwrap();
    let whole = buffer.advertisement();

    let window = buffer.slice(1024, 256).unwrap();
    let advertised = window.advertisement();

    // Same region, same key — only the address moves. tcp addresses by offset, so the base is 0 and
    // the window is the offset itself; on a virtual-addressed provider it would be base + 1024.
    assert_eq!(whole.address, advertised.address);
    assert_eq!(whole.remote_key, advertised.remote_key);
    assert_eq!(whole.remote_address + 1024, advertised.remote_address);
    // The length rides with the address, so what was bounds-checked is what goes on the wire.
    assert_eq!(256, window.length());
}

#[test]
fn a_slice_running_past_the_end_is_refused() {
    let fabric = DmaFabric::open(&tcp()).unwrap();
    let buffer = fabric.register(vec![0u8; 4096]).unwrap();

    assert!(buffer.slice(4096, 1).is_none());
    assert!(buffer.slice(0, 4097).is_none());
    assert!(buffer.slice(usize::MAX, 1).is_none());
    // The exact end is in bounds.
    assert!(buffer.slice(4096, 0).is_some());
}

/// The point of a shared source: one allocation, registered on every endpoint, rather than a copy
/// of the bytes per endpoint.
#[test]
fn one_shared_pool_registers_on_several_fabrics() {
    let pool = std::sync::Arc::new(vec![7u8; 8192]);

    let first = DmaFabric::open(&tcp()).unwrap();
    let second = DmaFabric::open(&tcp()).unwrap();
    let one = first.register_shared(pool.clone()).unwrap();
    let two = second.register_shared(pool.clone()).unwrap();

    assert_eq!(one.capacity(), 8192);
    assert_eq!(two.capacity(), 8192);
    // Distinct registrations on distinct domains, both over the same bytes.
    assert_ne!(one.advertisement().address, two.advertisement().address);
    // The pool outlives both registrations, and both hold it.
    assert_eq!(std::sync::Arc::strong_count(&pool), 3);
}

/// A shared source is read by the server and never written, so it exposes no writable mapping.
#[test]
fn a_shared_source_has_no_writable_mapping() {
    let fabric = DmaFabric::open(&tcp()).unwrap();
    let mut buffer = fabric
        .register_shared(std::sync::Arc::new(vec![0u8; 64]))
        .unwrap();

    assert!(buffer.as_host_mut().is_none());
    assert_eq!(buffer.copy_from(b"nope"), None);
}
