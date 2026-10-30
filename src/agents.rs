//! Everything agent-specific: the command lines for interactive (tmux) and headless runs, and
//! parsing of the agents' machine-readable output.
//!
//! Agent CLIs change often. Keep this the only module that knows their flags and formats.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::config::{Agent, AgentKind};
use crate::error::ApiError;

/// Reasoning effort, mapped to each agent's own flag.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

/// How to launch an agent.
#[derive(Clone, Debug, Default)]
pub struct Launch {
    /// First message. Interactive runs pass it on the command line; headless runs via stdin.
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    /// Disable the agent's permission prompts (its `bypass_args`).
    pub bypass: bool,
    /// Agent session to continue: Claude session id, Codex thread id or Hermes session id.
    pub resume: Option<String>,
    /// Id for a new Claude conversation, so the bridge knows it in advance.
    pub claude_session_id: Option<String>,
    /// Turn-complete hook (interactive Claude and Codex runs).
    pub hook: Option<HookSpec>,
}

#[derive(Clone, Debug)]
pub struct HookSpec {
    /// This executable.
    pub exe: PathBuf,
    /// The session's events file.
    pub events: PathBuf,
}

/// Model names are passed on the command line, so only plain identifiers are accepted
/// (e.g. `opus`, `opus[1m]`, `gpt-6.1-sol`, `anthropic/claude-sonnet-4`).
pub fn validate_model(model: &str) -> Result<(), ApiError> {
    let mut chars = model.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && model.len() <= 128
        && chars.all(|c| c.is_ascii_alphanumeric() || "._:/[]@+-".contains(c));
    ok.then_some(())
        .ok_or_else(|| ApiError::bad_request(format!("`{model}` is not an accepted model name.")))
}

/// Session ids to resume: UUIDs and similar plain identifiers.
pub fn validate_session_ref(id: &str) -> Result<(), ApiError> {
    let mut chars = id.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && id.len() <= 128
        && chars.all(|c| c.is_ascii_alphanumeric() || "._:-".contains(c));
    ok.then_some(()).ok_or_else(|| {
        ApiError::bad_request(format!("`{id}` is not an accepted session id to resume."))
    })
}

fn pair(flag: &str, value: impl Into<String>) -> [String; 2] {
    [flag.to_string(), value.into()]
}

impl Agent {
    /// Command line for an interactive session (run in tmux in `cwd`).
    pub fn interactive_argv(&self, cwd: &Path, launch: &Launch) -> Result<Vec<String>, ApiError> {
        let mut argv = vec![self.command.clone()];
        match self.kind {
            AgentKind::Claude => {
                argv.extend(self.effective_args(launch));
                self.common_flags(launch, &mut argv);
                if launch.bypass {
                    argv.extend(self.bypass_args.iter().cloned());
                }
                if let Some(id) = &launch.resume {
                    argv.extend(pair("--resume", id));
                } else if let Some(id) = &launch.claude_session_id {
                    argv.extend(pair("--session-id", id));
                }
                if let Some(hook) = &launch.hook {
                    argv.extend(pair("--settings", claude_hook_settings(hook)));
                }
                if let Some(prompt) = &launch.prompt {
                    argv.extend(["--".to_string(), prompt.clone()]);
                }
            }
            AgentKind::Codex => {
                if launch.resume.is_some() {
                    argv.push("resume".into());
                }
                argv.extend(self.effective_args(launch));
                self.common_flags(launch, &mut argv);
                if launch.bypass {
                    argv.extend(self.bypass_args.iter().cloned());
                }
                if let Some(hook) = &launch.hook {
                    argv.extend(pair("-c", codex_notify(hook)));
                }
                if launch.resume.is_some() || launch.prompt.is_some() {
                    argv.push("--".into());
                }
                argv.extend(launch.resume.iter().cloned());
                argv.extend(launch.prompt.iter().cloned());
            }
            AgentKind::Hermes => {
                argv.push("chat".into());
                argv.extend(self.effective_args(launch));
                self.common_flags(launch, &mut argv);
                if launch.bypass {
                    argv.extend(self.bypass_args.iter().cloned());
                }
                if let Some(id) = &launch.resume {
                    argv.extend(pair("--resume", id));
                }
                // Hermes restores a resumed session's own folder unless told otherwise.
                argv.extend(pair("--in", cwd.display().to_string()));
                if let Some(prompt) = &launch.prompt {
                    argv.push(format!("--query={prompt}"));
                }
            }
            AgentKind::Generic => {
                if launch.prompt.is_some()
                    || launch.model.is_some()
                    || launch.effort.is_some()
                    || launch.bypass
                    || launch.resume.is_some()
                {
                    return Err(ApiError::bad_request(format!(
                        "Agent `{}` is a generic terminal program: `prompt`, `model`, `effort`, `bypass_permissions` and `resume` are not supported. Start it, then send input.",
                        self.name
                    )));
                }
                argv.extend(self.effective_args(launch));
            }
        }
        Ok(argv)
    }

