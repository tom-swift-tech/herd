use clap::Parser;
use herd::cli::{Cli, Command, ServeArgs};
use herd::config::Config;
use herd::server;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

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

    match cli.command {
        Some(Command::Agent(args)) => herd::daemon::run(args).await,
        Some(Command::Serve(args)) => serve(args).await,
        Some(Command::Publish(args)) => herd::publish::run(args),
        None => serve(cli.serve).await,
    }
}

async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let (mut config, config_path) = if let Some(config_path) = args.config {
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
        config.server.host = args.host;
        config.server.port = args.port;

        for spec in args.backend {
            match herd::cli::parse_backend_spec(&spec) {
                Some(backend) => config.backends.push(backend),
                None => tracing::warn!("Ignoring invalid backend spec: {}", spec),
            }
        }

        (config, None)
    };

    config.validate()?;

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
