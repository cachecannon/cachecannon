use cachecannon::viewer;

use clap::{Parser, Subcommand};
#[cfg(target_os = "linux")]
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "cachecannon")]
#[command(about = "High-performance cache protocol benchmark tool")]
#[command(version)]
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to configuration file
    #[cfg(target_os = "linux")]
    #[arg(value_name = "CONFIG")]
    config: Option<PathBuf>,

    /// Path to write Parquet output file
    #[cfg(target_os = "linux")]
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
            viewer::run(args.into());
            Ok(())
        }
        None => {
            #[cfg(target_os = "linux")]
            {
                run_benchmark_cli(&cli)
            }
            #[cfg(not(target_os = "linux"))]
            {
                Cli::parse_from(["cachecannon", "--help"]);
                Ok(())
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn run_benchmark_cli(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    use cachecannon::config::Config;
    use cachecannon::{create_formatter, parse_cpu_list, run_benchmark_full};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    if let Some(ref config) = cli.config {
        let (non_blocking, _guard) = tracing_appender::non_blocking(std::io::stderr());
        tracing_subscriber::fmt()
            .with_writer(non_blocking)
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive(tracing::Level::INFO.into()),
            )
            .init();
        std::mem::forget(_guard);

        let mut config = Config::load(config)?;

        if let Some(ref parquet) = cli.parquet {
            config.admin.parquet = Some(parquet.clone());
        }

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

        let formatter = create_formatter(config.admin.format, config.admin.color);
        formatter.print_config(&config);

        let running = Arc::new(AtomicBool::new(true));
        let r = running.clone();
        ctrlc::set_handler(move || {
            r.store(false, std::sync::atomic::Ordering::SeqCst);
        })
        .expect("Error setting signal handler");

        run_benchmark_full(config, cpu_ids, formatter, running)
    } else {
        Cli::parse_from(["cachecannon", "--help"]);
        Ok(())
    }
}