    /// Command line for one headless turn. The prompt is written to stdin.
    pub fn headless_argv(&self, cwd: &Path, launch: &Launch) -> Result<Vec<String>, ApiError> {
        let permission_args = if launch.bypass {
            &self.bypass_args
        } else {
            &self.headless_args
        };
        let mut argv = vec![self.command.clone()];
        match self.kind {
            AgentKind::Claude => {
                argv.extend(
                    ["-p", "--output-format", "stream-json", "--verbose"].map(String::from),
                );
                argv.extend(self.effective_args(launch));
                self.common_flags(launch, &mut argv);
                argv.extend(permission_args.iter().cloned());
                if let Some(id) = &launch.resume {
                    argv.extend(pair("--resume", id));
                } else if let Some(id) = &launch.claude_session_id {
                    argv.extend(pair("--session-id", id));
                }
            }
            AgentKind::Codex => {
                argv.extend(["exec", "--json", "--skip-git-repo-check"].map(String::from));
                argv.extend(self.effective_args(launch));
                self.common_flags(launch, &mut argv);
                argv.extend(permission_args.iter().cloned());
                if let Some(id) = &launch.resume {
                    argv.extend(["resume".to_string(), id.clone()]);
                }
                argv.push("-".into());
            }
            AgentKind::Hermes => {
                argv.extend(["chat", "-Q", "--query-file", "-"].map(String::from));
                argv.extend(self.effective_args(launch));
                self.common_flags(launch, &mut argv);
                argv.extend(permission_args.iter().cloned());
                if let Some(id) = &launch.resume {
                    argv.extend(pair("--resume", id));
                }
                argv.extend(pair("--in", cwd.display().to_string()));
            }
            AgentKind::Generic => {
                return Err(ApiError::bad_request(format!(
                    "Agent `{}` is a generic terminal program and cannot run headless; use `runner: \"tmux\"`.",
                    self.name
                )));
            }
        }
        Ok(argv)
    }

    /// The configured `args`, minus the model and effort options the launch sets itself (Codex,
    /// for one, rejects a repeated `-m`).
    fn effective_args(&self, launch: &Launch) -> Vec<String> {
        let mut args = self.args.clone();
        if launch.model.is_some() {
            args = strip_option(&args, &["--model", "-m"]);
        }
        if launch.effort.is_some() {
            args = match self.kind {
                AgentKind::Claude => strip_option(&args, &["--effort"]),
                AgentKind::Hermes => strip_option(&args, &["--reasoning"]),
                AgentKind::Codex => strip_config(&args, "model_reasoning_effort"),
                AgentKind::Generic => args,
            };
        }
        args
    }

