//! The `protocol = "dma"` connection task.
//!
//! The client is a passive RMA target.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::client::{DmaClient, Error};
use crate::dma::{
    Dialect, DmaBuffer, DmaFabric, FabricConfig, Provider, RegionWindow, discover_domains,
};
use rand::{RngCore, SeedableRng};
use rand_xoshiro::Xoshiro256PlusPlus;

use crate::client::{RequestResult, RequestType};
use crate::config::{DmaModule, DmaProvider};
use crate::metrics;
use crate::worker::{
    DisconnectReason, Phase, SharedWorkerState, confirm_prefill_key, establish_connection,
    record_counters, value_offset, write_key,
};

thread_local! {
    /// This worker's fabric endpoint. Never shared: each endpoint is one refcounted entry in the
    /// server's address vector.
    static FABRIC: RefCell<Option<DmaFabric>> = const { RefCell::new(None) };

    /// The value pool, registered read-only on this worker's fabric. Every worker registers the
    /// same allocation, so the pages are pinned once.
    static SOURCE: RefCell<Option<DmaBuffer>> = const { RefCell::new(None) };
}

/// A window of this worker's registered value pool, or `None` if the pool never registered.
fn source_window(at: usize, length: usize) -> Option<RegionWindow> {
    SOURCE.with(|slot| slot.borrow().as_ref()?.slice(at, length))
}

/// Open this worker's fabric endpoint on card `worker_id % cards`. False if it will not open.
pub(crate) fn open_fabric(
    worker_id: usize,
    config: &crate::config::Config,
    value_pool: &Arc<Vec<u8>>,
) -> bool {
    let provider = match config.dma.provider {
        DmaProvider::EfaDirect => Provider::EfaDirect,
        DmaProvider::Tcp => Provider::Tcp,
    };

    let interfaces = if config.dma.interfaces.is_empty() {
        match discover_domains(&FabricConfig::new(provider)) {
            Ok(found) => found,
            Err(error) => {
                tracing::error!(worker = worker_id, "fabric discovery failed: {error}");
                return false;
            }
        }
    } else {
        config.dma.interfaces.clone()
    };

    if interfaces.is_empty() {
        tracing::error!(worker = worker_id, "no fabric domains found");
        return false;
    }

    let interface = &interfaces[worker_id % interfaces.len()];
    let fabric_config = FabricConfig::new(provider).with_interface(interface.clone());
    let fabric = match DmaFabric::open(&fabric_config) {
        Ok(fabric) => fabric,
        Err(error) => {
            tracing::error!(worker = worker_id, interface, "fabric open failed: {error}");
            return false;
        }
    };

    // Registered read-only, so no server can write into a pool every worker is serving from. This
    // is the slow step at startup: `fi_mr_reg` pins the pool's pages, once per worker.
    match fabric.register_shared(Arc::clone(value_pool)) {
        Ok(source) => SOURCE.with(|slot| *slot.borrow_mut() = Some(source)),
        Err(error) => {
            tracing::error!(
                worker = worker_id,
                interface,
                bytes = value_pool.len(),
                "registering the value pool failed: {error}"
            );
            return false;
        }
    }

    tracing::debug!(
        worker = worker_id,
        interface,
        pool_bytes = value_pool.len(),
        "fabric open"
    );
    FABRIC.with(|slot| *slot.borrow_mut() = Some(fabric));
    true
}

/// A clone of this worker's fabric, or `None` if it never opened.
fn fabric() -> Option<DmaFabric> {
    FABRIC.with(|slot| slot.borrow().clone())
}

