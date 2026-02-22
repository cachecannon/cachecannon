//! Redis Cluster topology discovery.
//!
//! Sends `CLUSTER SLOTS` to seed nodes via blocking TCP before ringline launches,
//! builds a slot table mapping each of 16384 slots to an endpoint index.

use resp_proto::{SLOT_COUNT, SlotMap, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// CLUSTER SLOTS command in RESP wire format.
const CLUSTER_SLOTS_CMD: &[u8] = b"*2\r\n$7\r\nCLUSTER\r\n$5\r\nSLOTS\r\n";

/// Connect timeout for seed nodes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Read timeout for CLUSTER SLOTS response.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Discover Redis Cluster topology from seed nodes.
///
/// Returns `(primary_endpoints, slot_table)` where `slot_table` maps each of
/// 16384 hash slots to an index into `primary_endpoints`.
pub fn discover_topology(
    seeds: &[SocketAddr],
) -> Result<(Vec<SocketAddr>, Vec<u16>), Box<dyn std::error::Error>> {
    let mut last_err: Option<Box<dyn std::error::Error>> = None;

    for seed in seeds {
        match try_discover(*seed) {
            Ok(result) => return Ok(result),
            Err(e) => {
                tracing::warn!(seed = %seed, error = %e, "cluster discovery failed for seed");
                last_err = Some(e);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| "no seed nodes provided".into()))
}

fn try_discover(
    seed: SocketAddr,
) -> Result<(Vec<SocketAddr>, Vec<u16>), Box<dyn std::error::Error>> {
    // Connect to seed
    let mut stream = TcpStream::connect_timeout(&seed, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_nodelay(true)?;

    // Send CLUSTER SLOTS
    stream.write_all(CLUSTER_SLOTS_CMD)?;
    stream.flush()?;

    // Read response
    let mut buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 4096];
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Err("connection closed before complete response".into());
        }
        buf.extend_from_slice(&tmp[..n]);

        // Try to parse
        match Value::parse(&buf) {
            Ok((value, _consumed)) => {
                return build_topology(&value);
            }
            Err(resp_proto::ParseError::Incomplete) => {
                // Need more data
                continue;
            }
            Err(e) => {
                return Err(format!("failed to parse CLUSTER SLOTS response: {}", e).into());
            }
        }
    }
}

fn build_topology(
    value: &Value,
) -> Result<(Vec<SocketAddr>, Vec<u16>), Box<dyn std::error::Error>> {
    // Check for error response
    if let Value::Error(msg) = value {
        let msg = String::from_utf8_lossy(msg);
        return Err(format!("CLUSTER SLOTS returned error: {}", msg).into());
    }

    let slot_map = SlotMap::from_cluster_slots(value)
        .ok_or("failed to parse CLUSTER SLOTS response as slot map")?;

    if slot_map.is_empty() {
        return Err("CLUSTER SLOTS returned empty slot map".into());
    }

    // Collect unique primary addresses and build endpoint index
    let mut endpoints: Vec<SocketAddr> = Vec::new();
    let mut addr_to_idx: std::collections::HashMap<SocketAddr, usize> =
        std::collections::HashMap::new();

    for range in slot_map.ranges() {
        let addr: SocketAddr =
            range.primary.address.parse().map_err(|e| {
                format!("invalid primary address '{}': {}", range.primary.address, e)
            })?;
        addr_to_idx.entry(addr).or_insert_with(|| {
            let idx = endpoints.len();
            endpoints.push(addr);
            idx
        });
    }

    // Build slot table: slot → endpoint index
    let mut slot_table = vec![0u16; SLOT_COUNT as usize];
    for range in slot_map.ranges() {
        let addr: SocketAddr = range.primary.address.parse().unwrap();
        let idx = addr_to_idx[&addr];
        for slot in range.start..=range.end {
            slot_table[slot as usize] = idx as u16;
        }
    }

    tracing::info!(
        primaries = endpoints.len(),
        slot_ranges = slot_map.ranges().len(),
        "discovered cluster topology"
    );
    for (idx, addr) in endpoints.iter().enumerate() {
        tracing::info!(idx, addr = %addr, "cluster primary");
    }

    Ok((endpoints, slot_table))
}