    /// Model and effort flags, which use the same spelling in both modes.
    fn common_flags(&self, launch: &Launch, argv: &mut Vec<String>) {
        if let Some(model) = &launch.model {
            let flag = if self.kind == AgentKind::Claude {
                "--model"
            } else {
                "-m"
            };
            argv.extend(pair(flag, model));
        }
        if let Some(effort) = launch.effort {
            match self.kind {
                AgentKind::Claude => argv.extend(pair("--effort", effort.as_str())),
                AgentKind::Codex => argv.extend(pair(
                    "-c",
                    format!("model_reasoning_effort=\"{}\"", effort.as_str()),
                )),
                AgentKind::Hermes => argv.extend(pair("--reasoning", effort.as_str())),
                AgentKind::Generic => {}
            }
        }
    }
}

/// Removes `name value` and `name=value` occurrences of the given options, and the attached
/// `-mvalue` form of a short option.
fn strip_option(args: &[String], names: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let attached = names.iter().any(|n| {
            arg.starts_with(&format!("{n}="))
                || (n.len() == 2 && !n.starts_with("--") && arg.len() > 2 && arg.starts_with(n))
        });
        if names.contains(&arg.as_str()) {
            iter.next();
        } else if !attached {
            out.push(arg.clone());
        }
    }
    out
}

/// Removes Codex `-c key=…`, `-ckey=…`, `--config key=…` and `--config=key=…` overrides of one
/// key.
fn strip_config(args: &[String], key: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let prefix = format!("{key}=");
    for arg in args {
        let attached = [format!("-c{prefix}"), format!("--config={prefix}")];
        if attached.iter().any(|a| arg.starts_with(a.as_str())) {
            continue;
        }
        if arg.starts_with(&prefix)
            && out
                .last()
                .is_some_and(|prev| prev == "-c" || prev == "--config")
        {
            out.pop();
        } else {
            out.push(arg.clone());
        }
    }
    out
}

/// `--settings` value adding a Stop hook that records each finished turn.
fn claude_hook_settings(hook: &HookSpec) -> String {
    let command = format!(
        "{} hook claude {}",
        shell_quote(&hook.exe.display().to_string()),
        shell_quote(&hook.events.display().to_string())
    );
    serde_json::json!({
        "hooks": { "Stop": [ { "hooks": [ { "type": "command", "command": command } ] } ] }
    })
    .to_string()
}

/// `-c` override making Codex call the hook after every turn (`notify` program).
fn codex_notify(hook: &HookSpec) -> String {
    let program = toml::Value::Array(vec![
        toml::Value::String(hook.exe.display().to_string()),
        toml::Value::String("hook".into()),
        toml::Value::String("codex".into()),
        toml::Value::String(hook.events.display().to_string()),
    ]);
    format!("notify={program}")
}

/// Quotes a string for POSIX shells (Claude Code runs command hooks through `sh`/Git Bash).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

// ---------------------------------------------------------------------------------------------
// Headless output
// ---------------------------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
pub enum StreamEvent {
    /// The agent's id for this conversation.
    SessionId(String),
    /// An assistant message (progress while the turn runs; the last one is the answer).
    Message(String),
    /// The turn completed successfully.
    Done,
    /// The turn failed with this message.
    Failed(String),
}

/// One line of `claude -p --output-format stream-json`.
pub fn parse_claude_line(line: &str) -> Vec<StreamEvent> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return vec![];
    };
    let mut events = Vec::new();
    match v["type"].as_str() {
        Some("system") if v["subtype"] == "init" => {
            events.extend(
                v["session_id"]
                    .as_str()
                    .map(|id| StreamEvent::SessionId(id.into())),
            );
        }
        Some("assistant") => {
            let text = text_blocks(&v["message"]["content"], "text");
            if !text.trim().is_empty() {
                events.push(StreamEvent::Message(text));
            }
        }
        Some("result") => {
            events.extend(
                v["session_id"]
                    .as_str()
                    .map(|id| StreamEvent::SessionId(id.into())),
            );
            let result = v["result"].as_str().unwrap_or_default();
            let subtype = v["subtype"].as_str().unwrap_or("success");
            if v["is_error"].as_bool() == Some(true) || subtype != "success" {
                events.push(StreamEvent::Failed(if result.is_empty() {
                    format!("Claude stopped with `{subtype}`.")
                } else {
                    result.to_string()
                }));
            } else {
                if !result.trim().is_empty() {
                    events.push(StreamEvent::Message(result.to_string()));
                }
                events.push(StreamEvent::Done);
            }
        }
        _ => {}
    }
    events
}

