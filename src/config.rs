//! The configuration file: TOML format, built-in defaults, validation and the `init` template.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use axum::http::HeaderName;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:4765";
const DEFAULT_CLIENT_IP_HEADER: &str = "cf-connecting-ip";

/// File patterns hidden from the file endpoints in every workspace.
pub const DEFAULT_DENY: &[&str] = &[
    ".git",
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "*.kdbx",
    "id_rsa*",
    "id_ecdsa*",
    "id_ed25519*",
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".credentials.json",
    "auth.json",
];

// ---------------------------------------------------------------------------------------------
// File format
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    server: RawServer,
    #[serde(default)]
    tokens: Vec<RawToken>,
    #[serde(default)]
    limits: Limits,
    #[serde(default)]
    sessions: RawSessions,
    #[serde(default)]
    experimental: Experimental,
    #[serde(default)]
    agents: BTreeMap<String, RawAgent>,
    #[serde(default)]
    workspaces: Vec<RawWorkspace>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawServer {
    listen: Option<String>,
    name: Option<String>,
    client_ip_header: Option<String>,
    state_dir: Option<PathBuf>,
    tls: Option<RawTls>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawTls {
    #[serde(default)]
    hostnames: Vec<String>,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawToken {
    name: String,
    sha256: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawSessions {
    runner: Option<RunnerChoice>,
    tmux: Option<String>,
    tmux_socket: Option<String>,
    max_turn_secs: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawAgent {
    kind: Option<AgentKind>,
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<BTreeMap<String, String>>,
    headless_args: Option<Vec<String>>,
    bypass_args: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkspace {
    path: PathBuf,
    name: Option<String>,
    allow: Option<BTreeSet<Capability>>,
    agents: Option<Vec<String>>,
    #[serde(default)]
    allow_bypass_permissions: bool,
    #[serde(default)]
    deny: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Resolved configuration
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
pub struct Config {
    pub path: PathBuf,
    pub server: Server,
    pub tokens: Vec<Token>,
    pub limits: Limits,
    pub sessions: SessionSettings,
    pub experimental: Experimental,
    /// Built-in agents first (claude, codex, hermes), then extra agents by name.
    pub agents: Vec<Agent>,
    pub workspaces: Vec<Workspace>,
}

#[derive(Debug)]
pub struct Server {
    pub listen: SocketAddr,
    pub name: String,
    pub client_ip_header: Option<HeaderName>,
    pub state_dir: PathBuf,
    pub tls: Option<TlsSettings>,
}

#[derive(Debug)]
pub struct TlsSettings {
    pub hostnames: Vec<String>,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
}

#[derive(Debug)]
pub struct Token {
    pub name: String,
    pub sha256: [u8; 32],
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    pub auth_failures: u32,
    pub auth_window_secs: u64,
    pub auth_block_secs: u64,
    pub requests_per_minute: u32,
    pub max_sessions: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            auth_failures: 10,
            auth_window_secs: 600,
            auth_block_secs: 900,
            requests_per_minute: 300,
            max_sessions: 8,
        }
    }
}

#[derive(Debug)]
pub struct SessionSettings {
    pub runner: RunnerChoice,
    pub tmux: String,
    /// A dedicated tmux server (`tmux -L <name>`); `None` uses the user's default server.
    pub tmux_socket: Option<String>,
    pub max_turn_secs: u64,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RunnerChoice {
    #[default]
    Auto,
    Tmux,
    Headless,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Experimental {
    /// Message agent sessions that run outside tmux (Claude inbox socket, `codex queue`).
    pub external_sessions: bool,
}

#[derive(Clone, Debug)]
pub struct Agent {
    pub name: String,
    pub kind: AgentKind,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub headless_args: Vec<String>,
    pub bypass_args: Vec<String>,
}

/// Which agent CLI an agent entry drives. `generic` is any other terminal program; it can only
/// run in tmux.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
    Codex,
    Hermes,
    Generic,
}

impl AgentKind {
    fn builtin(name: &str) -> Option<AgentKind> {
        match name {
            "claude" => Some(AgentKind::Claude),
            "codex" => Some(AgentKind::Codex),
            "hermes" => Some(AgentKind::Hermes),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Hermes => "hermes",
            AgentKind::Generic => "generic",
        }
    }
}

fn builtin_agent(name: &str, kind: AgentKind) -> Agent {
    let strings = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let (command, headless, bypass) = match kind {
        AgentKind::Claude => (
            "claude",
            strings(&["--permission-mode", "auto"]),
            strings(&["--dangerously-skip-permissions"]),
        ),
        AgentKind::Codex => (
            "codex",
            strings(&["--sandbox", "workspace-write"]),
            strings(&["--dangerously-bypass-approvals-and-sandbox"]),
        ),
        AgentKind::Hermes => ("hermes", vec![], strings(&["--yolo"])),
        AgentKind::Generic => ("", vec![], vec![]),
    };
    Agent {
        name: name.to_string(),
        kind,
        command: command.to_string(),
        args: vec![],
        env: vec![],
        headless_args: headless,
        bypass_args: bypass,
    }
}

/// Folder-level permission granted to clients.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Capability {
    /// List directories and read text files.
    Files,
    /// Read git status, history and diffs.
    Git,
    /// List agent sessions and read their output.
    Sessions,
    /// Send text and keys to sessions; stop sessions the bridge started.
    Input,
    /// Start new agent sessions.
    Start,
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Capability::Files => "files",
            Capability::Git => "git",
            Capability::Sessions => "sessions",
            Capability::Input => "input",
            Capability::Start => "start",
        })
    }
}

#[derive(Debug)]
pub struct Workspace {
    pub name: String,
    /// Canonical absolute path.
    pub root: PathBuf,
    pub allow: BTreeSet<Capability>,
    pub agents: Vec<String>,
    pub allow_bypass_permissions: bool,
    pub deny: DenyRules,
}

impl Workspace {
    /// Whether a path relative to the workspace root matches a deny pattern.
    pub fn is_denied(&self, rel: &Path) -> bool {
        self.deny.matches(rel)
    }

    pub fn allows(&self, capability: Capability) -> bool {
        self.allow.contains(&capability)
    }
}

/// Compiled deny patterns. Patterns without `/` are checked against every path component,
/// patterns with `/` against the whole path relative to the workspace root.
#[derive(Debug, Clone)]
pub struct DenyRules {
    names: GlobSet,
    paths: GlobSet,
}

impl DenyRules {
    pub fn matches(&self, rel: &Path) -> bool {
        let mut joined = String::new();
        for component in rel.components() {
            if let Component::Normal(name) = component {
                let name = name.to_string_lossy();
                if self.names.is_match(name.as_ref()) {
                    return true;
                }
                if !joined.is_empty() {
                    joined.push('/');
                }
                joined.push_str(&name);
            }
        }
        !joined.is_empty() && self.paths.is_match(&joined)
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config file {}", path.display()))?;
        Self::parse(&text, path).with_context(|| format!("invalid config file {}", path.display()))
    }

    pub fn parse(text: &str, path: &Path) -> Result<Config> {
        let raw: RawConfig = toml::from_str(text)?;
        let server = resolve_server(raw.server)?;
        let tokens = resolve_tokens(raw.tokens)?;
        validate_limits(&raw.limits)?;
        let sessions = SessionSettings {
            runner: raw.sessions.runner.unwrap_or_default(),
            tmux: raw.sessions.tmux.unwrap_or_else(|| "tmux".into()),
            tmux_socket: raw.sessions.tmux_socket,
            max_turn_secs: raw.sessions.max_turn_secs.unwrap_or(3600),
        };
        if sessions.max_turn_secs == 0 {
            bail!("[sessions] max_turn_secs must be greater than 0");
        }
        let agents = resolve_agents(raw.agents)?;
        let workspaces = resolve_workspaces(raw.workspaces, &agents)?;
        Ok(Config {
            path: path.to_path_buf(),
            server,
            tokens,
            limits: raw.limits,
            sessions,
            experimental: raw.experimental,
            agents,
            workspaces,
        })
    }

    pub fn agent(&self, name: &str) -> Option<&Agent> {
        self.agents.iter().find(|a| a.name == name)
    }
}

fn resolve_server(raw: RawServer) -> Result<Server> {
    let listen_text = raw.listen.unwrap_or_else(|| DEFAULT_LISTEN.into());
    let listen: SocketAddr = listen_text
        .parse()
        .with_context(|| format!("[server] listen `{listen_text}` is not an `ip:port` address"))?;
    let tls = match raw.tls {
        None => None,
        Some(tls) => {
            if tls.cert.is_some() != tls.key.is_some() {
                bail!(
                    "[server.tls] needs both `cert` and `key`, or neither for a self-signed certificate"
                );
            }
            Some(TlsSettings {
                hostnames: tls.hostnames,
                cert: tls.cert,
                key: tls.key,
            })
        }
    };
    if !listen.ip().is_loopback() && tls.is_none() {
        bail!(
            "[server] listen = \"{listen}\" is reachable from other machines, but TLS is off. Bearer \
             tokens would travel in clear text. Add a [server.tls] section (a self-signed \
             certificate is generated automatically), or listen on 127.0.0.1 behind a Cloudflare \
             Tunnel or reverse proxy."
        );
    }
    let client_ip_header = match raw.client_ip_header.as_deref() {
        None => Some(HeaderName::from_static(DEFAULT_CLIENT_IP_HEADER)),
        Some("") => None,
        Some(name) => Some(HeaderName::try_from(name).with_context(|| {
            format!("[server] client_ip_header `{name}` is not a valid header name")
        })?),
    };
    let name = raw
        .name
        .or_else(|| sysinfo::System::host_name().ok())
        .unwrap_or_else(|| "unknown".into());
    Ok(Server {
        listen,
        name,
        client_ip_header,
        state_dir: raw.state_dir.unwrap_or_else(default_state_dir),
        tls,
    })
}

fn resolve_tokens(raw: Vec<RawToken>) -> Result<Vec<Token>> {
    if raw.is_empty() {
        bail!(
            "no [[tokens]] configured. Run `safe-grokbot-bridge token` and add the printed snippet."
        );
    }
    let mut tokens: Vec<Token> = Vec::new();
    for token in raw {
        if token.name.trim().is_empty() {
            bail!("[[tokens]] name must not be empty");
        }
        if tokens.iter().any(|t| t.name == token.name) {
            bail!("[[tokens]] name `{}` is used twice", token.name);
        }
        let sha256 = parse_sha256(&token.sha256).with_context(|| {
            format!(
                "[[tokens]] `{}`: sha256 must be 64 hex characters (the output of `safe-grokbot-bridge token`)",
                token.name
            )
        })?;
        tokens.push(Token {
            name: token.name,
            sha256,
        });
    }
    Ok(tokens)
}

fn parse_sha256(hex: &str) -> Result<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        bail!("expected 64 hex characters, got {}", hex.len());
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)?;
    }
    Ok(out)
}

