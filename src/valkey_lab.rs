use cachecannon::config::{
    Commands, Config, Connection, Distribution, General, Protocol, SaturationSearch, SloThresholds,
    Target, Values, Workload,
};
use cachecannon::output::create_formatter_with_banner;
use cachecannon::viewer;
use cachecannon::{parse_cpu_list, run_benchmark_full};

use clap::{Parser, Subcommand, ValueEnum};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// High-performance Valkey/Redis benchmark tool.
///
/// A drop-in benchmarking CLI compatible with valkey-benchmark conventions,
/// powered by the cachecannon engine with io_uring and per-connection pipelining.
#[derive(Parser, Debug)]
#[command(name = "valkey-lab")]
#[command(version)]
#[command(args_conflicts_with_subcommands = true)]
#[command(disable_help_flag = true)]
struct Cli {
    /// Print help information.
    #[arg(long, action = clap::ArgAction::Help)]
    help: Option<bool>,

    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    bench: BenchArgs,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Find the maximum throughput that meets latency SLOs.
    Saturate(Box<SaturateArgs>),
    /// View benchmark results from a parquet file in a web dashboard.
    View(viewer::ViewArgs),
}

// ---------------------------------------------------------------------------
// Benchmark flags (used by the default run and shared with `saturate`)
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
struct BenchArgs {
    // -- Tier 1 flags -------------------------------------------------------
    /// Server hostname or IP address.
    #[arg(short = 'h', long)]
    host: Option<String>,

    /// Server port.
    #[arg(short = 'p', long)]
    port: Option<u16>,

    /// Test duration (e.g. 60, 30s, 5m, 1h).
    #[arg(short = 'd', long, value_parser = parse_duration)]
    duration: Option<Duration>,

    /// Number of connections.
    #[arg(short = 'c', long)]
    connections: Option<usize>,

    /// Pipeline depth (requests in flight per connection).
    #[arg(short = 'P', long)]
    pipeline: Option<usize>,

    /// Number of worker threads (default: CPU count).
    #[arg(short = 't', long)]
    threads: Option<usize>,

    /// GET:SET ratio (e.g. "80:20").
    #[arg(short = 'r', long)]
    ratio: Option<String>,

    /// Number of unique keys in the keyspace.
    #[arg(short = 'n', long)]
    keyspace: Option<usize>,

    /// Value size in bytes for SET commands.
    #[arg(short = 's', long)]
    data_size: Option<usize>,

    /// Enable Valkey/Redis Cluster mode.
    #[arg(long)]
    cluster: bool,

    /// Enable TLS encryption.
    #[arg(long)]
    tls: bool,

    /// Output format: clean, json, verbose, quiet.
    #[arg(short = 'o', long)]
    output: Option<String>,

    // -- Tier 2 flags -------------------------------------------------------
    /// Warmup period before recording results (e.g. 10s, 1m).
    #[arg(long, value_parser = parse_duration)]
    warmup: Option<Duration>,

    /// Key size in bytes.
    #[arg(long)]
    key_size: Option<usize>,

    /// Key distribution: uniform or zipf.
    #[arg(long)]
    distribution: Option<DistributionArg>,

    /// Rate limit in requests/second (0 = unlimited).
    #[arg(long)]
    rate_limit: Option<u64>,

    /// Prefill cache with all keys before benchmarking.
    #[arg(long)]
    prefill: bool,

    /// On GET miss, SET the key to backfill the cache.
    #[arg(long)]
    backfill: bool,

    /// Path to write Parquet output file.
    #[arg(long)]
    parquet: Option<PathBuf>,

    /// Pin worker threads to CPUs (Linux style: "0-3,8-11").
    #[arg(long)]
    cpu_list: Option<String>,

    /// Use RESP3 protocol instead of RESP2.
    #[arg(long)]
    resp3: bool,

    /// Skip TLS certificate verification.
    #[arg(long)]
    tls_skip_verify: bool,

    /// TLS SNI hostname (defaults to --host value).
    #[arg(long)]
    tls_hostname: Option<String>,