/// One line of `codex exec --json`.
pub fn parse_codex_line(line: &str) -> Vec<StreamEvent> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return vec![];
    };
    match v["type"].as_str() {
        Some("thread.started") => v["thread_id"]
            .as_str()
            .map(|id| vec![StreamEvent::SessionId(id.into())])
            .unwrap_or_default(),
        Some("item.completed") if v["item"]["type"] == "agent_message" => v["item"]["text"]
            .as_str()
            .filter(|text| !text.trim().is_empty())
            .map(|text| vec![StreamEvent::Message(text.into())])
            .unwrap_or_default(),
        Some("turn.completed") => vec![StreamEvent::Done],
        Some("turn.failed") => vec![StreamEvent::Failed(
            v["error"]["message"]
                .as_str()
                .unwrap_or("The Codex turn failed.")
                .into(),
        )],
        _ => vec![],
    }
}

/// Output of `hermes chat -Q`: the answer on stdout, `session_id: <id>` on stderr.
pub fn parse_hermes_output(stdout: &str, stderr: &str) -> (Option<String>, String) {
    let session_id = stderr
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("session_id:"))
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty());
    let answer = stdout
        .lines()
        .filter(|line| !line.starts_with("↪ restored workspace dir"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    (session_id, answer)
}

fn text_blocks(content: &Value, kind: &str) -> String {
    content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block["type"] == kind)
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn agent(name: &str) -> Agent {
        let dir = tempfile::tempdir().unwrap();
        let text = format!(
            "[[tokens]]\nname = \"t\"\nsha256 = \"{}\"\n[[workspaces]]\npath = {}\n[agents.aider]\ncommand = \"aider\"\n[agents.codex]\nargs = [\"-m\", \"gpt-x\"]\n",
            "0".repeat(64),
            toml::Value::String(dir.path().display().to_string())
        );
        Config::parse(&text, Path::new("t.toml"))
            .unwrap()
            .agent(name)
            .unwrap()
            .clone()
    }

    fn hook() -> HookSpec {
        HookSpec {
            exe: PathBuf::from("/opt/it's/sgb"),
            events: PathBuf::from("/state/e.jsonl"),
        }
    }

    #[test]
    fn claude_command_lines() {
        let claude = agent("claude");
        let launch = Launch {
            prompt: Some("-starts with a dash".into()),
            model: Some("opus".into()),
            effort: Some(Effort::High),
            bypass: true,
            claude_session_id: Some("11111111-2222-3333-4444-555555555555".into()),
            hook: Some(hook()),
            ..Launch::default()
        };
        let argv = claude.interactive_argv(Path::new("/w"), &launch).unwrap();
        assert_eq!(
            argv[..6],
            [
                "claude",
                "--model",
                "opus",
                "--effort",
                "high",
                "--dangerously-skip-permissions"
            ]
        );
        assert_eq!(
            argv[6..8],
            ["--session-id", "11111111-2222-3333-4444-555555555555"]
        );
        assert_eq!(argv[8], "--settings");
        let settings: Value = serde_json::from_str(&argv[9]).unwrap();
        assert_eq!(
            settings["hooks"]["Stop"][0]["hooks"][0]["command"],
            r"'/opt/it'\''s/sgb' hook claude '/state/e.jsonl'"
        );
        assert_eq!(argv[10..], ["--", "-starts with a dash"]);

        let resume = Launch {
            resume: Some("abc".into()),
            ..Launch::default()
        };
        let headless = claude.headless_argv(Path::new("/w"), &resume).unwrap();
        assert_eq!(
            headless,
            [
                "claude",
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-mode",
                "auto",
                "--resume",
                "abc"
            ]
        );
    }

    #[test]
    fn codex_command_lines() {
        let codex = agent("codex");
        let launch = Launch {
            prompt: Some("hi".into()),
            hook: Some(hook()),
            ..Launch::default()
        };
        let argv = codex.interactive_argv(Path::new("/w"), &launch).unwrap();
        assert_eq!(
            argv[..5],
            [
                "codex",
                "-m",
                "gpt-x",
                "-c",
                r#"notify=["/opt/it's/sgb", "hook", "codex", "/state/e.jsonl"]"#
            ]
        );
        assert_eq!(argv[5..], ["--", "hi"]);
        let resume = Launch {
            resume: Some("t-1".into()),
            prompt: Some("go".into()),
            ..Launch::default()
        };
        let argv = codex.interactive_argv(Path::new("/w"), &resume).unwrap();
        assert_eq!(argv, ["codex", "resume", "-m", "gpt-x", "--", "t-1", "go"]);

        let headless = codex
            .headless_argv(
                Path::new("/w"),
                &Launch {
                    resume: Some("t-1".into()),
                    effort: Some(Effort::Max),
                    ..Launch::default()
                },
            )
            .unwrap();
        assert_eq!(
            headless,
            [
                "codex",
                "exec",
                "--json",
                "--skip-git-repo-check",
                "-m",
                "gpt-x",
                "-c",
                "model_reasoning_effort=\"max\"",
                "--sandbox",
                "workspace-write",
                "resume",
                "t-1",
                "-"
            ]
        );
        let bypass = codex
            .headless_argv(
                Path::new("/w"),
                &Launch {
                    bypass: true,
                    ..Launch::default()
                },
            )
            .unwrap();
        assert!(bypass.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
        assert!(!bypass.contains(&"workspace-write".to_string()));
    }

    #[test]
    fn hermes_and_generic_command_lines() {
        let hermes = agent("hermes");
        let argv = hermes
            .interactive_argv(
                Path::new("/w"),
                &Launch {
                    prompt: Some("-x".into()),
                    bypass: true,
                    ..Launch::default()
                },
            )
            .unwrap();
        assert_eq!(
            argv,
            ["hermes", "chat", "--yolo", "--in", "/w", "--query=-x"]
        );
        let headless = hermes
            .headless_argv(
                Path::new("/w"),
                &Launch {
                    resume: Some("s1".into()),
                    ..Launch::default()
                },
            )
            .unwrap();
        assert_eq!(
            headless,
            [
                "hermes",
                "chat",
                "-Q",
                "--query-file",
                "-",
                "--resume",
                "s1",
                "--in",
                "/w"
            ]
        );

        let aider = agent("aider");
        assert_eq!(
            aider
                .interactive_argv(Path::new("/w"), &Launch::default())
                .unwrap(),
            ["aider"]
        );
        assert!(
            aider
                .interactive_argv(
                    Path::new("/w"),
                    &Launch {
                        prompt: Some("x".into()),
                        ..Launch::default()
                    }
                )
                .is_err()
        );
        assert!(
            aider
                .headless_argv(Path::new("/w"), &Launch::default())
                .is_err()
        );
    }

    #[test]
    fn requested_model_and_effort_replace_configured_ones() {
        let mut codex = agent("codex"); // args = ["-m", "gpt-x"]
        codex.args.extend([
            "-c".into(),
            "model_reasoning_effort=\"xhigh\"".into(),
            "--search".into(),
        ]);
        let launch = Launch {
            model: Some("gpt-y".into()),
            effort: Some(Effort::Low),
            ..Launch::default()
        };
        let argv = codex.headless_argv(Path::new("/w"), &launch).unwrap();
        assert_eq!(argv.iter().filter(|a| *a == "-m").count(), 1, "{argv:?}");
        assert!(argv.windows(2).any(|w| w == ["-m", "gpt-y"]), "{argv:?}");
        assert!(!argv.iter().any(|a| a.contains("xhigh")), "{argv:?}");
        assert!(argv.contains(&"--search".to_string()));
        let mut claude = agent("claude");
        claude.args = vec!["--model=opus".into(), "--strict-mcp-config".into()];
        let argv = claude
            .interactive_argv(
                Path::new("/w"),
                &Launch {
                    model: Some("haiku".into()),
                    ..Launch::default()
                },
            )
            .unwrap();
        assert_eq!(argv, ["claude", "--strict-mcp-config", "--model", "haiku"]);
    }

    #[test]
    fn configured_options_in_attached_form_are_replaced_too() {
        let args = |list: &[&str]| -> Vec<String> { list.iter().map(|s| s.to_string()).collect() };
        assert_eq!(
            strip_option(
                &args(&["-mgpt-x", "--strict", "-m", "y", "--model=z"]),
                &["--model", "-m"]
            ),
            args(&["--strict"])
        );
        assert_eq!(
            strip_config(
                &args(&[
                    "-cmodel_reasoning_effort=high",
                    "--config=model_reasoning_effort=low",
                    "-c",
                    "other=1"
                ]),
                "model_reasoning_effort"
            ),
            args(&["-c", "other=1"])
        );
    }

    #[test]
    fn model_and_session_validation() {
        for ok in [
            "opus",
            "opus[1m]",
            "gpt-6.1-sol",
            "anthropic/claude-sonnet-4",
        ] {
            assert!(validate_model(ok).is_ok(), "{ok}");
        }
        for bad in ["-x", "a b", "", "$(id)"] {
            assert!(validate_model(bad).is_err(), "{bad}");
        }
        assert!(validate_session_ref("09412d3d-1dad-4d8b-8a88-8217aa17ba2d").is_ok());
        assert!(validate_session_ref("--last").is_err());
    }

    #[test]
    fn claude_stream_is_parsed() {
        let init = r#"{"type":"system","subtype":"init","session_id":"s-1","cwd":"/w"}"#;
        let assistant = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Working on it"},{"type":"tool_use","name":"Bash"}]},"session_id":"s-1"}"#;
        let ok = r#"{"type":"result","subtype":"success","is_error":false,"result":"All tests pass.","session_id":"s-1"}"#;
        let failed =
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,"session_id":"s-1"}"#;
        assert_eq!(
            parse_claude_line(init),
            [StreamEvent::SessionId("s-1".into())]
        );
        assert_eq!(
            parse_claude_line(assistant),
            [StreamEvent::Message("Working on it".into())]
        );
        assert_eq!(
            parse_claude_line(ok),
            [
                StreamEvent::SessionId("s-1".into()),
                StreamEvent::Message("All tests pass.".into()),
                StreamEvent::Done
            ]
        );
        assert_eq!(
            parse_claude_line(failed)[1],
            StreamEvent::Failed("Claude stopped with `error_max_turns`.".into())
        );
        assert!(parse_claude_line("not json").is_empty());
    }

    #[test]
    fn codex_stream_and_hermes_output_are_parsed() {
        assert_eq!(
            parse_codex_line(r#"{"type":"thread.started","thread_id":"t-9"}"#),
            [StreamEvent::SessionId("t-9".into())]
        );
        assert_eq!(
            parse_codex_line(
                r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"Done."}}"#
            ),
            [StreamEvent::Message("Done.".into())]
        );
        assert!(
            parse_codex_line(
                r#"{"type":"item.completed","item":{"type":"command_execution","command":"ls"}}"#
            )
            .is_empty()
        );
        assert_eq!(
            parse_codex_line(r#"{"type":"turn.completed","usage":{}}"#),
            [StreamEvent::Done]
        );
        assert_eq!(
            parse_codex_line(r#"{"type":"turn.failed","error":{"message":"quota"}}"#),
            [StreamEvent::Failed("quota".into())]
        );
        let (id, answer) =
            parse_hermes_output("The answer.\n", "warming up\n\nsession_id: 20261030_abc\n");
        assert_eq!(
            (id.as_deref(), answer.as_str()),
            (Some("20261030_abc"), "The answer.")
        );
    }
}
