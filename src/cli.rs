use crate::config::Backend;
use crate::daemon::AgentArgs;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Top-level CLI. The serve flags are flattened at the top level so the
/// pre-v1.2 flat invocations (`herd -c herd.yaml`, `herd -p 8080 -b a=...`)
/// keep parsing unchanged; `serve` and `agent` are explicit subcommands.
#[derive(Parser)]
#[command(name = "herd")]
#[command(about = "Intelligent Ollama router with GPU awareness", long_about = None)]
pub struct Cli {
    #[command(flatten)]
    pub serve: ServeArgs,

    /// Check for updates and install if available
    #[arg(long)]
    pub update: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the gateway (default when no subcommand is given)
    Serve(ServeArgs),
    /// Run the node-side agent daemon that heartbeats a gateway
    Agent(AgentArgs),
    /// Promote a binary into the fleet publish-dir for an os/arch
    Publish(PublishArgs),
}

#[derive(clap::Args)]
pub struct PublishArgs {
    /// Binary to publish (default: the currently running herd executable)
    #[arg(value_name = "BINARY")]
    pub binary: Option<PathBuf>,

    /// Version to publish under (path component: fleet.target_agent_version)
    #[arg(long, value_name = "VERSION")]
    pub version: String,

    /// Target OS (default: host OS, e.g. linux/windows/macos)
    #[arg(long, value_name = "OS")]
    pub os: Option<String>,

    /// Target arch (default: host arch, e.g. x86_64/aarch64)
    #[arg(long, value_name = "ARCH")]
    pub arch: Option<String>,

    /// Publish directory root (overrides HERD_AGENT_PUBLISH_DIR and config)
    #[arg(long, value_name = "DIR")]
    pub publish_dir: Option<PathBuf>,

    /// Read fleet.publish_dir from this config file when --publish-dir is unset
    #[arg(short, long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Overwrite an existing binary even if its bytes (sha256) differ
    #[arg(long)]
    pub force: bool,
}

#[derive(clap::Args)]
pub struct ServeArgs {
    /// Path to config file
    #[arg(short, long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Port to listen on
    #[arg(short, long, default_value = "40114")]
    pub port: u16,

    /// Host to bind to
    #[arg(long, default_value = "0.0.0.0")]
    pub host: String,

    /// Backend URLs (format: name=url:priority)
    #[arg(short, long)]
    pub backend: Vec<String>,
}

pub fn parse_backend_spec(spec: &str) -> Option<Backend> {
    let (name, raw_target) = spec.split_once('=')?;
    let name = name.trim();
    let raw_target = raw_target.trim();
    if name.is_empty() || raw_target.is_empty() {
        return None;
    }

    let (raw_url, priority) = if has_explicit_priority(raw_target) {
        match raw_target.rsplit_once(':') {
            Some((url_part, priority_part)) => {
                let priority = priority_part.parse::<u32>().ok()?;
                (url_part.trim(), priority)
            }
            None => (raw_target, 50),
        }
    } else {
        (raw_target, 50)
    };

    if raw_url.is_empty() {
        return None;
    }

    let url = if raw_url.starts_with("http://") || raw_url.starts_with("https://") {
        raw_url.to_string()
    } else {
        format!("http://{}", raw_url)
    };

    Some(Backend {
        name: name.to_string(),
        url,
        priority,
        ..Default::default()
    })
}

fn has_explicit_priority(raw_target: &str) -> bool {
    let remainder = raw_target
        .strip_prefix("http://")
        .or_else(|| raw_target.strip_prefix("https://"))
        .unwrap_or(raw_target);

    if remainder.starts_with('[') {
        if let Some(end) = remainder.find(']') {
            let after_bracket = &remainder[end + 1..];
            return after_bracket.matches(':').count() >= 2;
        }
    }

    remainder.matches(':').count() >= 2
}

#[cfg(test)]
mod cli_tests {
    use super::{Cli, Command};
    use clap::Parser;

    #[test]
    fn legacy_flat_invocation_parses_unchanged() {
        let cli = Cli::try_parse_from(["herd", "-c", "foo.yaml"]).unwrap();
        assert!(cli.command.is_none());
        assert!(!cli.update);
        assert_eq!(
            cli.serve.config.as_deref().unwrap().to_str(),
            Some("foo.yaml")
        );
        assert_eq!(cli.serve.port, 40114);
        assert_eq!(cli.serve.host, "0.0.0.0");
        assert!(cli.serve.backend.is_empty());
    }