    /// Connection timeout (e.g. 5s, 500ms).
    #[arg(long, value_parser = parse_duration)]
    connect_timeout: Option<Duration>,

    /// Request timeout (e.g. 1s, 500ms).
    #[arg(long, value_parser = parse_duration)]
    request_timeout: Option<Duration>,

    /// Color mode: auto, always, never.
    #[arg(long)]
    color: Option<String>,

    /// Load base config from a TOML file. CLI flags override file values.
    #[arg(long)]
    config: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Saturate subcommand flags
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
struct SaturateArgs {
    #[command(flatten)]
    bench: BenchArgs,

    /// p50 latency SLO (e.g. "500us", "1ms").
    #[arg(long)]
    slo_p50: Option<String>,

    /// p99 latency SLO (e.g. "1ms", "5ms").
    #[arg(long)]
    slo_p99: Option<String>,

    /// p99.9 latency SLO (e.g. "1ms", "10ms").
    #[arg(long)]
    slo_p999: Option<String>,

    /// Starting request rate (requests/second).
    #[arg(long, default_value_t = 1000)]
    start_rate: u64,

    /// Rate step multiplier (e.g. 1.05 = 5% increase each step).
    #[arg(long, default_value_t = 1.05)]
    step: f64,

    /// Sample window duration in seconds at each rate level.
    #[arg(long, default_value_t = 5)]
    sample_window: u64,

    /// Maximum rate to try (absolute ceiling).
    #[arg(long, default_value_t = 100_000_000)]
    max_rate: u64,
}

// ---------------------------------------------------------------------------
// Enums for clap ValueEnum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DistributionArg {
    Uniform,
    Zipf,
}

impl From<DistributionArg> for Distribution {
    fn from(d: DistributionArg) -> Self {
        match d {
            DistributionArg::Uniform => Distribution::Uniform,
            DistributionArg::Zipf => Distribution::Zipf,
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::View(args)) => {
            viewer::run(args.into());
            Ok(())
        }
        Some(Command::Saturate(args)) => {
            init_tracing();
            let config = build_config_from_saturate(&args)?;
            launch_benchmark(config)
        }
        None => {
            init_tracing();
            let config = build_config(&cli.bench)?;
            launch_benchmark(config)
        }
    }
}

/// Set up the output formatter, signal handler, CPU pinning, and launch the
/// shared benchmark engine.
fn launch_benchmark(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    // Parse CPU list if configured
    let cpu_ids = if let Some(ref cpu_list) = config.general.cpu_list {
        match parse_cpu_list(cpu_list) {
            Ok(ids) => Some(ids),
            Err(e) => {
                tracing::error!("invalid cpu_list '{}': {}", cpu_list, e);
                return Err(e.into());
            }
        }
    } else {
        None
    };

    // Create the output formatter
    let formatter = create_formatter_with_banner(
        config.admin.format,
        config.admin.color,
        "valkey-lab, powered by cachecannon".to_string(),
    );

    // Print config using the formatter
    formatter.print_config(&config);

    // Set up signal handler for graceful shutdown
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting signal handler");

    run_benchmark_full(config, cpu_ids, formatter, running)
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();
}

// ---------------------------------------------------------------------------
// Config construction
// ---------------------------------------------------------------------------

/// Build a [`Config`] from the bench CLI flags, optionally layering on top of
/// a TOML base config loaded via `--config`.
fn build_config(args: &BenchArgs) -> Result<Config, Box<dyn std::error::Error>> {
    let mut config = load_base_config(args)?;
    apply_bench_flags(&mut config, args)?;
    Ok(config)
}

