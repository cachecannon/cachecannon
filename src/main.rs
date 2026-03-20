use cachecannon::config::Config;
use cachecannon::viewer;
use cachecannon::{create_formatter, parse_cpu_list, run_benchmark_full};

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[derive(Parser, Debug)]
#[command(name = "cachecannon")]
#[command(about = "High-performance cache protocol benchmark tool")]
#[command(version)]
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to configuration file
    #[arg(value_name = "CONFIG")]
    config: Option<PathBuf>,

    /// Path to write Parquet output file
    #[arg(long)]
    parquet: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// View benchmark results in a web dashboard
    View(viewer::ViewArgs),
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::View(args)) => {
            // Run the viewer (viewer has its own tracing subscriber)
            viewer::run(args.into());
            Ok(())
        }
        None => {
            // Run cachecannon if config was provided
            if let Some(ref config) = cli.config {
                init_tracing();
                run_cachecannon_cli(config, &cli)
            } else {
                // Show help
                Cli::parse_from(["cachecannon", "--help"]);
                Ok(())
            }
        }
    }
}

fn init_tracing() {
    let (non_blocking, _guard) = tracing_appender::non_blocking(std::io::stderr());
    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();
    // Leak the guard so the non-blocking writer lives for the process lifetime
    std::mem::forget(_guard);
}

fn run_cachecannon_cli(config_path: &PathBuf, cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    // Load configuration
    let mut config = Config::load(config_path)?;

    // Apply CLI overrides
    if let Some(ref parquet) = cli.parquet {
        config.admin.parquet = Some(parquet.clone());
    }

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
    let formatter = create_formatter(config.admin.format, config.admin.color);

    // Print config using the formatter
    formatter.print_config(&config);

    // Set up signal handler for graceful shutdown
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, std::sync::atomic::Ordering::SeqCst);
    })
    .expect("Error setting signal handler");

    run_benchmark_full(config, cpu_ids, formatter, running)
}