    #[test]
    fn legacy_port_host_backend_flags_parse_unchanged() {
        let cli = Cli::try_parse_from([
            "herd",
            "-p",
            "8080",
            "--host",
            "127.0.0.1",
            "-b",
            "citadel=http://citadel:11434:100",
        ])
        .unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.serve.port, 8080);
        assert_eq!(cli.serve.host, "127.0.0.1");
        assert_eq!(cli.serve.backend, vec!["citadel=http://citadel:11434:100"]);
    }

    #[test]
    fn legacy_update_flag_parses_unchanged() {
        let cli = Cli::try_parse_from(["herd", "--update"]).unwrap();
        assert!(cli.update);
        assert!(cli.command.is_none());
    }

    #[test]
    fn explicit_serve_subcommand_accepts_same_flags() {
        let cli = Cli::try_parse_from(["herd", "serve", "-c", "x.yaml", "-p", "9000"]).unwrap();
        match cli.command {
            Some(Command::Serve(args)) => {
                assert_eq!(args.config.as_deref().unwrap().to_str(), Some("x.yaml"));
                assert_eq!(args.port, 9000);
            }
            _ => panic!("expected serve subcommand"),
        }
    }

    #[test]
    fn agent_subcommand_parses_with_defaults() {
        let cli = Cli::try_parse_from(["herd", "agent", "--gateway", "http://gw:40114"]).unwrap();
        match cli.command {
            Some(Command::Agent(args)) => {
                assert_eq!(args.gateway, "http://gw:40114");
                assert!(args.node_id.is_none());
                assert_eq!(args.heartbeat_secs, 2);
                assert_eq!(args.backend_url, "http://127.0.0.1:11434");
                assert!(args.advertise_url.is_none());
                assert!(args.backend.is_none());
            }
            _ => panic!("expected agent subcommand"),
        }
    }

    #[test]
    fn agent_subcommand_accepts_overrides() {
        let cli = Cli::try_parse_from([
            "herd",
            "agent",
            "--gateway",
            "http://gw:40114",
            "--node-id",
            "citadel-5090",
            "--heartbeat-secs",
            "5",
            "--backend",
            "llama-server",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Agent(args)) => {
                assert_eq!(args.node_id.as_deref(), Some("citadel-5090"));
                assert_eq!(args.heartbeat_secs, 5);
                assert_eq!(args.backend, Some(crate::config::BackendType::LlamaServer));
            }
            _ => panic!("expected agent subcommand"),
        }
    }

    #[test]
    fn agent_requires_gateway() {
        assert!(Cli::try_parse_from(["herd", "agent"]).is_err());
    }

    #[test]
    fn agent_rejects_unknown_backend_type() {
        assert!(Cli::try_parse_from([
            "herd",
            "agent",
            "--gateway",
            "http://gw:40114",
            "--backend",
            "vllm"
        ])
        .is_err());
    }

    #[test]
    fn publish_subcommand_parses_with_defaults() {
        let cli = Cli::try_parse_from(["herd", "publish", "--version", "1.2.0"]).unwrap();
        match cli.command {
            Some(Command::Publish(args)) => {
                assert!(args.binary.is_none());
                assert_eq!(args.version, "1.2.0");
                assert!(args.os.is_none());
                assert!(args.arch.is_none());
                assert!(args.publish_dir.is_none());
                assert!(args.config.is_none());
                assert!(!args.force);
            }
            _ => panic!("expected publish subcommand"),
        }
    }

    #[test]
    fn publish_accepts_positional_and_overrides() {
        let cli = Cli::try_parse_from([
            "herd",
            "publish",
            "./herd-arm",
            "--version",
            "1.2.0",
            "--os",
            "linux",
            "--arch",
            "aarch64",
            "--publish-dir",
            "/srv/bin",
            "--force",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Publish(args)) => {
                use std::path::Path;
                assert_eq!(args.binary.as_deref().unwrap(), Path::new("./herd-arm"));
                assert_eq!(args.version, "1.2.0");
                assert_eq!(args.os.as_deref(), Some("linux"));
                assert_eq!(args.arch.as_deref(), Some("aarch64"));
                assert_eq!(args.publish_dir.as_deref().unwrap(), Path::new("/srv/bin"));
                assert!(args.force);
            }
            _ => panic!("expected publish subcommand"),
        }
    }

    #[test]
    fn publish_requires_version() {
        assert!(Cli::try_parse_from(["herd", "publish"]).is_err());
    }

    #[test]
    fn publish_config_short_flag() {
        let cli = Cli::try_parse_from(["herd", "publish", "--version", "1.2.0", "-c", "herd.yaml"])
            .unwrap();
        match cli.command {
            Some(Command::Publish(args)) => {
                use std::path::Path;
                assert_eq!(args.config.as_deref().unwrap(), Path::new("herd.yaml"));
            }
            _ => panic!("expected publish subcommand"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_backend_spec;

    #[test]
    fn parses_documented_format() {
        let backend = parse_backend_spec("citadel=http://citadel:11434:100").unwrap();
        assert_eq!(backend.name, "citadel");
        assert_eq!(backend.url, "http://citadel:11434");
        assert_eq!(backend.priority, 100);
    }

    #[test]
    fn defaults_priority_when_omitted() {
        let backend = parse_backend_spec("edge=http://edge:11434").unwrap();
        assert_eq!(backend.url, "http://edge:11434");
        assert_eq!(backend.priority, 50);
    }

    #[test]
    fn accepts_https_and_bracketed_ipv6() {
        let backend = parse_backend_spec("gpu=https://[::1]:11434:70").unwrap();
        assert_eq!(backend.url, "https://[::1]:11434");
        assert_eq!(backend.priority, 70);
    }
}
