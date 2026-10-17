//! Per-request key-selection cost on one core, for each multi-endpoint path a
//! connection task can take. No network; nothing is sent.
//!
//! ```text
//! cargo run --release --example zipfbench -- [keys] [endpoints] [theta] [threads] [base_addr] [connections]
//! cargo run --release --example zipfbench -- 100000 48 0.99 48 10.0.0.5:6379 1536
//! ```
//!
//! Defaults: 100,000 keys, 48 endpoints, theta 0.99, 1 thread, 172.31.0.10:6379,
//! 1,536 connections.
//!
//! First prints one line per endpoint: index, address, keys owned, zipf share
//! (the fraction of requests it should receive), and the connections
//! `endpoint_allocation = "key_share"` gives it out of `connections`. Compare
//! a run's per-instance request fractions against the share column.
//! Endpoint `i` is `base_addr` with port `+ i`, the shape of a run with one
//! server instance per port; the ketama partition depends on these strings, so
//! pass the real server address to reproduce a run's partition. Keys are
//! 16-byte hex, the default `keyspace.length`.
//!
//! Paths, per accepted key:
//! - `reject`: global zipf draw, render, RwLock read, ketama route, repeated
//!   until the key routes to the connection's endpoint (the zipf path before
//!   the per-endpoint alias tables).
//! - `alias`: `EndpointZipf::sample` plus render (zipf now).
//! - `uniform`: index into the endpoint's key list plus render (uniform).
//!
//! Each thread is independent (own RNG, read-only shared tables), so with
//! `threads > 1` the per-thread figures show whether anything is shared.

use cachecannon::keydist::KeyDist;
use cachecannon::worker::{build_endpoint_keys, key_share_endpoints, write_key};
use rand::{Rng, SeedableRng};
use rand_xoshiro::Xoshiro256PlusPlus;
use std::hint::black_box;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Instant;

const KEY_LEN: usize = 16;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize, default: &str| args.get(i).cloned().unwrap_or(default.to_string());
    let n: usize = arg(0, "100000").parse().expect("keys");
    let eps: usize = arg(1, "48").parse().expect("endpoints");
    let theta: f64 = arg(2, "0.99").parse().expect("theta");
    let threads: usize = arg(3, "1").parse().expect("threads");
    let base: SocketAddr = arg(4, "172.31.0.10:6379").parse().expect("base_addr");
    let connections: usize = arg(5, "1536").parse().expect("connections");
    assert!(
        eps >= 2,
        "the per-endpoint paths need at least two endpoints"
    );

    let endpoints: Vec<SocketAddr> = (0..eps)
        .map(|i| {
            let port = u16::try_from(i)
                .ok()
                .and_then(|i| base.port().checked_add(i))
                .unwrap_or_else(|| {
                    eprintln!("base port {} + {eps} endpoints exceeds 65535", base.port());
                    std::process::exit(2);
                });
            SocketAddr::new(base.ip(), port)
        })
        .collect();
    let ids: Vec<String> = endpoints.iter().map(|a| a.to_string()).collect();
    let ring = Arc::new(ketama::Ring::build(
        &ids.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
    ));
    let keys = Arc::new(build_endpoint_keys(n, KEY_LEN, &endpoints, &None));
    let t = Instant::now();
    let dist = Arc::new(KeyDist::zipf(n, theta).with_endpoint_keys(&keys));
    let build_ms = t.elapsed().as_secs_f64() * 1e3;
    let shares = dist.endpoint_sampler().unwrap().shares().to_vec();
    // Endpoints that own no keys have share 0 and are left out of min/max.
    let (lo, hi) = shares
        .iter()
        .filter(|&&s| s > 0.0)
        .fold((f64::MAX, 0.0f64), |(l, h), &s| (l.min(s), h.max(s)));
    let sizes: Vec<usize> = keys.iter().map(|b| b.len()).collect();
    println!(
        "keys={n} endpoints={eps} theta={theta} threads={threads} alias build {build_ms:.1} ms"
    );
    let empty = sizes.iter().filter(|&&s| s == 0).count();
    let mut conns = vec![0usize; eps];
    for e in key_share_endpoints(&shares, connections) {
        conns[e] += 1;
    }
    println!("endpoint  address                keys  zipf_share  key_share_conns/{connections}");
    for e in 0..eps {
        println!(
            "{e:>8}  {:<21} {:>6}  {:>10.6}  {:>6}",
            endpoints[e].to_string(),
            sizes[e],
            shares[e],
            conns[e]
        );
    }
    println!(
        "endpoint keys min {} max {} ({empty} endpoints own none, skipped below); \
         zipf share over endpoints with keys min {lo:.4} max {hi:.4}",
        sizes.iter().min().unwrap(),
        sizes.iter().max().unwrap()
    );

    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let (dist, keys, ring) = (Arc::clone(&dist), Arc::clone(&keys), Arc::clone(&ring));
            std::thread::spawn(move || bench_thread(t as u64, &dist, &keys, &ring))
        })
        .collect();
    println!("thread  reject_ns  attempts  alias_ns  uniform_ns");
    let mut sum = [0.0f64; 3];
    for (t, h) in handles.into_iter().enumerate() {
        let r = h.join().unwrap();
        println!(
            "{t:>6} {:>10.0} {:>9.1} {:>9.1} {:>11.1}",
            r[0], r[1], r[2], r[3]
        );
        sum[0] += r[0];
        sum[1] += r[2];
        sum[2] += r[3];
    }
    let k = threads as f64;
    println!(
        "mean    reject {:.0} ns, alias {:.1} ns, uniform {:.1} ns per key; \
         keys/s/thread reject {:.0}, alias {:.0}, uniform {:.0}",
        sum[0] / k,
        sum[1] / k,
        sum[2] / k,
        1e9 * k / sum[0],
        1e9 * k / sum[1],
        1e9 * k / sum[2]
    );
}

