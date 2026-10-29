//! safe-grokbot-bridge: a secure HTTP bridge between a remote AI agent and the coding agents
//! running on this machine. See `docs/design.md` for the architecture.

mod auth;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a new access token and print the config snippet that authorizes it.
    Token {
        /// Name recorded with the token; it appears in the audit log.
        #[arg(long, default_value = "grok")]
        name: String,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Token { name } => cmd_token(&name),
    }
}

fn cmd_token(name: &str) -> anyhow::Result<()> {
    let token = auth::generate_token();
    println!("Token (shown only once, store it in your client's secret store):\n\n  {token}\n");
    println!("Add this to the config file and restart the bridge:\n");
    println!(
        "[[tokens]]\nname = \"{name}\"\nsha256 = \"{}\"",
        auth::hash_token(&token)
    );
    Ok(())
}
