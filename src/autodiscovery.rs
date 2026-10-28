//! ElastiCache for Memcached auto discovery.
//!
//! Sends `config get cluster` to a configuration endpoint via blocking TCP
//! (or TLS) before ringline launches and returns the node list. If the server
//! answers `ERROR`, the node list is read from the `AmazonElastiCache:cluster`
//! key instead, which AWS documents for engines before memcached 1.4.14. See
//! <https://docs.aws.amazon.com/AmazonElastiCache/latest/dg/AutoDiscovery.AddingToYourClientLibrary.html>.
//!
//! Discovery runs once. Nodes added or removed during the run are not seen.

use crate::config::Target;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

const CONFIG_GET_CLUSTER: &[u8] = b"config get cluster\r\n";

/// The form AWS documents for engines before memcached 1.4.14.
const GET_CLUSTER_KEY: &[u8] = b"get AmazonElastiCache:cluster\r\n";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound on one seed's exchange after the TCP connect: the TLS handshake and
/// both requests. Every socket read and write is limited to the time left, so
/// a server sending a byte at a time is stopped at the deadline too.
const SEED_DEADLINE: Duration = Duration::from_secs(10);

/// Upper bound on a response, so a server that never sends `END` cannot grow
/// the buffer without limit.
const MAX_RESPONSE: usize = 1 << 20;

/// The cluster configuration returned by a configuration endpoint.
#[derive(Debug, PartialEq, Eq)]
pub struct ClusterConfig {
    /// Incremented by ElastiCache each time the node list changes.
    pub version: u64,
    pub nodes: Vec<SocketAddr>,
}

/// Query each seed in turn and return the first cluster configuration
/// obtained. Uses TLS when `target.tls` is set.
pub fn discover_nodes(target: &Target) -> Result<ClusterConfig, Box<dyn std::error::Error>> {
    discover(target, SEED_DEADLINE)
}

fn discover(
    target: &Target,
    deadline: Duration,
) -> Result<ClusterConfig, Box<dyn std::error::Error>> {
    let tls = if target.tls {
        Some(Arc::new(crate::runner::build_tls_client_config(target)?))
    } else {
        None
    };

    let mut last_err: Option<Box<dyn std::error::Error>> = None;
    for seed in &target.endpoints {
        match try_discover(
            *seed,
            tls.as_ref(),
            target.tls_hostname.as_deref(),
            deadline,
        ) {
            Ok(config) => {
                tracing::info!(
                    seed = %seed,
                    version = config.version,
                    nodes = config.nodes.len(),
                    "discovered memcached cluster"
                );
                for (idx, addr) in config.nodes.iter().enumerate() {
                    tracing::info!(idx, addr = %addr, "memcached node");
                }
                return Ok(config);
            }
            Err(e) => {
                tracing::warn!(seed = %seed, error = %e, "auto discovery failed for seed");
                last_err = Some(format!("auto discovery via {seed}: {e}").into());
            }
        }
    }

    Err(last_err.unwrap_or_else(|| "no seed endpoints provided".into()))
}

fn try_discover(
    seed: SocketAddr,
    tls: Option<&Arc<rustls::ClientConfig>>,
    tls_hostname: Option<&str>,
    deadline: Duration,
) -> Result<ClusterConfig, Box<dyn std::error::Error>> {
    let stream = TcpStream::connect_timeout(&seed, CONNECT_TIMEOUT)?;
    stream.set_nodelay(true)?;
    let stream = DeadlineStream {
        stream,
        at: Instant::now() + deadline,
    };

    let result = match tls {
        Some(tls) => {
            // Same server name the workers use: tls_hostname, else the IP.
            let name = match tls_hostname {
                Some(host) => rustls::pki_types::ServerName::try_from(host.to_string())?,
                None => rustls::pki_types::ServerName::from(seed.ip()),
            };
            let conn = rustls::ClientConnection::new(Arc::clone(tls), name)?;
            query(&mut rustls::StreamOwned::new(conn, stream))
        }
        None => {
            let mut stream = stream;
            query(&mut stream)
        }
    };
    // Maps a timeout while the current request had no reply bytes buffered:
    // in the TLS handshake, in a write, or before the first decrypted byte of
    // either reply. `request` maps a timeout once part of a reply is buffered.
    result.map_err(|e| {
        if e.downcast_ref::<io::Error>().is_some_and(is_timeout) {
            format!("discovery exchange did not finish within {deadline:?}").into()
        } else {
            e
        }
    })
}