fn validate_limits(limits: &Limits) -> Result<()> {
    if limits.auth_failures == 0
        || limits.auth_window_secs == 0
        || limits.auth_block_secs == 0
        || limits.requests_per_minute == 0
        || limits.max_sessions == 0
    {
        bail!("every value in [limits] must be greater than 0");
    }
    Ok(())
}

fn valid_agent_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn resolve_agents(raw: BTreeMap<String, RawAgent>) -> Result<Vec<Agent>> {
    let mut agents: Vec<Agent> = ["claude", "codex", "hermes"]
        .into_iter()
        .map(|name| builtin_agent(name, AgentKind::builtin(name).expect("built-in")))
        .collect();
    for (name, raw) in raw {
        if !valid_agent_name(&name) {
            bail!(
                "[agents.{name}]: agent names may contain only lowercase letters, digits, `-` and `_` (max 32)"
            );
        }
        let builtin_kind = AgentKind::builtin(&name);
        let kind = match (raw.kind, builtin_kind) {
            (Some(kind), Some(builtin)) if kind != builtin => {
                bail!("[agents.{name}] is a built-in {name} agent; `kind` cannot change it")
            }
            (Some(kind), _) => kind,
            (None, Some(builtin)) => builtin,
            (None, None) => AgentKind::Generic,
        };
        let base = builtin_agent(&name, kind);
        let agent = Agent {
            name: name.clone(),
            kind,
            command: raw.command.unwrap_or(base.command),
            args: raw.args.unwrap_or(base.args),
            env: raw.env.unwrap_or_default().into_iter().collect(),
            headless_args: raw.headless_args.unwrap_or(base.headless_args),
            bypass_args: raw.bypass_args.unwrap_or(base.bypass_args),
        };
        if agent.command.trim().is_empty() {
            bail!("[agents.{name}] needs a `command`");
        }
        match agents.iter_mut().find(|a| a.name == name) {
            Some(existing) => *existing = agent,
            None => agents.push(agent),
        }
    }
    Ok(agents)
}