/// Build a [`Config`] for the `saturate` subcommand. This layers the bench
/// flags *and* the saturation-specific flags on top of an optional TOML base.
fn build_config_from_saturate(args: &SaturateArgs) -> Result<Config, Box<dyn std::error::Error>> {
    let mut config = build_config(&args.bench)?;

    // Parse SLO durations
    let slo_p50 = args.slo_p50.as_deref().map(parse_duration).transpose()?;
    let slo_p99 = args.slo_p99.as_deref().map(parse_duration).transpose()?;
    let slo_p999 = args.slo_p999.as_deref().map(parse_duration).transpose()?;

    if slo_p50.is_none() && slo_p99.is_none() && slo_p999.is_none() {
        return Err(
            "saturate requires at least one SLO threshold (--slo-p50, --slo-p99, or --slo-p999)"
                .into(),
        );
    }

    config.workload.saturation_search = Some(SaturationSearch {
        slo: SloThresholds {
            p50: slo_p50,
            p99: slo_p99,
            p999: slo_p999,
        },
        start_rate: args.start_rate,
        step_multiplier: args.step,
        sample_window: Duration::from_secs(args.sample_window),
        stop_after_failures: 3,
        max_rate: args.max_rate,
        min_throughput_ratio: 0.9,
    });

    Ok(config)
}

/// If `--config` was provided, load it as the base; otherwise start from defaults.
fn load_base_config(args: &BenchArgs) -> Result<Config, Box<dyn std::error::Error>> {
    if let Some(ref path) = args.config {
        Ok(Config::load(path)?)
    } else {
        // No config file — build defaults matching CLI help text.
        let host = args.host.as_deref().unwrap_or("127.0.0.1");
        let port = args.port.unwrap_or(6379);
        let addr = resolve_host(host, port)?;

        Ok(Config {
            general: General {
                duration: args.duration.unwrap_or(Duration::from_secs(60)),
                warmup: args.warmup.unwrap_or(Duration::from_secs(10)),
                ..General::default()
            },
            target: Target {
                endpoints: vec![addr],
                protocol: Protocol::default(),
                tls: false,
                tls_hostname: None,
                tls_verify: true,
                cluster: false,
            },
            connection: Connection {
                connections: args.connections.unwrap_or(1),
                pipeline_depth: args.pipeline.unwrap_or(1),
                connect_timeout: args.connect_timeout.unwrap_or(Duration::from_secs(5)),
                request_timeout: args.request_timeout.unwrap_or(Duration::from_secs(1)),
                ..Connection::default()
            },
            workload: Workload::default(),
            timestamps: Default::default(),
            admin: Default::default(),
            momento: Default::default(),
        })
    }
}