/// A `TcpStream` whose reads and writes return a timeout error (see
/// `is_timeout`) once `at` has passed. A socket timeout bounds one call, and a
/// read returns as soon as any byte arrives, so on its own it does not stop a
/// server that sends a byte within each timeout. Each call therefore sets the timeout
/// to the time left.
struct DeadlineStream {
    stream: TcpStream,
    at: Instant,
}

impl DeadlineStream {
    fn arm(&self) -> io::Result<()> {
        let left = self.at.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "discovery deadline passed",
            ));
        }
        self.stream.set_read_timeout(Some(left))?;
        self.stream.set_write_timeout(Some(left))
    }
}

impl Read for DeadlineStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.arm()?;
        self.stream.read(buf)
    }
}

impl Write for DeadlineStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.arm()?;
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// A socket timeout is `WouldBlock` on Unix and may be `TimedOut` on Windows
/// (see `TcpStream::set_read_timeout`). `DeadlineStream::arm` returns
/// `TimedOut` once the deadline has passed.
fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Run the discovery exchange on an open connection.
fn query<S: Read + Write>(stream: &mut S) -> Result<ClusterConfig, Box<dyn std::error::Error>> {
    match request(stream, CONFIG_GET_CLUSTER)? {
        Response::Config(body) => parse_config(&body),
        Response::Error(line) if line == "ERROR" => match request(stream, GET_CLUSTER_KEY)? {
            Response::Config(body) => parse_config(&body),
            Response::Empty => Err("server has no cluster configuration (`config get cluster` \
                                    is unknown and `AmazonElastiCache:cluster` is missing); \
                                    the endpoint is probably not an ElastiCache \
                                    configuration endpoint"
                .into()),
            Response::Error(line) => Err(format!("get AmazonElastiCache:cluster: {line}").into()),
        },
        Response::Error(line) => Err(format!("config get cluster: {line}").into()),
        Response::Empty => Err("config get cluster returned no configuration".into()),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Response {
    /// The data block between the `CONFIG`/`VALUE` header and `END`.
    Config(Vec<u8>),
    /// `END` with no data block.
    Empty,
    /// An `ERROR`, `CLIENT_ERROR` or `SERVER_ERROR` line, without the CRLF.
    Error(String),
}

fn request<S: Read + Write>(
    stream: &mut S,
    cmd: &[u8],
) -> Result<Response, Box<dyn std::error::Error>> {
    stream.write_all(cmd)?;
    stream.flush()?;

    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 4096];
    loop {
        if let Some(response) = parse_response(&buf)? {
            return Ok(response);
        }
        if buf.len() > MAX_RESPONSE {
            return Err(format!("response exceeded {MAX_RESPONSE} bytes without END").into());
        }
        let n = match stream.read(&mut tmp) {
            Ok(n) => n,
            Err(e) if is_timeout(&e) && !buf.is_empty() => {
                return Err(format!(
                    "reply incomplete at the deadline ({} bytes received)",
                    buf.len()
                )
                .into());
            }
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            return Err("connection closed before complete response".into());
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

/// Parse one response from `buf`, or return `None` if it is incomplete.
///
/// The data block is taken by the length in the header line
/// (`CONFIG cluster <flags> <bytes>` or `VALUE <key> <flags> <bytes>`), and
/// must be followed by `\r\nEND\r\n`.
fn parse_response(buf: &[u8]) -> Result<Option<Response>, Box<dyn std::error::Error>> {
    let Some(eol) = buf.windows(2).position(|w| w == b"\r\n") else {
        return Ok(None);
    };
    let header = std::str::from_utf8(&buf[..eol])?;
    let rest = &buf[eol + 2..];

    if header == "END" {
        return Ok(Some(Response::Empty));
    }
    if header == "ERROR" || header.starts_with("CLIENT_ERROR") || header.starts_with("SERVER_ERROR")
    {
        return Ok(Some(Response::Error(header.to_string())));
    }

    let fields: Vec<&str> = header.split_ascii_whitespace().collect();
    let len = match fields.as_slice() {
        ["CONFIG", "cluster", _flags, len] | ["VALUE", _, _flags, len] => len
            .parse::<usize>()
            .map_err(|e| format!("bad length in header {header:?}: {e}"))?,
        _ => return Err(format!("unexpected response header {header:?}").into()),
    };
    // Also keeps `len + TRAILER.len()` below from overflowing.
    if len > MAX_RESPONSE {
        return Err(format!("data block length {len} exceeds {MAX_RESPONSE} bytes").into());
    }

    const TRAILER: &[u8] = b"\r\nEND\r\n";
    if rest.len() < len + TRAILER.len() {
        return Ok(None);
    }
    if &rest[len..len + TRAILER.len()] != TRAILER {
        return Err(format!("data block of {len} bytes is not followed by END").into());
    }
    Ok(Some(Response::Config(rest[..len].to_vec())))
}

/// Parse the data block: a version line, then a line of space-separated
/// `hostname|ip|port` entries.
fn parse_config(body: &[u8]) -> Result<ClusterConfig, Box<dyn std::error::Error>> {
    let body = std::str::from_utf8(body)?;
    let mut lines = body.lines().map(str::trim).filter(|l| !l.is_empty());

    let version_line = lines.next().ok_or("empty cluster configuration")?;
    let version = version_line
        .parse::<u64>()
        .map_err(|e| format!("bad config version {version_line:?}: {e}"))?;
    let node_line = lines
        .next()
        .ok_or("cluster configuration has no node list")?;

    let mut nodes = Vec::new();
    for entry in node_line.split_ascii_whitespace() {
        let addr = parse_node(entry)?;
        if !nodes.contains(&addr) {
            nodes.push(addr);
        }
    }
    if nodes.is_empty() {
        return Err("cluster configuration lists no nodes".into());
    }

    Ok(ClusterConfig { version, nodes })
}

/// Parse one `hostname|ip|port` entry. The IP is used when present; when the
/// IP field is empty, the hostname is resolved.
fn parse_node(entry: &str) -> Result<SocketAddr, Box<dyn std::error::Error>> {
    let [host, ip, port] = entry.split('|').collect::<Vec<_>>()[..] else {
        return Err(format!("bad node entry {entry:?} (want hostname|ip|port)").into());
    };
    let port: u16 = port
        .parse()
        .map_err(|e| format!("bad port in node entry {entry:?}: {e}"))?;

    if !ip.is_empty() {
        let ip: IpAddr = ip
            .parse()
            .map_err(|e| format!("bad ip in node entry {entry:?}: {e}"))?;
        return Ok(SocketAddr::new(ip, port));
    }

    (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {host}: {e}"))?
        .next()
        .ok_or_else(|| format!("{host} resolved to no addresses").into())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::net::TcpListener;

    /// The example response from the AWS auto discovery documentation.
    const AWS_EXAMPLE: &[u8] = b"CONFIG cluster 0 136\r\n\
        12\n\
        myCluster.pc4ldq.0001.use1.cache.amazonaws.com|10.82.235.120|11211 \
        myCluster.pc4ldq.0002.use1.cache.amazonaws.com|10.80.249.27|11211\n\
        \r\nEND\r\n";

    fn expected() -> ClusterConfig {
        ClusterConfig {
            version: 12,
            nodes: vec![
                "10.82.235.120:11211".parse().unwrap(),
                "10.80.249.27:11211".parse().unwrap(),
            ],
        }
    }

    fn parse_complete(buf: &[u8]) -> Response {
        parse_response(buf).unwrap().expect("complete response")
    }

    #[test]
    fn parses_aws_example() {
        let Response::Config(body) = parse_complete(AWS_EXAMPLE) else {
            panic!("expected a data block");
        };
        assert_eq!(parse_config(&body).unwrap(), expected());
    }

    #[test]
    fn every_truncation_is_incomplete() {
        for n in 0..AWS_EXAMPLE.len() {
            assert!(
                parse_response(&AWS_EXAMPLE[..n]).unwrap().is_none(),
                "{n}-byte prefix parsed as complete"
            );
        }
    }

    #[test]
    fn rejects_wrong_length() {
        let bad = String::from_utf8_lossy(AWS_EXAMPLE).replace(" 136\r\n", " 130\r\n");
        assert!(parse_response(bad.as_bytes()).is_err());
    }

    #[test]
    fn rejects_oversized_length() {
        let header = format!("CONFIG cluster 0 {}\r\n", usize::MAX);
        assert!(parse_response(header.as_bytes()).is_err());
    }

    #[test]
    fn parses_legacy_value_form() {
        let legacy = String::from_utf8_lossy(AWS_EXAMPLE)
            .replace("CONFIG cluster", "VALUE AmazonElastiCache:cluster");
        let Response::Config(body) = parse_complete(legacy.as_bytes()) else {
            panic!("expected a data block");
        };
        assert_eq!(parse_config(&body).unwrap(), expected());
    }

    #[test]
    fn error_lines() {
        assert_eq!(
            parse_complete(b"ERROR\r\n"),
            Response::Error("ERROR".into())
        );
        assert_eq!(
            parse_complete(b"CLIENT_ERROR bad command line format\r\n"),
            Response::Error("CLIENT_ERROR bad command line format".into())
        );
        assert_eq!(parse_complete(b"END\r\n"), Response::Empty);
    }

    #[test]
    fn node_entries() {
        assert_eq!(
            parse_node("h|10.0.0.1|11211").unwrap(),
            "10.0.0.1:11211".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            parse_node("h|2600:1f18::1|11211").unwrap(),
            "[2600:1f18::1]:11211".parse::<SocketAddr>().unwrap()
        );
        // An empty IP falls back to resolving the hostname.
        assert_eq!(
            parse_node("127.0.0.1||11211").unwrap(),
            "127.0.0.1:11211".parse::<SocketAddr>().unwrap()
        );
        assert!(parse_node("h|10.0.0.1").is_err());
        assert!(parse_node("h|10.0.0.1|port").is_err());
    }

    #[test]
    fn duplicate_nodes_are_listed_once() {
        let config =
            parse_config(b"3\na|10.0.0.1|11211 b|10.0.0.1|11211 c|10.0.0.2|11211\n").unwrap();
        assert_eq!(config.nodes.len(), 2);
    }

    #[test]
    fn rejects_empty_node_list() {
        assert!(parse_config(b"3\n\n").is_err());
        assert!(parse_config(b"3\n").is_err());
    }

    /// Serve one connection, answering each request line from `replies` in
    /// order, and return the seed address.
    fn fake_server(replies: Vec<(&'static [u8], &'static [u8])>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            for (want, reply) in replies {
                let mut got = vec![0u8; want.len()];
                sock.read_exact(&mut got).unwrap();
                assert_eq!(got, want);
                sock.write_all(reply).unwrap();
            }
        });
        addr
    }

    fn target(endpoints: Vec<SocketAddr>) -> Target {
        let mut config: crate::config::Config =
            toml::from_str("[target]\nendpoints = [\"127.0.0.1:11211\"]\n").unwrap();
        config.target.endpoints = endpoints;
        config.target
    }

    #[test]
    fn discovers_over_tcp() {
        let seed = fake_server(vec![(CONFIG_GET_CLUSTER, AWS_EXAMPLE)]);
        assert_eq!(discover_nodes(&target(vec![seed])).unwrap(), expected());
    }

    #[test]
    fn falls_back_to_key_on_old_engines() {
        const LEGACY: &[u8] = b"VALUE AmazonElastiCache:cluster 0 19\r\n\
            7\nh|10.0.0.9|11211\n\
            \r\nEND\r\n";
        let seed = fake_server(vec![
            (CONFIG_GET_CLUSTER, b"ERROR\r\n"),
            (GET_CLUSTER_KEY, LEGACY),
        ]);
        let config = discover_nodes(&target(vec![seed])).unwrap();
        assert_eq!(config.version, 7);
        assert_eq!(config.nodes, vec!["10.0.0.9:11211".parse().unwrap()]);
    }

    #[test]
    fn plain_memcached_is_an_error() {
        let seed = fake_server(vec![
            (CONFIG_GET_CLUSTER, b"ERROR\r\n"),
            (GET_CLUSTER_KEY, b"END\r\n"),
        ]);
        let err = discover_nodes(&target(vec![seed])).unwrap_err().to_string();
        assert!(err.contains("configuration endpoint"), "{err}");
        assert!(
            err.contains(&seed.to_string()),
            "error should name the seed: {err}"
        );
    }

    /// A deadline short enough to keep the timeout tests fast.
    const TEST_DEADLINE: Duration = Duration::from_millis(300);

    /// Accept one connection and hold it open, sending nothing.
    fn silent_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (_sock, _) = listener.accept().unwrap();
            std::thread::sleep(TEST_DEADLINE * 4);
        });
        addr
    }

    #[test]
    fn silent_seed_times_out() {
        let err = discover(&target(vec![silent_server()]), TEST_DEADLINE)
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not finish within"), "{err}");
    }

    #[test]
    fn trickling_seed_is_stopped_at_the_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let seed = listener.local_addr().unwrap();
        // A valid header, then one byte of the data block every 50 ms: each
        // read returns well inside any per-read timeout.
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut req = vec![0u8; CONFIG_GET_CLUSTER.len()];
            sock.read_exact(&mut req).unwrap();
            let _ = sock.write_all(b"CONFIG cluster 0 1000\r\n");
            for _ in 0..1000 {
                if sock.write_all(b"x").is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let start = Instant::now();
        let err = discover(&target(vec![seed]), TEST_DEADLINE)
            .unwrap_err()
            .to_string();
        assert!(err.contains("incomplete at the deadline"), "{err}");
        assert!(start.elapsed() < TEST_DEADLINE * 3, "{:?}", start.elapsed());
    }

    #[test]
    fn append_writers_are_checked_against_discovered_nodes() {
        // One seed passes config validation with one writer; the three
        // discovered nodes need three.
        const THREE_NODES: &[u8] = b"CONFIG cluster 0 53\r\n\
            1\na|10.0.0.1|11211 b|10.0.0.2|11211 c|10.0.0.3|11211\n\
            \r\nEND\r\n";
        let seed = fake_server(vec![(CONFIG_GET_CLUSTER, THREE_NODES)]);
        let config: crate::config::Config = toml::from_str(&format!(
            "[target]\nendpoints = [\"{seed}\"]\nprotocol = \"memcache\"\n\
             auto_discovery = true\n\
             [connection]\nconnections = 3\n\
             [workload.keyspace]\ndistribution = \"recency\"\n\
             [workload.append]\nbatch = 10\nevery = \"1s\"\nconnections = 1\n"
        ))
        .unwrap();
        config.validate().expect("one seed passes validation");
        let mut config = config;
        let err = crate::runner::resolve_endpoints(&mut config)
            .unwrap_err()
            .to_string();
        assert!(err.contains("append.connections (1)"), "{err}");
        assert_eq!(config.target.endpoints.len(), 3, "endpoints are the nodes");
    }

    /// Generate a self-signed cert and key with openssl. Returns None when
    /// openssl is unavailable, so the suite still runs without it.
    fn gen_cert(dir: &std::path::Path) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
        let crt = dir.join("c.crt");
        let key = dir.join("c.key");
        let out = std::process::Command::new("openssl")
            .args([
                "req", "-x509", "-newkey", "rsa:2048", "-sha256", "-days", "3650", "-nodes",
            ])
            .args(["-keyout", key.to_str()?, "-out", crt.to_str()?])
            .args(["-subj", "/CN=cachecannon-test"])
            .args(["-addext", "subjectAltName=IP:127.0.0.1"])
            .output()
            .ok()?;
        out.status.success().then_some((crt, key))
    }

    /// A rustls server config with a fresh self-signed certificate, or None
    /// when openssl is unavailable.
    pub(crate) fn tls_server_config() -> Option<Arc<rustls::ServerConfig>> {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};

        let dir = std::env::temp_dir().join(format!(
            "cc-autodiscovery-tls-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let generated = gen_cert(&dir).map(|(crt, key)| {
            let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&crt)
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            let key = PrivateKeyDer::from_pem_file(&key).unwrap();
            Arc::new(
                rustls::ServerConfig::builder()
                    .with_no_client_auth()
                    .with_single_cert(certs, key)
                    .unwrap(),
            )
        });
        let _ = std::fs::remove_dir_all(&dir);
        if generated.is_none() {
            eprintln!("openssl unavailable; skipping TLS discovery test");
        }
        generated
    }

    /// The self-signed leaf is not a CA, so the client does not verify it; the
    /// handshake and the framing over it are what the TLS tests cover.
    fn tls_target(seed: SocketAddr) -> Target {
        let mut t = target(vec![seed]);
        t.tls = true;
        t.tls_verify = false;
        t
    }

    #[test]
    fn discovers_over_tls() {
        let Some(server_config) = tls_server_config() else {
            return;
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let seed = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let conn = rustls::ServerConnection::new(server_config).unwrap();
            let mut tls = rustls::StreamOwned::new(conn, sock);
            let mut got = vec![0u8; CONFIG_GET_CLUSTER.len()];
            tls.read_exact(&mut got).unwrap();
            assert_eq!(got, CONFIG_GET_CLUSTER);
            tls.write_all(AWS_EXAMPLE).unwrap();
            tls.flush().unwrap();
        });
        assert_eq!(discover_nodes(&tls_target(seed)).unwrap(), expected());
    }

    #[test]
    fn tries_the_next_seed() {
        // Bound then dropped, so connecting to it is refused.
        let dead = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let seed = fake_server(vec![(CONFIG_GET_CLUSTER, AWS_EXAMPLE)]);
        assert_eq!(
            discover_nodes(&target(vec![dead, seed])).unwrap(),
            expected()
        );
    }

    #[test]
    fn silent_tls_seed_times_out() {
        // The client blocks in the handshake, inside the first write.
        let err = discover(&tls_target(silent_server()), TEST_DEADLINE)
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not finish within"), "{err}");
    }
}
