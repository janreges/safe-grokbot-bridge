//! Command-line entry point of safe-grokbot-bridge.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use safe_grokbot_bridge::config::{self, Config};
use safe_grokbot_bridge::{api, auth, hook, server};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct ConfigArg {
    /// Config file [default: the platform config dir, e.g. ~/.config/safe-grokbot-bridge/config.toml]
    #[arg(long, env = "SGB_CONFIG")]
    config: Option<PathBuf>,
}

impl ConfigArg {
    fn path(&self) -> PathBuf {
        self.config
            .clone()
            .unwrap_or_else(config::default_config_path)
    }
}

#[derive(Subcommand)]
enum Command {
    /// Run the bridge.
    Serve(ConfigArg),
    /// Generate a new access token and print the config snippet that authorizes it.
    Token {
        /// Name recorded with the token; it appears in the audit log.
        #[arg(long, default_value = "grok")]
        name: String,
    },
    /// Print the OpenAPI document of the HTTP API.
    Openapi,
    /// Record a turn-complete event (called by agent hooks, not by people).
    #[command(hide = true)]
    Hook {
        agent: String,
        file: PathBuf,
        /// Event payload (Codex passes it as an argument; Claude Code on stdin).
        payload: Option<String>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Serve(args) => serve(&args.path()),
        Command::Token { name } => token(&name),
        Command::Hook {
            agent,
            file,
            payload,
        } => {
            hook::run_cli(&agent, &file, payload);
            Ok(())
        }
        Command::Openapi => {
            println!("{}", api::openapi().to_pretty_json()?);
            Ok(())
        }
    }
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
}

fn serve(path: &std::path::Path) -> Result<()> {
    init_logging();
    let config = Config::load(path)?;
    tokio::runtime::Runtime::new()?.block_on(server::run(config))
}

fn token(name: &str) -> Result<()> {
    let token = auth::generate_token();
    println!("Token (shown only once, store it in your client's secret store):\n\n  {token}\n");
    println!("Add this to the config file and restart the bridge:\n");
    println!(
        "[[tokens]]\nname = \"{name}\"\nsha256 = \"{}\"",
        auth::hash_token(&token)
    );
    Ok(())
}