fn resolve_workspaces(raw: Vec<RawWorkspace>, agents: &[Agent]) -> Result<Vec<Workspace>> {
    if raw.is_empty() {
        bail!("no [[workspaces]] configured; add at least one folder the client may access");
    }
    let mut workspaces: Vec<Workspace> = Vec::new();
    for ws in raw {
        let shown = ws.path.display().to_string();
        if !ws.path.is_absolute() {
            bail!("[[workspaces]] path `{shown}` must be absolute");
        }
        let root = dunce::canonicalize(&ws.path)
            .with_context(|| format!("[[workspaces]] path `{shown}` does not exist"))?;
        if !root.is_dir() {
            bail!("[[workspaces]] path `{shown}` is not a directory");
        }
        let name = ws.name.unwrap_or_else(|| {
            root.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "root".into())
        });
        if workspaces.iter().any(|w| w.name == name) {
            bail!("[[workspaces]] name `{name}` is used twice; set distinct `name` values");
        }
        if workspaces.iter().any(|w| w.root == root) {
            bail!("[[workspaces]] path `{shown}` is listed twice");
        }
        let allow = ws.allow.unwrap_or_else(|| {
            [Capability::Files, Capability::Git, Capability::Sessions]
                .into_iter()
                .collect()
        });
        let ws_agents = match ws.agents {
            Some(list) => {
                for agent in &list {
                    if !agents.iter().any(|a| &a.name == agent) {
                        bail!("[[workspaces]] `{name}` lists unknown agent `{agent}`");
                    }
                }
                list
            }
            None => agents.iter().map(|a| a.name.clone()).collect(),
        };
        let deny = build_deny(&ws.deny)
            .with_context(|| format!("[[workspaces]] `{name}`: invalid deny pattern"))?;
        workspaces.push(Workspace {
            name,
            root,
            allow,
            agents: ws_agents,
            allow_bypass_permissions: ws.allow_bypass_permissions,
            deny,
        });
    }
    Ok(workspaces)
}

