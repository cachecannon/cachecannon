//! Valkey/Redis Cluster topology discovery.
//!
//! Sends `CLUSTER SLOTS` to seed nodes via blocking TCP (TLS when
//! `target.tls` is set) before ringline launches, and builds a slot table
//! mapping each of 16384 slots to an endpoint index.

use crate::config::Target;
use resp_proto::{SLOT_COUNT, SlotMap, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

/// CLUSTER SLOTS command in RESP wire format.
const CLUSTER_SLOTS_CMD: &[u8] = b"*2\r\n$7\r\nCLUSTER\r\n$5\r\nSLOTS\r\n";

/// Connect timeout for seed nodes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Read and write timeout for each step of the exchange, TLS handshake
/// included.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Discover Valkey/Redis Cluster topology from `target.endpoints`, the seed
/// nodes. Sends `AUTH` first when the target has a password, and speaks TLS
/// when `target.tls` is set.
///
/// Returns `(primary_endpoints, slot_table)` where `slot_table` maps each of
/// 16384 hash slots to an index into `primary_endpoints`.
pub fn discover_topology(
    target: &Target,
) -> Result<(Vec<SocketAddr>, Vec<u16>), Box<dyn std::error::Error>> {
    let tls = if target.tls {
        Some(Arc::new(crate::runner::build_tls_client_config(target)?))
    } else {
        None
    };
    let mut last_err: Option<Box<dyn std::error::Error>> = None;

    for seed in &target.endpoints {
        match try_discover(*seed, target, tls.as_ref()) {
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
    target: &Target,
    tls: Option<&Arc<rustls::ClientConfig>>,
) -> Result<(Vec<SocketAddr>, Vec<u16>), Box<dyn std::error::Error>> {
    let stream = TcpStream::connect_timeout(&seed, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.set_nodelay(true)?;

    match tls {
        Some(tls) => {
            // Same server name the workers use: tls_hostname, else the IP.
            let name = match target.tls_hostname.as_deref() {
                Some(host) => rustls::pki_types::ServerName::try_from(host.to_string())?,
                None => rustls::pki_types::ServerName::from(seed.ip()),
            };
            let conn = rustls::ClientConnection::new(Arc::clone(tls), name)?;
            query(&mut rustls::StreamOwned::new(conn, stream), target)
        }
        None => {
            let mut stream = stream;
            query(&mut stream, target)
        }
    }
}

/// Run `AUTH` (when configured) and `CLUSTER SLOTS` on an open connection.
fn query<S: Read + Write>(
    stream: &mut S,
    target: &Target,
) -> Result<(Vec<SocketAddr>, Vec<u16>), Box<dyn std::error::Error>> {
    if let Some(request) = target.auth_request() {
        let mut cmd = vec![0u8; request.encoded_len()];
        request.encode(&mut cmd);
        stream.write_all(&cmd)?;
        stream.flush()?;
        if let Value::Error(msg) = read_value(stream, "AUTH")? {
            return Err(format!("AUTH rejected: {}", String::from_utf8_lossy(&msg)).into());
        }
    }

    stream.write_all(CLUSTER_SLOTS_CMD)?;
    stream.flush()?;

    build_topology(&read_value(stream, "CLUSTER SLOTS")?)
}

/// Read one complete RESP value from a blocking stream.
fn read_value<S: Read>(stream: &mut S, what: &str) -> Result<Value, Box<dyn std::error::Error>> {
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

    /// AUTH as the user `bench` with the password `pw`.
    const AUTH_BENCH: &[u8] = b"*3\r\n$4\r\nAUTH\r\n$5\r\nbench\r\n$2\r\npw\r\n";

    /// A cluster target with the given seed and extra `[target]` keys.
    fn target(seed: SocketAddr, extra: &str) -> Target {
        let config: crate::config::Config = toml::from_str(&format!(
            "[target]\nendpoints = [\"{seed}\"]\ncluster = true\n{extra}\n"
        ))
        .unwrap();
        config.target
    }

    /// Answer each request read from `stream` with the next reply, reporting
    /// the bytes received.
    fn answer<S: Read + Write>(stream: &mut S, replies: Vec<&[u8]>, tx: mpsc::Sender<Vec<u8>>) {
        for reply in replies {
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).unwrap();
            tx.send(buf[..n].to_vec()).unwrap();
            stream.write_all(reply).unwrap();
            stream.flush().unwrap();
        }
    }

    /// A plaintext seed.
    fn fake_seed(replies: Vec<&'static [u8]>) -> (SocketAddr, mpsc::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            answer(&mut stream, replies, tx);
        });
        (addr, rx)
    }

    #[test]
    fn sends_auth_before_cluster_slots() {
        let (seed, rx) = fake_seed(vec![b"+OK\r\n", ALL_SLOTS]);
        let t = target(seed, "auth_user = \"bench\"\nauth_password = \"pw\"");
        let (endpoints, table) = discover_topology(&t).unwrap();
        assert_eq!(endpoints, vec!["127.0.0.1:7000".parse().unwrap()]);
        assert_eq!(table.len(), SLOT_COUNT as usize);
        assert_eq!(rx.recv().unwrap(), AUTH_BENCH);
        assert_eq!(rx.recv().unwrap(), CLUSTER_SLOTS_CMD);
    }

    #[test]
    fn no_auth_sends_cluster_slots_first() {
        let (seed, rx) = fake_seed(vec![ALL_SLOTS]);
        discover_topology(&target(seed, "")).unwrap();
        assert_eq!(rx.recv().unwrap(), CLUSTER_SLOTS_CMD);
    }

    #[test]
    fn rejected_auth_is_reported() {
        let (seed, _rx) = fake_seed(vec![b"-WRONGPASS invalid username-password pair\r\n"]);
        let err = discover_topology(&target(seed, "auth_password = \"bad\""))
            .unwrap_err()
            .to_string();
        assert!(err.contains("AUTH rejected: WRONGPASS"), "{err}");
        assert!(!err.contains("bad"), "{err}");
    }

    #[test]
    fn tls_discovery_sends_auth_inside_tls() {
        let Some(server_config) = crate::autodiscovery::tests::tls_server_config() else {
            return;
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let seed = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let conn = rustls::ServerConnection::new(server_config).unwrap();
            let mut tls = rustls::StreamOwned::new(conn, sock);
            answer(&mut tls, vec![b"+OK\r\n", ALL_SLOTS], tx);
        });
        let t = target(
            seed,
            "tls = true\ntls_verify = false\nauth_user = \"bench\"\nauth_password = \"pw\"",
        );
        let (endpoints, _) = discover_topology(&t).unwrap();
        assert_eq!(endpoints, vec!["127.0.0.1:7000".parse().unwrap()]);
        // The server read these after decrypting them.
        assert_eq!(rx.recv().unwrap(), AUTH_BENCH);
        assert_eq!(rx.recv().unwrap(), CLUSTER_SLOTS_CMD);
    }

    #[test]
    fn tls_discovery_never_writes_auth_in_cleartext() {
        // A plaintext listener behind a TLS target: the first bytes it sees
        // must be a TLS ClientHello, not the AUTH command.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let seed = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).unwrap();
            tx.send(buf[..n].to_vec()).unwrap();
        });
        let t = target(
            seed,
            "tls = true\ntls_verify = false\nauth_user = \"bench\"\nauth_password = \"secretpw\"",
        );
        assert!(discover_topology(&t).is_err());
        let first = rx.recv().unwrap();
        assert_eq!(first.first(), Some(&0x16), "not a TLS handshake record");
        assert!(
            !first.windows(b"secretpw".len()).any(|w| w == b"secretpw"),
            "password sent in cleartext"
        );
    }
}
