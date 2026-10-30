//! Command-line entry point of safe-grokbot-bridge.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use safe_grokbot_bridge::agents::{HookSpec, Launch};
use safe_grokbot_bridge::config::{self, AgentKind, Capability, Config};
use safe_grokbot_bridge::tmux::Tmux;
use safe_grokbot_bridge::{api, auth, exec, hook, paths, server, sessions, tls};

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
    /// Write a config file with a new access token (and print the token once).
    Init {
        #[command(flatten)]
        config: ConfigArg,
        /// Folder to expose as the first workspace [default: the current folder]
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Overwrite an existing config file.
        #[arg(long)]
        force: bool,
    },
    /// Generate a new access token and print the config snippet that authorizes it.
    Token {
        /// Name recorded with the token; it appears in the audit log.
        #[arg(long, default_value = "grok")]
        name: String,
    },
    /// Validate the config and show what the bridge will expose.
    Check(ConfigArg),
    /// Start an agent in a new tmux session in the current folder and attach to it, so the
    /// bridge can reach it. Extra arguments after `--` go to the agent.
    Launch {
        /// Agent name, e.g. `claude` or `codex`.
        agent: String,
        #[command(flatten)]
        config: ConfigArg,
        /// Extra arguments for the agent.
        #[arg(last = true)]
        args: Vec<String>,
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
        Command::Init {
            config,
            workspace,
            force,
        } => init(&config.path(), workspace, force),
        Command::Token { name } => token(&name),
        Command::Check(args) => check(&args.path()),
        Command::Launch {
            agent,
            config,
            args,
        } => launch(&config.path(), &agent, args),
        Command::Openapi => {
            println!("{}", api::openapi().to_pretty_json()?);
            Ok(())
        }
        Command::Hook {
            agent,
            file,
            payload,
        } => {
            hook::run_cli(&agent, &file, payload);
            Ok(())
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")
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

fn serve(path: &Path) -> Result<()> {
    init_logging();
    let config = Config::load(path)?;
    runtime()?.block_on(server::run(config))
}

fn token(name: &str) -> Result<()> {
    let token = auth::generate_token();
    println!("Token (shown only once, store it in your client's secret store):\n\n  {token}\n");
    println!("Add this to the config file and restart the bridge:\n");
    println!(
        "[[tokens]]\nname = {}\nsha256 = \"{}\"",
        toml::Value::String(name.to_string()),
        auth::hash_token(&token)
    );
    Ok(())
}

fn init(path: &Path, workspace: Option<PathBuf>, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            path.display()
        );
    }
    let workspace = match workspace {
        Some(dir) => dir,
        None => std::env::current_dir()?,
    };
    let workspace = dunce::canonicalize(&workspace)
        .with_context(|| format!("workspace folder {} does not exist", workspace.display()))?;
    let token = auth::generate_token();
    let text = config::template("grok", &auth::hash_token(&token), &workspace);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))?;
    println!("Wrote {}", path.display());
    println!("\nAccess token (shown only once; the config stores only its hash):\n\n  {token}\n");
    println!("Next steps:");
    println!(
        "  1. Review the workspaces in the config. They are read-only until you add \"input\" and \"start\"."
    );
    println!("  2. safe-grokbot-bridge check");
    println!("  3. safe-grokbot-bridge serve");
    println!(
        "  4. Publish it, e.g. with a Cloudflare Tunnel to http://{}",
        config::DEFAULT_LISTEN
    );
    println!("  5. Give the URL and the token to your client (see docs/grok-bot.md for Grok Bot).");
    Ok(())
}