fn build_deny(extra: &[String]) -> Result<DenyRules> {
    let mut names = GlobSetBuilder::new();
    let mut paths = GlobSetBuilder::new();
    for pattern in DEFAULT_DENY
        .iter()
        .copied()
        .chain(extra.iter().map(String::as_str))
    {
        let pattern = pattern.trim().trim_start_matches('/');
        if pattern.is_empty() {
            continue;
        }
        // Case-insensitive, because Windows and macOS file systems are.
        let glob = GlobBuilder::new(pattern)
            .case_insensitive(true)
            .literal_separator(true)
            .build()?;
        if pattern.contains('/') {
            paths.add(glob);
        } else {
            names.add(glob);
        }
    }
    Ok(DenyRules {
        names: names.build()?,
        paths: paths.build()?,
    })
}

/// The commented config file written by `safe-grokbot-bridge init`.
pub fn template(token_name: &str, token_hash: &str, workspace: &Path) -> String {
    let workspace = toml::Value::String(workspace.display().to_string());
    format!(
        r#"# safe-grokbot-bridge configuration.
# Reference: https://github.com/janreges/safe-grokbot-bridge/blob/main/docs/configuration.md

[server]
# Loopback only; publish it with a Cloudflare Tunnel (or another reverse proxy).
listen = "{DEFAULT_LISTEN}"
# name = "workstation"                # shown to clients (default: the host name)

# To listen on a public address (e.g. a VPS), enable TLS. A self-signed certificate is created
# on first start; `safe-grokbot-bridge check` prints the pin clients use to verify it.
# Grok Bot can only reach port 443.
# [server.tls]
# hostnames = ["bridge.example.com"]

[[tokens]]
name = "{token_name}"
sha256 = "{token_hash}"

[limits]
# auth_failures = 10                  # failed logins per client IP within auth_window_secs …
# auth_window_secs = 600
# auth_block_secs = 900               # … block the IP this long
# requests_per_minute = 300           # per token
# max_sessions = 8                    # sessions started by the bridge running at once

[sessions]
# runner = "auto"                     # auto (tmux when available) | tmux | headless
# max_turn_secs = 3600                # timeout of one headless turn

# [experimental]
# external_sessions = false           # message Claude Code / Codex sessions running outside tmux

# claude, codex and hermes are built in. Override their command, add flags or environment:
# [agents.claude]
# args = ["--model", "opus"]
# [agents.codex]
# env = {{ CODEX_HOME = "/home/me/.codex" }}

[[workspaces]]
path = {workspace}
# Read-only by default. Add "input" and "start" to let the client drive agents here.
allow = ["files", "git", "sessions"]
# agents = ["claude", "codex"]        # which agents may be started here (default: all)
# allow_bypass_permissions = false    # allow starting agents without permission prompts
"#
    )
}

