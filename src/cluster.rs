//! Valkey/Redis Cluster topology discovery.
//!
//! Sends `CLUSTER SLOTS` to seed nodes via blocking TCP before ringline launches,
//! builds a slot table mapping each of 16384 slots to an endpoint index.

use resp_proto::{Request, SLOT_COUNT, SlotMap, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// CLUSTER SLOTS command in RESP wire format.
const CLUSTER_SLOTS_CMD: &[u8] = b"*2\r\n$7\r\nCLUSTER\r\n$5\r\nSLOTS\r\n";

/// Connect timeout for seed nodes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Read timeout for CLUSTER SLOTS response.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Discover Valkey/Redis Cluster topology from seed nodes.
///
/// `auth` is the `AUTH` argument list (see `Target::auth_args`), sent before
/// `CLUSTER SLOTS` when set.
///
/// Returns `(primary_endpoints, slot_table)` where `slot_table` maps each of
/// 16384 hash slots to an index into `primary_endpoints`.
pub fn discover_topology(
    seeds: &[SocketAddr],
    auth: Option<&[&[u8]]>,
) -> Result<(Vec<SocketAddr>, Vec<u16>), Box<dyn std::error::Error>> {
    let mut last_err: Option<Box<dyn std::error::Error>> = None;

    for seed in seeds {
        match try_discover(*seed, auth) {
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
    auth: Option<&[&[u8]]>,
) -> Result<(Vec<SocketAddr>, Vec<u16>), Box<dyn std::error::Error>> {
    // Connect to seed
    let mut stream = TcpStream::connect_timeout(&seed, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_nodelay(true)?;

    if let Some(args) = auth {
        let mut request = Request::cmd(b"AUTH");
        for arg in args {
            request = request.arg(arg);
        }
        let mut cmd = vec![0u8; request.encoded_len()];
        request.encode(&mut cmd);
        stream.write_all(&cmd)?;
        stream.flush()?;
        if let Value::Error(msg) = read_value(&mut stream, "AUTH")? {
            return Err(format!("AUTH rejected: {}", String::from_utf8_lossy(&msg)).into());
        }
    }

    // Send CLUSTER SLOTS
    stream.write_all(CLUSTER_SLOTS_CMD)?;
    stream.flush()?;

    build_topology(&read_value(&mut stream, "CLUSTER SLOTS")?)
}

/// Read one complete RESP value from a blocking stream.
fn read_value(stream: &mut TcpStream, what: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let mut buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 4096];
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Err("connection closed before complete response".into());
        }
        buf.extend_from_slice(&tmp[..n]);

        match Value::parse(&buf) {
            Ok((value, _consumed)) => return Ok(value),
            Err(resp_proto::ParseError::Incomplete) => continue,
            Err(e) => {
                return Err(format!("failed to parse {what} response: {e}").into());
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

    // Build slot table: slot → endpoint index. Use u16::MAX as the sentinel
    // for "not yet assigned" so we can detect gaps in the response.
    let mut slot_table = vec![u16::MAX; SLOT_COUNT as usize];
    for range in slot_map.ranges() {
        let addr: SocketAddr =
            range.primary.address.parse().map_err(|e| {
                format!("invalid primary address '{}': {}", range.primary.address, e)
            })?;
        let idx = addr_to_idx[&addr];
        for slot in range.start..=range.end {
            slot_table[slot as usize] = idx as u16;
        }
    }

    let unmapped = slot_table.iter().filter(|&&s| s == u16::MAX).count();
    if unmapped > 0 {
        return Err(format!(
            "CLUSTER SLOTS response left {} of {} hash slots unmapped (cluster is degraded or partially migrated); refusing to run with silent slot-0 fallback",
            unmapped, SLOT_COUNT
        )
        .into());
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// One node owning every slot.
    const ALL_SLOTS: &[u8] = b"*1\r\n*3\r\n:0\r\n:16383\r\n*2\r\n$9\r\n127.0.0.1\r\n:7000\r\n";

    /// A seed that answers each request with the next reply and reports the
    /// bytes it received.
    fn fake_seed(replies: Vec<&'static [u8]>) -> (SocketAddr, mpsc::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            for reply in replies {
                let mut buf = [0u8; 1024];
                let n = stream.read(&mut buf).unwrap();
                tx.send(buf[..n].to_vec()).unwrap();
                stream.write_all(reply).unwrap();
            }
        });
        (addr, rx)
    }

    #[test]
    fn sends_auth_before_cluster_slots() {
        let (seed, rx) = fake_seed(vec![b"+OK\r\n", ALL_SLOTS]);
        let args: [&[u8]; 2] = [b"bench", b"pw"];
        let (endpoints, table) = discover_topology(&[seed], Some(&args[..])).unwrap();
        assert_eq!(endpoints, vec!["127.0.0.1:7000".parse().unwrap()]);
        assert_eq!(table.len(), SLOT_COUNT as usize);
        assert_eq!(
            rx.recv().unwrap(),
            b"*3\r\n$4\r\nAUTH\r\n$5\r\nbench\r\n$2\r\npw\r\n"
        );
        assert_eq!(rx.recv().unwrap(), CLUSTER_SLOTS_CMD);
    }

    #[test]
    fn no_auth_sends_cluster_slots_first() {
        let (seed, rx) = fake_seed(vec![ALL_SLOTS]);
        discover_topology(&[seed], None).unwrap();
        assert_eq!(rx.recv().unwrap(), CLUSTER_SLOTS_CMD);
    }

    #[test]
    fn rejected_auth_is_reported() {
        let (seed, _rx) = fake_seed(vec![b"-WRONGPASS invalid username-password pair\r\n"]);
        let args: [&[u8]; 1] = [b"bad"];
        let err = discover_topology(&[seed], Some(&args[..]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("AUTH rejected: WRONGPASS"), "{err}");
        assert!(!err.contains("bad"), "{err}");
    }
}
