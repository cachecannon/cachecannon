use axum::Router;
use axum::routing::get;
use clap::Parser;
use http::StatusCode;
use http::header;
use include_dir::{Dir, include_dir};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower_http::compression::CompressionLayer;
use tower_http::decompression::RequestDecompressionLayer;
use tower_livereload::LiveReloadLayer;
use tracing::{error, info};

static ASSETS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/src/viewer/assets");

mod api;
mod dashboard;
mod plot;

use metriken_query::ParquetReader;

#[derive(Parser, Debug)]
#[command(name = "view")]
#[command(about = "View benchmark results in a web dashboard")]
pub struct ViewArgs {
    /// Path to benchmark parquet file
    pub input: PathBuf,

    /// Path to server-side rezolus parquet file
    #[arg(long)]
    pub server: Option<PathBuf>,

    /// Path to client-side rezolus parquet file
    #[arg(long)]
    pub client: Option<PathBuf>,

    /// Listen address for web server
    #[arg(long, default_value = "127.0.0.1:0")]
    pub listen: SocketAddr,

    /// Increase verbosity
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

pub struct Config {
    pub input: PathBuf,
    pub server: Option<PathBuf>,
    pub client: Option<PathBuf>,
    pub listen: SocketAddr,
    pub verbose: u8,
}

impl From<ViewArgs> for Config {
    fn from(args: ViewArgs) -> Self {
        Config {
            input: args.input,
            server: args.server,
            client: args.client,
            listen: args.listen,
            verbose: args.verbose,
        }
    }
}

/// Run the viewer
pub fn run(config: Config) {
    let config: Arc<Config> = config.into();

    // Initialize tracing with appropriate level
    let level = match config.verbose {
        0 => tracing::Level::INFO,
        1 => tracing::Level::DEBUG,
        _ => tracing::Level::TRACE,
    };

    let (non_blocking, _guard) = tracing_appender::non_blocking(std::io::stderr());
    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_max_level(level)
        .init();
    // Leak the guard so the non-blocking writer lives for the process lifetime
    std::mem::forget(_guard);

    // initialize async runtime
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .thread_name("cachecannon-viewer")
        .build()
        .expect("failed to launch async runtime");

    ctrlc::set_handler(move || {
        std::process::exit(2);
    })
    .expect("failed to set ctrl-c handler");

    info!("Loading benchmark data from parquet file...");
    let benchmark_data = ParquetReader::open(&config.input)
        .map_err(|e| {
            eprintln!("failed to load benchmark data from parquet: {e}");
            std::process::exit(1);
        })
        .unwrap();

    // Load optional server rezolus data
    let server_data = if let Some(ref path) = config.server {
        info!("Loading server rezolus data from parquet file...");
        match ParquetReader::open(path) {
            Ok(data) => Some(Arc::new(data)),
            Err(e) => {
                eprintln!("warning: failed to load server rezolus data: {e}");
                None
            }
        }
    } else {
        None
    };

    // Load optional client rezolus data
    let client_data = if let Some(ref path) = config.client {
        info!("Loading client rezolus data from parquet file...");
        match ParquetReader::open(path) {
            Ok(data) => Some(Arc::new(data)),
            Err(e) => {
                eprintln!("warning: failed to load client rezolus data: {e}");
                None
            }
        }
    } else {
        None
    };

    info!("Generating dashboards...");
    let benchmark_arc = Arc::new(benchmark_data);
    let state = dashboard::generate(benchmark_arc, server_data, client_data);

    // open the tcp listener
    let listener = std::net::TcpListener::bind(config.listen).expect("failed to listen");
    let addr = listener.local_addr().expect("socket missing local addr");

    // open in browser
    rt.spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;

        if std::env::var_os("CACHECANNON_NO_OPEN").is_some() {
            info!("Use your browser to view: http://{addr}");
        } else if open::that(format!("http://{addr}")).is_err() {
            info!("Use your browser to view: http://{addr}");
        } else {
            info!("Launched browser to view: http://{addr}");
        }
    });

    // launch the HTTP listener
    rt.block_on(async move { serve(listener, state).await });

    std::thread::sleep(Duration::from_millis(200));
}

async fn serve(listener: std::net::TcpListener, state: AppState) {
    let livereload = LiveReloadLayer::new();

    let app = app(livereload, state);

    listener
        .set_nonblocking(true)
        .expect("failed to set listener to non-blocking mode");
    let listener =
        TcpListener::from_std(listener).expect("failed to convert listener to tokio TcpListener");

    axum::serve(listener, app)
        .await
        .expect("failed to run http server");
}