fn check(path: &Path) -> Result<()> {
    let config = Config::load(path)?;
    let mut warnings: Vec<String> = Vec::new();
    println!("Config       {}", path.display());
    let listen = config.server.listen;
    match &config.server.tls {
        None => println!("Listen       http://{listen} (this machine only)"),
        Some(settings) => {
            let material =
                tls::load_or_create(settings, &config.server.state_dir, &config.server.name)?;
            println!("Listen       https://{listen}");
            match material.pin {
                Some(pin) => {
                    println!("TLS          self-signed {}", material.cert_path.display());
                    println!("Pin          {pin}");
                    println!(
                        "             clients: curl --pinnedpubkey \"{pin}\" -k https://<host>/v1/info"
                    );
                }
                None => println!("TLS          {}", material.cert_path.display()),
            }
            if listen.port() != 443 {
                warnings.push(
                    "Grok Bot can only reach port 443; forward 443 to this port or listen on 443."
                        .into(),
                );
            }
        }
    }
    let names: Vec<&str> = config.tokens.iter().map(|t| t.name.as_str()).collect();
    println!("Tokens       {}", names.join(", "));

    let rt = runtime()?;
    let tmux = Tmux::new(
        &config.sessions.tmux,
        config.sessions.tmux_socket.as_deref(),
    );
    match rt.block_on(tmux.version()) {
        Some(version) => println!("tmux         {version}"),
        None => {
            println!(
                "tmux         not found ({}): sessions run headless",
                config.sessions.tmux
            );
            if config.sessions.runner == config::RunnerChoice::Tmux {
                warnings.push("[sessions] runner = \"tmux\", but tmux is not available.".into());
            }
        }
    }
    let git = rt.block_on(async {
        let mut cmd = tokio::process::Command::new("git");
        cmd.arg("--version");
        exec::run(cmd, None, std::time::Duration::from_secs(5), 1024)
            .await
            .ok()
    });
    match git.filter(|out| out.success()) {
        Some(out) => println!("git          {}", out.stdout.trim()),
        None => warnings.push("git was not found; the git endpoints will fail.".into()),
    }

    println!("Agents");
    let mut available = 0;
    for agent in &config.agents {
        match exec::which_with(&agent.command, &agent.env) {
            Some(found) => {
                available += 1;
                println!(
                    "  {:<12} {:<8} {}",
                    agent.name,
                    agent.kind.as_str(),
                    found.display()
                );
            }
            None => println!(
                "  {:<12} {:<8} not found: {}",
                agent.name,
                agent.kind.as_str(),
                agent.command
            ),
        }
    }
    if available == 0 {
        warnings.push("No agent command was found on PATH.".into());
    }

    println!("Workspaces");
    for ws in &config.workspaces {
        let allow: Vec<String> = ws.allow.iter().map(|c| c.to_string()).collect();
        println!("  {:<20} {}", ws.name, ws.root.display());
        println!(
            "  {:<20} allow: {}; agents: {}",
            "",
            allow.join(", "),
            ws.agents.join(", ")
        );
        if ws.root.parent().is_none() {
            warnings.push(format!(
                "Workspace `{}` covers the whole file system.",
                ws.name
            ));
        }
        if ws.allow_bypass_permissions && ws.allows(Capability::Start) {
            warnings.push(format!(
                "Workspace `{}` lets clients start agents without permission prompts.",
                ws.name
            ));
        }
    }
    if config.experimental.external_sessions {
        warnings.push("[experimental] external_sessions is on: clients can message agent sessions running outside tmux.".into());
    }
    if warnings.is_empty() {
        println!("\nOK");
    } else {
        println!("\nWarnings");
        for warning in warnings {
            println!("  - {warning}");
        }
    }
    Ok(())
}

fn launch(path: &Path, agent_name: &str, extra: Vec<String>) -> Result<()> {
    let config = Config::load(path)?;
    let mut agent = config.agent(agent_name).cloned().ok_or_else(|| {
        let names: Vec<&str> = config.agents.iter().map(|a| a.name.as_str()).collect();
        anyhow!(
            "unknown agent `{agent_name}`; configured agents: {}",
            names.join(", ")
        )
    })?;
    // Arguments that pick a conversation cannot be combined with a new session id.
    let picks_conversation = extra.iter().any(|arg| {
        let name = arg.split('=').next().unwrap_or(arg);
        [
            "-r",
            "-c",
            "--resume",
            "--continue",
            "--session-id",
            "--from-pr",
        ]
        .contains(&name)
    });
    agent.args.extend(extra);
    let cwd = dunce::canonicalize(std::env::current_dir()?)?;
    if paths::workspace_for(&config, &cwd).is_none_or(|ws| !ws.allows(Capability::Sessions)) {
        eprintln!(
            "warning: {} is not in a workspace with the `sessions` capability; the bridge will not see this session",
            cwd.display()
        );
    }
    let name = sessions::new_session_name(&cwd, &agent.name);
    let tracked = matches!(agent.kind, AgentKind::Claude | AgentKind::Codex);
    let events_dir = config.server.state_dir.join("events");
    if tracked {
        tls::create_private_dir(&events_dir)?;
    }
    let launch = Launch {
        claude_session_id: (agent.kind == AgentKind::Claude && !picks_conversation).then(uuid_v4),
        hook: tracked.then(|| HookSpec {
            exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("safe-grokbot-bridge")),
            events: events_dir.join(format!("{name}.jsonl")),
        }),
        ..Launch::default()
    };
    let mut argv = agent
        .interactive_argv(&cwd, &launch)
        .map_err(|e| anyhow!(e.message))?;
    argv[0] = sessions::resolve_program(&argv[0], &agent).map_err(|e| anyhow!(e.message))?;
    let tmux = Tmux::new(
        &config.sessions.tmux,
        config.sessions.tmux_socket.as_deref(),
    );
    runtime()?
        .block_on(tmux.new_session(&name, &cwd, &agent.env, &argv))
        .context("cannot start the tmux session")?;

    let mut attach = std::process::Command::new(&config.sessions.tmux);
    if let Some(socket) = &config.sessions.tmux_socket {
        attach.args(["-L", socket]);
    }
    let target = format!("={name}");
    if std::env::var_os("TMUX").is_some() && config.sessions.tmux_socket.is_none() {
        attach.args(["switch-client", "-t", &target]);
    } else {
        attach.args(["attach-session", "-t", &target]);
    }
    let status = attach
        .status()
        .context("cannot attach to the tmux session")?;
    if !status.success() {
        eprintln!("The session keeps running: {}", tmux.attach_command(&name));
    }
    Ok(())
}

fn uuid_v4() -> String {
    uuid::Uuid::new_v4().to_string()
}
