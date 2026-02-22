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

static ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/src/viewer/assets");

mod dashboard;
mod plot;

use metriken_query::{QueryEngine, Tsdb};

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

    tracing_subscriber::fmt().with_max_level(level).init();

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
    let benchmark_data = Tsdb::load(&config.input)
        .map_err(|e| {
            eprintln!("failed to load benchmark data from parquet: {e}");
            std::process::exit(1);
        })
        .unwrap();

    // Load optional server rezolus data
    let server_data = if let Some(ref path) = config.server {
        info!("Loading server rezolus data from parquet file...");
        match Tsdb::load(path) {
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
        match Tsdb::load(path) {
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

        if open::that(format!("http://{addr}")).is_err() {
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

    listener.set_nonblocking(true).unwrap();
    let listener = TcpListener::from_std(listener).unwrap();

    axum::serve(listener, app)
        .await
        .expect("failed to run http server");
}

pub struct AppState {
    pub sections: HashMap<String, String>,
    query_engine: Arc<QueryEngine>,
}

impl AppState {
    pub fn new(tsdb: Arc<Tsdb>) -> Self {
        Self {
            sections: Default::default(),
            query_engine: Arc::new(QueryEngine::new(tsdb)),
        }
    }
}

fn app(livereload: LiveReloadLayer, state: AppState) -> Router {
    let state = Arc::new(state);

    let router = Router::new()
        .route("/about", get(about))
        .route("/data/{path}", get(data))
        .with_state(state.clone())
        .merge(metriken_query::promql::routes(state.query_engine.clone()));

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
) -> (StatusCode, String) {
    (
        StatusCode::OK,
        state
            .sections
            .get(&path)
            .map(|v| v.to_string())
            .unwrap_or("{ }".to_string()),
    )
}

async fn index() -> (StatusCode, [(header::HeaderName, &'static str); 1], String) {
    if let Some(asset) = ASSETS.get_file("index.html") {
        let body = asset.contents_utf8().unwrap();
        let content_type = "text/html";

        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, content_type)],
            body.to_string(),
        )
    } else {
        error!("index.html missing from build");
        (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain")],
            "404 Not Found".to_string(),
        )
    }
}

async fn lib(
    axum::extract::Path(path): axum::extract::Path<String>,
) -> (StatusCode, [(header::HeaderName, &'static str); 1], String) {
    if let Some(asset) = ASSETS.get_file(format!("lib/{path}")) {
        let body = asset.contents_utf8().unwrap();
        let content_type = if path.ends_with(".js") {
            "text/javascript"
        } else if path.ends_with(".css") {
            "text/css"
        } else {
            "text/plain"
        };

        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, content_type)],
            body.to_string(),
        )
    } else {
        error!("path: {path} does not map to a static resource");
        (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain")],
            "404 Not Found".to_string(),
        )
    }
}