/// One DMA connection: connect, handshake, then run transfers until the phase says stop.
pub(crate) async fn dma_connection_task(
    state: Arc<SharedWorkerState>,
    endpoint_idx: usize,
    seed: u64,
) {
    let endpoint = state.task_state.endpoints[endpoint_idx];
    let config = &state.task_state.config;
    let worker_id = state.task_state.worker_id;
    let capacity = config
        .dma
        .buffer_capacity
        .unwrap_or(config.workload.values.length);
    let dialect = match config.dma.module {
        DmaModule::Vdma => Dialect::Vdma,
        DmaModule::LargeObj => Dialect::LargeObj,
    };

    let Some(fabric) = fabric() else {
        return; // the worker logged why at startup
    };

    let mut rng = Xoshiro256PlusPlus::seed_from_u64(seed);
    let mut key_buf = vec![0u8; config.workload.keyspace.length];
    let mut backfill_queue: Vec<usize> = Vec::new();

    loop {
        if state.task_state.shared.phase().should_stop() {
            return;
        }

        let conn = match establish_connection(
            endpoint,
            worker_id,
            None,
            config.connection.connect_timeout,
        )
        .await
        {
            Ok(conn) => conn,
            Err(_) => {
                ringline::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };

        metrics::CONNECTIONS_ACTIVE.increment();
        let client = match ringline_redis::Client::new(conn) {
            Ok(client) => client,
            Err(error) => {
                tracing::debug!(worker = worker_id, %endpoint, "RESP client setup failed: {error}");
                conn.close();
                metrics::CONNECTIONS_ACTIVE.decrement();
                metrics::CONNECTIONS_FAILED.increment();
                ringline::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        // Registration happens here, not on first use: `fi_mr_reg` pins pages and is slow, and
        // paying it inside the measured phase would land in the latency histogram.
        let mut dma = match DmaClient::connect(client, fabric.clone(), capacity, dialect).await {
            Ok(dma) => dma.with_checksum(config.dma.checksum),
            Err(error) => {
                tracing::debug!(worker = worker_id, %endpoint, "dma handshake failed: {error}");
                metrics::CONNECTIONS_ACTIVE.decrement();
                metrics::CONNECTIONS_FAILED.increment();
                ringline::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };

        // The hello answered, so the module is loaded and the fabric address is accepted.
        if !state.is_precheck_done() {
            state.mark_precheck_done();
        }

        let reason = run_transfers(
            &mut dma,
            &state,
            endpoint_idx,
            &mut rng,
            &mut key_buf,
            &mut backfill_queue,
        )
        .await;
        metrics::CONNECTIONS_ACTIVE.decrement();
        if let Err(reason) = reason {
            tracing::debug!(worker = worker_id, %endpoint, "disconnected: {reason:?}");
        }
    }
}

/// Issue transfers on an established connection until it fails or the run ends.
async fn run_transfers(
    dma: &mut DmaClient,
    state: &Arc<SharedWorkerState>,
    endpoint_idx: usize,
    rng: &mut Xoshiro256PlusPlus,
    key_buf: &mut [u8],
    backfill_queue: &mut Vec<usize>,
) -> Result<(), DisconnectReason> {
    let config = &state.task_state.config;
    let key_count = config.workload.keyspace.count;
    let get_ratio = config.workload.commands.get as usize;
    let value_len = config.workload.values.length;
    let pool_len = state.task_state.value_pool.len();
    let backfill_on_miss = state.task_state.backfill_on_miss;

    loop {
        let phase = state.task_state.shared.phase();
        if phase.should_stop() {
            return Ok(());
        }
        // Precheck is the runner's connectivity gate and the connect already satisfied it. Issuing
        // traffic here would count operations against a phase that is not measuring any.
        if phase == Phase::Precheck {
            ringline::sleep(Duration::from_micros(200)).await;
            continue;
        }

        // Prefill drains the shared queue; every other phase picks from the keyspace.
        let prefill = phase == Phase::Prefill && !state.is_prefill_done();
        let (key_id, is_set, is_prefill) = if prefill {
            match state.task_state.prefill_queues.pop_front(endpoint_idx) {
                Some(key_id) => (key_id, true, true),
                None => {
                    ringline::sleep(Duration::from_micros(100)).await;
                    continue;
                }
            }
        } else if let Some(key_id) = backfill_queue.pop() {
            (key_id, true, false)
        } else {
            if let Some(limiter) = state.task_state.ratelimiter.as_ref()
                && limiter.try_wait().is_err()
            {
                ringline::sleep(Duration::from_micros(100)).await;
                continue;
            }
            let key_id = (rng.next_u64() % key_count.max(1) as u64) as usize;
            let is_set = (rng.next_u64() % 100) as usize >= get_ratio;
            (key_id, is_set, false)
        };

        write_key(key_buf, key_id);
        metrics::REQUESTS_SENT.increment();
        let start = Instant::now();

        let (outcome, request_type, hit) = if is_set {
            // No staging copy: the value is already in the registered pool, so the transfer
            // advertises the window where it lives and the server reads it from there.
            let at = value_offset(rng, value_len, pool_len);
            let checksum =
                state.task_state.config.dma.checksum.then(|| {
                    crate::dma::checksum(&state.task_state.value_pool[at..at + value_len])
                });
            let outcome = match source_window(at, value_len) {
                Some(window) => dma.set_registered(key_buf, &window, checksum).await,
                None => Err(Error::Dma(crate::dma::DmaError::Fabric(
                    "the value pool is not registered on this worker".into(),
                ))),
            };
            match outcome {
                Ok(receipt) => (Ok(receipt.bytes_written), RequestType::Set, None),
                Err(error) => (Err(error), RequestType::Set, None),
            }
        } else {
            match dma.get(key_buf).await {
                Ok(Some(receipt)) => (Ok(receipt.bytes_written), RequestType::Get, Some(true)),
                Ok(None) => (Ok(0), RequestType::Get, Some(false)),
                Err(error) => (Err(error), RequestType::Get, None),
            }
        };
        let latency_ns = start.elapsed().as_nanos() as u64;

        let bytes = match &outcome {
            Ok(bytes) => *bytes,
            Err(_) => 0,
        };
        // The payload moved over the fabric, so the socket counters would report a few hundred
        // bytes for a multi-megabyte transfer. Count what actually moved.
        if is_set {
            metrics::BYTES_TX.add(bytes as u64);
            let _ = metrics::SET_LATENCY.increment(latency_ns);
        } else {
            metrics::BYTES_RX.add(bytes as u64);
            let _ = metrics::GET_LATENCY.increment(latency_ns);
        }
        let _ = metrics::RESPONSE_LATENCY.increment(latency_ns);
        let _ = metrics::PERCEIVED_LATENCY
            .increment(latency_ns + metrics::CURRENT_SLIP_NS.value() as u64);

        let failed = outcome.is_err();
        record_counters(&RequestResult {
            id: key_id as u64,
            success: !failed,
            is_error_response: false,
            latency_ns,
            // The payload never crosses the socket, so there is no first byte to timestamp.
            ttfb_ns: None,
            request_type,
            hit,
            key_id: Some(key_id),
            backfill: is_set && !is_prefill && !prefill,
            prefill: is_prefill,
            append: false,
            redirect: None,
        });

        if backfill_on_miss && hit == Some(false) {
            backfill_queue.push(key_id);
        }
        // A prefill key is confirmed only once its SET actually succeeded; a failed one goes back on
        // the queue so the phase cannot finish having silently skipped keys.
        if is_prefill {
            if failed {
                state
                    .task_state
                    .prefill_queues
                    .push_front(endpoint_idx, key_id);
            } else {
                confirm_prefill_key(state);
            }
        }
        // A failed transfer takes the connection down rather than being retried in place: the
        // failure may be the fabric or the control channel, and this path cannot tell which.
        if failed {
            return Err(DisconnectReason::RecvError);
        }
    }
}