pub struct AppState {
    pub sections: HashMap<String, String>,
    /// Each recording and the root its API is mounted at.
    sources: Vec<(&'static str, Arc<ParquetReader>)>,
}

impl AppState {
    pub fn new(sources: Vec<(&'static str, Arc<ParquetReader>)>) -> Self {
        Self {
            sections: Default::default(),
            sources,
        }
    }
}

fn app(livereload: LiveReloadLayer, state: AppState) -> Router {
    let state = Arc::new(state);

    let router = state.sources.iter().fold(
        Router::new()
            .route("/about", get(about))
            .route("/data/{path}", get(data))
            .with_state(state.clone()),
        |router, (root, source)| router.nest(root, api::routes(source.clone())),
    );

    let router = router
        .route("/", get(index))
        .route("/lib/{*path}", get(lib))
        .fallback(get(index));

    router.layer(
        ServiceBuilder::new()
            .layer(RequestDecompressionLayer::new())
            .layer(CompressionLayer::new())
            .layer(livereload),
    )
}

// Basic /about page handler
async fn about() -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        "Cachecannon Viewer {version}\nFor information, see: https://github.com/cachecannon/cachecannon\n"
    )
}

async fn data(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> (StatusCode, [(header::HeaderName, &'static str); 1], String) {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        state
            .sections
            .get(&path)
            .map(|v| v.to_string())
            .unwrap_or("{ }".to_string()),
    )
}

async fn index() -> (StatusCode, [(header::HeaderName, &'static str); 1], String) {
    if let Some(asset) = ASSETS.get_file("index.html") {
        if let Some(body) = asset.contents_utf8() {
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html")],
                body.to_string(),
            );
        }
        error!("index.html is not valid UTF-8");
    } else {
        error!("index.html missing from build");
    }
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/plain")],
        "404 Not Found".to_string(),
    )
}

/// The file behind `/lib/{path}`: the viewer's own assets, or the
/// metriken-charts assets, which hold `charts/` and `charts.css`.
fn lib_asset(path: &str) -> Option<&'static include_dir::File<'static>> {
    ASSETS
        .get_file(format!("lib/{path}"))
        .or_else(|| metriken_charts::ASSETS.get_file(path))
}

async fn lib(
    axum::extract::Path(path): axum::extract::Path<String>,
) -> (StatusCode, [(header::HeaderName, &'static str); 1], String) {
    if let Some(asset) = lib_asset(&path) {
        if let Some(body) = asset.contents_utf8() {
            let content_type = if path.ends_with(".js") {
                "text/javascript"
            } else if path.ends_with(".css") {
                "text/css"
            } else {
                "text/plain"
            };

            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, content_type)],
                body.to_string(),
            );
        }
        error!("asset lib/{path} is not valid UTF-8");
    } else {
        error!("path: {path} does not map to a static resource");
    }
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/plain")],
        "404 Not Found".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::{LiveReloadLayer, ParquetReader, app, dashboard, lib_asset};
    use arrow::array::{ArrayRef, Int64Array, UInt64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use axum::Router;
    use axum::body::Body;
    use http::Request;
    use parquet::arrow::ArrowWriter;
    use parquet::basic::Compression;
    use parquet::file::metadata::KeyValue;
    use parquet::file::properties::WriterProperties;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tower::ServiceExt;

    const NS_PER_S: u64 = 1_000_000_000;
    /// Snapshots per recording in `recording`.
    const SNAPSHOTS: u64 = 10;

    /// A recording of a host with two cores, one snapshot a second from
    /// `start` seconds, using 250 ms of user time and 750 ms of system time
    /// per second across them.
    fn recording(start: u64) -> Arc<ParquetReader> {
        let column = |name: &str, kind: &str, labels: &[(&str, &str)], ty| {
            let mut metadata = HashMap::from([
                ("metric".to_string(), name.to_string()),
                ("metric_type".to_string(), kind.to_string()),
            ]);
            for (k, v) in labels {
                metadata.insert(k.to_string(), v.to_string());
            }
            let column = labels
                .iter()
                .fold(name.to_string(), |c, (_, v)| c + "/" + v);
            Field::new(column, ty, true).with_metadata(metadata)
        };
        let schema = Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::UInt64, false),
            column(
                "cpu_usage",
                "counter",
                &[("state", "user")],
                DataType::UInt64,
            ),
            column(
                "cpu_usage",
                "counter",
                &[("state", "system")],
                DataType::UInt64,
            ),
            column("cpu_cores", "gauge", &[], DataType::Int64),
        ]));
        let seconds = 0..SNAPSHOTS;
        let columns: Vec<ArrayRef> = vec![
            Arc::new(UInt64Array::from_iter_values(
                seconds.clone().map(|i| (start + i) * NS_PER_S),
            )),
            Arc::new(UInt64Array::from_iter_values(
                seconds.clone().map(|i| i * NS_PER_S / 4),
            )),
            Arc::new(UInt64Array::from_iter_values(
                seconds.clone().map(|i| i * NS_PER_S / 4 * 3),
            )),
            Arc::new(Int64Array::from_iter_values(seconds.map(|_| 2))),
        ];

        let props = WriterProperties::builder()
            .set_compression(Compression::UNCOMPRESSED)
            .set_key_value_metadata(Some(vec![KeyValue {
                key: "sampling_interval_ms".to_string(),
                value: Some("1000".to_string()),
            }]))
            .build();
        let mut bytes = Vec::new();
        let mut writer = ArrowWriter::try_new(&mut bytes, schema.clone(), Some(props)).unwrap();
        writer
            .write(&RecordBatch::try_new(schema, columns).unwrap())
            .unwrap();
        writer.close().unwrap();
        Arc::new(ParquetReader::open_bytes(bytes).unwrap())
    }

    /// The viewer for three recordings starting at different times, with the
    /// JSON of each section by name.
    fn viewer() -> (Router, HashMap<String, serde_json::Value>) {
        let mut state = dashboard::generate(
            recording(1_000),
            Some(recording(2_000)),
            Some(recording(3_000)),
        );
        let sections = std::mem::take(&mut state.sections)
            .into_iter()
            .map(|(name, json)| (name, serde_json::from_str(&json).unwrap()))
            .collect();
        (app(LiveReloadLayer::new(), state), sections)
    }

    async fn get(router: &Router, uri: &str) -> serde_json::Value {
        let request = Request::get(uri).body(Body::empty()).unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap_or_else(|_| panic!("{uri} did not return JSON"))
    }

    /// Each recording has its own metrics and time range, so a section's `api`
    /// must be the root of the recording its queries read.
    #[tokio::test]
    async fn each_section_names_the_api_of_its_recording() {
        let (router, sections) = viewer();
        for (section, start) in [
            ("overview.json", 1_000.0),
            ("throughput.json", 1_000.0),
            ("latency.json", 1_000.0),
            ("cache.json", 1_000.0),
            ("connections.json", 1_000.0),
            ("server.json", 2_000.0),
            ("client.json", 3_000.0),
        ] {
            let api = sections[section]["api"].as_str().unwrap();
            let metadata = get(&router, &format!("{api}/metadata")).await;
            assert_eq!(metadata["data"]["minTime"], start, "{section}");
        }
    }

    /// `Unit::Percentage` multiplies by 100 when drawing, so a host at half
    /// of its cores must read 0.5. `recording` uses 1 s of CPU time per
    /// second across two cores.
    #[tokio::test]
    async fn cpu_usage_is_a_fraction_of_the_hosts_cores() {
        let (router, sections) = viewer();
        for (section, start) in [("server.json", 2_000), ("client.json", 3_000)] {
            let view = &sections[section];
            let api = view["api"].as_str().unwrap();
            let plot = &view["groups"][0]["plots"][0];
            assert_eq!(plot["opts"]["id"], "cpu-usage");
            let query = plot["promql_query"]
                .as_str()
                .unwrap()
                .bytes()
                .map(|b| format!("%{b:02X}"))
                .collect::<String>();
            let end = start + SNAPSHOTS - 1;
            let uri = format!("{api}/query_range?query={query}&start={start}&end={end}&step=1");
            let body = get(&router, &uri).await;
            let values = body["data"]["result"][0]["values"].as_array();
            let values = values.unwrap_or_else(|| panic!("{section}: {body}"));
            assert!(!values.is_empty(), "{section}: {body}");
            for point in values {
                let value = point[1].as_f64().unwrap();
                assert!((value - 0.5).abs() < 1e-9, "{section}: {value}");
            }
        }
    }

    #[test]
    fn lib_serves_the_files_the_page_loads_from_metriken_charts() {
        for path in [
            "charts/chart.js",
            "charts/host.js",
            "charts/echarts.min.js",
            "charts.css",
        ] {
            assert!(lib_asset(path).is_some(), "{path} is not served");
        }
    }

    #[test]
    fn lib_serves_the_viewers_own_assets() {
        assert!(lib_asset("script.js").is_some());
    }

    #[test]
    fn lib_returns_none_for_an_unknown_path() {
        assert!(lib_asset("charts/missing.js").is_none());
    }
}
