use clap::Parser;
use herd::config::Config;
use herd::server;
use std::path::PathBuf;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
#[command(name = "herd")]
#[command(about = "Intelligent Ollama router with GPU awareness", long_about = None)]
struct Cli {
    /// Path to config file
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Port to listen on
    #[arg(short, long, default_value = "40114")]
    port: u16,

    /// Host to bind to
    #[arg(long, default_value = "0.0.0.0")]
    host: String,

    /// Backend URLs (format: name=url:priority)
    #[arg(short, long)]
    backend: Vec<String>,

    /// Check for updates and install if available
    #[arg(long)]
    update: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "herd=info,tower_http=debug".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();

    if cli.update {
        println!("Checking for updates...");
        match herd::updater::check_for_update() {
            Ok(info) if info.update_available => {
                println!("Update available: v{} → v{}", info.current, info.latest);
                println!("Downloading and installing...");
                match herd::updater::perform_update(true) {
                    Ok(version) => {
                        println!("Updated to v{}. Please restart herd.", version);
                        return Ok(());
                    }
                    Err(e) => {
                        eprintln!("Update failed: {}", e);
                        std::process::exit(1);
                    }
                }
            }
            Ok(info) => {
                println!("Already up to date (v{})", info.current);
                return Ok(());
            }
            Err(e) => {
                eprintln!("Update check failed: {}", e);
                std::process::exit(1);
            }
        }
    }

    let (config, config_path) = if let Some(config_path) = cli.config {
        let config = match Config::from_file(&config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to load config from {:?}: {}", config_path, e);
                tracing::info!("Starting with default configuration");
                Config::default()
            }
        };
        (config, Some(config_path))
    } else {
        let mut config = Config::default();
        config.server.host = cli.host;
        config.server.port = cli.port;

        for spec in cli.backend {
            match herd::cli::parse_backend_spec(&spec) {
                Some(backend) => config.backends.push(backend),
                None => tracing::warn!("Ignoring invalid backend spec: {}", spec),
            }
        }

        (config, None)
    };

    tracing::info!(
        "Starting Herd on {}:{} with {} backends",
        config.server.host,
        config.server.port,
        config.backends.len()
    );

    // Background update check (non-blocking, best-effort)
    tokio::spawn(herd::updater::startup_update_check());

    server::run(config, config_path).await
}