pub fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .map(|dir| dir.join("safe-grokbot-bridge").join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("safe-grokbot-bridge.toml"))
}

pub fn default_state_dir() -> PathBuf {
    dirs::data_local_dir()
        .map(|dir| dir.join("safe-grokbot-bridge"))
        .unwrap_or_else(|| PathBuf::from(".safe-grokbot-bridge"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "[[tokens]]\nname = \"grok\"\nsha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"\n";

    fn workspace_toml(path: &Path) -> String {
        format!(
            "[[workspaces]]\npath = {}\n",
            toml::Value::String(path.display().to_string())
        )
    }

    fn parse(extra: &str, dir: &Path) -> Result<Config> {
        Config::parse(
            &format!("{TOKEN}{}{extra}", workspace_toml(dir)),
            Path::new("test.toml"),
        )
    }

    #[test]
    fn minimal_config_gets_defaults_and_builtin_agents() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = parse("", dir.path()).unwrap();
        assert_eq!(cfg.server.listen.to_string(), DEFAULT_LISTEN);
        let names: Vec<_> = cfg.agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["claude", "codex", "hermes"]);
        let ws = &cfg.workspaces[0];
        assert!(ws.allows(Capability::Files) && !ws.allows(Capability::Start));
        assert_eq!(ws.agents.len(), 3);
    }

    #[test]
    fn plain_http_on_public_address_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let err = parse("[server]\nlisten = \"0.0.0.0:443\"\n", dir.path()).unwrap_err();
        assert!(err.to_string().contains("TLS is off"), "{err}");
        assert!(
            parse(
                "[server]\nlisten = \"0.0.0.0:443\"\n[server.tls]\n",
                dir.path()
            )
            .is_ok()
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(parse("[server]\nlisen = \"127.0.0.1:1\"\n", dir.path()).is_err());
    }

    #[test]
    fn agent_overrides_keep_unset_defaults_and_extra_agents_reuse_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = parse(
            "[agents.claude]\ncommand = \"/opt/claude\"\n\n[agents.codex-sol]\nkind = \"codex\"\nenv = { CODEX_HOME = \"/x\" }\n",
            dir.path(),
        )
        .unwrap();
        let claude = cfg.agent("claude").unwrap();
        assert_eq!(claude.command, "/opt/claude");
        assert_eq!(claude.headless_args, ["--permission-mode", "auto"]);
        let sol = cfg.agent("codex-sol").unwrap();
        assert_eq!(
            (sol.kind, sol.command.as_str()),
            (AgentKind::Codex, "codex")
        );
        assert_eq!(sol.env, [("CODEX_HOME".to_string(), "/x".to_string())]);
    }

    #[test]
    fn generic_agents_need_a_command() {
        let dir = tempfile::tempdir().unwrap();
        assert!(parse("[agents.aider]\nargs = []\n", dir.path()).is_err());
        let cfg = parse("[agents.aider]\ncommand = \"aider\"\n", dir.path()).unwrap();
        assert_eq!(cfg.agent("aider").unwrap().kind, AgentKind::Generic);
    }

    #[test]
    fn init_template_is_a_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let text = template("grok", &"ab".repeat(32), dir.path());
        let cfg = Config::parse(&text, Path::new("t.toml")).unwrap();
        assert_eq!(cfg.tokens[0].name, "grok");
        assert!(!cfg.workspaces[0].allows(Capability::Start));
    }

    #[test]
    fn deny_patterns_match_components_and_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = parse("deny = [\"build/*.log\"]\n", dir.path()).unwrap();
        let ws = &cfg.workspaces[0];
        assert!(ws.is_denied(Path::new(".env")));
        assert!(ws.is_denied(Path::new("app/.ENV")));
        assert!(ws.is_denied(Path::new("keys/id_rsa.pub")));
        assert!(ws.is_denied(Path::new(".git/config")));
        assert!(ws.is_denied(Path::new("build/out.log")));
        assert!(!ws.is_denied(Path::new("src/build/out.log")));
        assert!(!ws.is_denied(Path::new("src/main.rs")));
        assert!(!ws.is_denied(Path::new(".gitignore")));
    }
}