/// Apply explicitly-provided CLI flags onto the config, preserving TOML
/// values for anything the user didn't specify on the command line.
fn apply_bench_flags(
    config: &mut Config,
    args: &BenchArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    // Target — only override endpoint when host or port is explicitly given
    if args.host.is_some() || args.port.is_some() {
        let host = args.host.as_deref().unwrap_or("127.0.0.1");
        let port = args.port.unwrap_or(6379);
        let addr = resolve_host(host, port)?;
        config.target.endpoints = vec![addr];
    }

    if args.resp3 {
        config.target.protocol = Protocol::Resp3;
    }
    if args.tls {
        config.target.tls = true;
    }
    if args.tls_skip_verify {
        config.target.tls_verify = false;
    }
    if args.cluster {
        config.target.cluster = true;
    }
    if let Some(ref hostname) = args.tls_hostname {
        config.target.tls_hostname = Some(hostname.clone());
    }

    // General
    if let Some(d) = args.duration {
        config.general.duration = d;
    }
    if let Some(w) = args.warmup {
        config.general.warmup = w;
    }
    if let Some(t) = args.threads {
        config.general.threads = t;
    }
    if let Some(ref cpu_list) = args.cpu_list {
        config.general.cpu_list = Some(cpu_list.clone());
    }

    // Connection
    if let Some(c) = args.connections {
        config.connection.connections = c;
    }
    if let Some(p) = args.pipeline {
        config.connection.pipeline_depth = p;
    }
    if let Some(t) = args.connect_timeout {
        config.connection.connect_timeout = t;
    }
    if let Some(t) = args.request_timeout {
        config.connection.request_timeout = t;
    }

    // Workload
    if let Some(ref ratio) = args.ratio {
        let (get, set) = parse_ratio(ratio)?;
        config.workload.commands = Commands {
            get,
            set,
            delete: 0,
        };
    }
    if let Some(key_size) = args.key_size {
        config.workload.keyspace.length = key_size;
    }
    if let Some(keyspace) = args.keyspace {
        config.workload.keyspace.count = keyspace;
    }
    if let Some(dist) = args.distribution {
        config.workload.keyspace.distribution = dist.into();
    }
    if let Some(data_size) = args.data_size {
        config.workload.values = Values { length: data_size };
    }
    if args.prefill {
        config.workload.prefill = true;
    }
    if args.backfill {
        config.workload.backfill_on_miss = true;
    }
    if let Some(rate) = args.rate_limit {
        config.workload.rate_limit = Some(rate);
    }

    // Admin / output
    if let Some(ref output) = args.output {
        config.admin.format = output.parse().map_err(|e: String| e)?;
    }
    if let Some(ref color) = args.color {
        config.admin.color = color.parse().map_err(|e: String| e)?;
    }
    if let Some(ref parquet) = args.parquet {
        config.admin.parquet = Some(parquet.clone());
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a ratio string like "80:20" into (get, set) u8 values.
fn parse_ratio(s: &str) -> Result<(u8, u8), Box<dyn std::error::Error>> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        return Err(format!("invalid ratio '{}', expected format like '80:20'", s).into());
    }
    let get: u8 = parts[0]
        .trim()
        .parse()
        .map_err(|_| format!("invalid GET ratio '{}'", parts[0]))?;
    let set: u8 = parts[1]
        .trim()
        .parse()
        .map_err(|_| format!("invalid SET ratio '{}'", parts[1]))?;
    if get == 0 && set == 0 {
        return Err("ratio cannot be 0:0".into());
    }
    Ok((get, set))
}

/// Parse a human-readable duration string (e.g. "500us", "1ms", "5s").
fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration string".into());
    }

    let split_pos = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, suffix) = s.split_at(split_pos);

    let value: u64 = num
        .parse()
        .map_err(|e| format!("invalid number in duration: {e}"))?;

    match suffix.trim() {
        "ns" => Ok(Duration::from_nanos(value)),
        "us" => Ok(Duration::from_micros(value)),
        "ms" => Ok(Duration::from_millis(value)),
        "s" | "sec" | "secs" | "" => Ok(Duration::from_secs(value)),
        "m" | "min" | "mins" => Ok(Duration::from_secs(value * 60)),
        "h" | "hr" | "hrs" => Ok(Duration::from_secs(value * 3600)),
        other => Err(format!("unknown time unit '{}' in duration", other)),
    }
}

/// Resolve a hostname (or IP literal) and port to a `SocketAddr`.
fn resolve_host(host: &str, port: u16) -> Result<SocketAddr, Box<dyn std::error::Error>> {
    (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| format!("could not resolve host '{host}'").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ratio_valid() {
        assert_eq!(parse_ratio("80:20").unwrap(), (80, 20));
        assert_eq!(parse_ratio("50:50").unwrap(), (50, 50));
        assert_eq!(parse_ratio("100:0").unwrap(), (100, 0));
        assert_eq!(parse_ratio("0:100").unwrap(), (0, 100));
    }

    #[test]
    fn test_parse_ratio_invalid() {
        assert!(parse_ratio("80").is_err());
        assert!(parse_ratio("80:20:0").is_err());
        assert!(parse_ratio("abc:20").is_err());
        assert!(parse_ratio("0:0").is_err());
    }

    #[test]
    fn test_parse_duration_valid() {
        assert_eq!(parse_duration("500us").unwrap(), Duration::from_micros(500));
        assert_eq!(parse_duration("1ms").unwrap(), Duration::from_millis(1));
        assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_duration("10").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_duration("100ns").unwrap(), Duration::from_nanos(100));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("2hr").unwrap(), Duration::from_secs(7200));
    }

    #[test]
    fn test_parse_duration_invalid() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
    }
}