/// Returns [reject ns/key, reject attempts/key, alias ns/key, uniform ns/key].
fn bench_thread(seed: u64, dist: &KeyDist, keys: &[Vec<u32>], ring: &ketama::Ring) -> [f64; 4] {
    let mut rng = Xoshiro256PlusPlus::seed_from_u64(seed);
    let mut kb = [0u8; KEY_LEN];
    // Stands in for `route_key`'s per-call read lock on the slot table.
    let slot_table: RwLock<Option<Vec<u16>>> = RwLock::new(None);
    let by_ep = dist.endpoint_sampler().unwrap();
    // Round-robin over the endpoints that own keys, as connections are, so
    // attempts average 1 / P(endpoint) over them. An endpoint with no keys has
    // no key to find (the reject loop would never end) and no alias table.
    let live: Vec<usize> = (0..keys.len()).filter(|&e| !keys[e].is_empty()).collect();
    let reject_keys = 100_000;
    let fast_keys = 10_000_000;

    let mut attempts = 0usize;
    let t = Instant::now();
    for i in 0..reject_keys {
        let ep = live[i % live.len()];
        loop {
            let c = dist.sample(&mut rng);
            write_key(&mut kb, c);
            attempts += 1;
            let guard = slot_table.read().unwrap();
            black_box(&*guard);
            drop(guard);
            if ring.route(&kb) == ep {
                break;
            }
        }
    }
    let reject = t.elapsed().as_nanos() as f64 / reject_keys as f64;

    let t = Instant::now();
    for i in 0..fast_keys {
        let id = by_ep
            .sample(live[i % live.len()], &mut rng)
            .expect("live endpoints own keys");
        write_key(&mut kb, id);
        black_box(&kb);
    }
    let alias = t.elapsed().as_nanos() as f64 / fast_keys as f64;

    let t = Instant::now();
    for i in 0..fast_keys {
        let bucket = &keys[live[i % live.len()]];
        write_key(&mut kb, bucket[rng.random_range(0..bucket.len())] as usize);
        black_box(&kb);
    }
    let uniform = t.elapsed().as_nanos() as f64 / fast_keys as f64;

    [reject, attempts as f64 / reject_keys as f64, alias, uniform]
}
