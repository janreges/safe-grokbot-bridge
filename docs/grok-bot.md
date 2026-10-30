# Connecting Grok Bot

[Grok Bot](https://x.ai) agents have a cloud computer with a shell, so they call the bridge with
`curl`. On the bot's side nothing has to be installed.

> **New here?** [Setup with Grok Bot and Cloudflare](setup.md) walks through everything, with
> prompts to paste into Grok Bot and into Claude Code or Codex on your machine. This page is the
> reference for the secrets and the skill.

You set up three things:

1. a bridge per machine, reachable over HTTPS on port 443 (see [deployment](deployment.md)),
2. the bridge URLs and tokens as Grok Bot secrets,
3. the skill below, which teaches your bots how to use the API.

## 1. Collect what the bot needs

For every machine:

- **URL**, e.g. `https://pc1-bridge.example.com` (Cloudflare Tunnel) or `https://vps.example.com`
  (direct TLS).
- **Token**, created with `safe-grokbot-bridge init` or `safe-grokbot-bridge token`. Use a
  separate token per machine.
- **Pin**, only for a self-signed certificate (direct TLS). `safe-grokbot-bridge check` prints it.

## 2. Store them as secrets

Grok Bot keeps API keys in a **secure secret card**. Ask one of your bots to store a secret and
paste the value into the card it shows. The value is not shown in chat. It is exported as an
environment variable to the shell sessions on the bot's cloud computer, so `curl` can use it.
Grok Bot's *Vault* is for website logins, not for API tokens.

Use one name scheme for all machines; the skill relies on it:

| Secret | Example value |
|---|---|
| `SGB_PC1_URL` | `https://pc1-bridge.example.com` |
| `SGB_PC1_TOKEN` | `sgb_…` |
| `SGB_VPS_URL` | `https://vps.example.com` |
| `SGB_VPS_TOKEN` | `sgb_…` |
| `SGB_VPS_PIN` | `sha256//…` (only for self-signed TLS) |
| `SGB_PC1_ACCESS_ID`, `SGB_PC1_ACCESS_SECRET` | Cloudflare Access service token (only with the [optional second factor](setup.md#step-5-optional-a-second-factor-with-cloudflare-access)) |

Keep in mind:

- All bots of one Grok account share the same cloud computer and its secrets, so bots are no
  security boundary. The real boundary is what each bridge config allows: workspaces,
  capabilities and `allow_bypass_permissions`.
- A chat that was started before a secret was saved may not see it. Start a new chat.
- Never paste a token into a chat message. Chats are stored, and the token would end up in the
  conversation.

## 3. Install the skill

Edit the machines table in the skill below to match your secrets. Then paste the whole skill into
a chat with your main bot and say *"Save this as a skill named safe-grokbot-bridge."* Enable it
for every bot that should work with your machines. Skills are enabled per bot (under the bot's
connected apps → private skills).

## The skill

````markdown
---
name: safe-grokbot-bridge
description: Work with my computers through safe-grokbot-bridge. Read project folders and git history there, see which Claude Code, Codex and Hermes sessions are running, give them tasks and follow-ups, answer their prompts, and start new sessions. Use it whenever a task involves code, repositories or coding agents on my machines.
---

# safe-grokbot-bridge

Each of my machines runs a bridge with an HTTP API. You cannot run commands on my machines
yourself. You can read the folders I allowed, and you can work through coding agents (Claude
Code, Codex, Hermes) that run there.

## Machines

| Machine | Secrets | What it is |
|---|---|---|
| pc1 | `SGB_PC1_URL`, `SGB_PC1_TOKEN` | main workstation |
| vps | `SGB_VPS_URL`, `SGB_VPS_TOKEN`, `SGB_VPS_PIN` | server with self-signed TLS |

Optional per machine: `SGB_<MACHINE>_ACCESS_ID` and `SGB_<MACHINE>_ACCESS_SECRET` (Cloudflare
Access service token); the helper below sends them when they exist.

## Calling the API

Define this helper in your shell (once per shell session). It never prints a secret, and passes
secrets to curl through a pipe, so they do not appear on a command line:

```bash
sgb() { # sgb <machine> <METHOD> <path> [json-file]
  local m; m=$(printf '%s' "$1" | tr '[:lower:]' '[:upper:]')
  local url="SGB_${m}_URL" tok="SGB_${m}_TOKEN" pin="SGB_${m}_PIN"
  local aid="SGB_${m}_ACCESS_ID" asec="SGB_${m}_ACCESS_SECRET" tls=()
  [ -n "${!pin}" ] && tls=(-k --pinnedpubkey "${!pin}")
  curl -sS --max-time 120 "${tls[@]}" -X "$2" -H 'Content-Type: application/json' \
       -H @<(printf 'Authorization: Bearer %s\n' "${!tok}"
             [ -n "${!aid}" ] && printf 'CF-Access-Client-Id: %s\nCF-Access-Client-Secret: %s\n' "${!aid}" "${!asec}") \
       "${!url}$3" ${4:+--data-binary "@$4"}
}
sgb pc1 GET /v1/info
```

Write JSON bodies to a file with a quoted heredoc, so quotes, `$` and newlines survive:

```bash
cat > /tmp/start.json <<'EOF'
{"cwd": "/home/me/projects/app", "agent": "claude", "prompt": "…"}
EOF
sgb pc1 POST /v1/sessions /tmp/start.json
```

## Workflow

1. `GET /v1/info` lists the workspaces I allowed, what each one allows (`files`, `git`,
   `sessions`, `input`, `start`) and which agents exist. All paths are absolute.
2. Before you start work in a folder, check `GET /v1/sessions?path=<folder>`. If an agent is
   already working there, do not start a second one in parallel. Continue an idle one, or ask me.
3. Look around if useful: `GET /v1/files?path=…`, `GET /v1/files/content?path=…`,
   `GET /v1/git/log?dir=…`, `/v1/git/status`, `/v1/git/diff`.
4. To start a session, `POST /v1/sessions` with `cwd`, `agent` and `prompt`. Optional fields are
   `runner` (`tmux` or `headless`), `model`, `effort`, `keep_alive` and `resume`. The agent knows
   nothing about our conversation, so write a complete prompt: the goal, the context, how to
   verify the result, and what to report back. In tmux, `prompt` holds at most 8 000 bytes; for
   a longer task, start without `prompt` and send the task as input.
5. To wait, call `GET /v1/sessions/{id}?wait=60` and repeat while `state` is `working`. A note
   saying "Still working" means keep waiting. It does **not** mean the message was lost.
6. To read the result: `last_message` is the agent's answer, `screen` is its terminal.
7. `state: waiting` means the agent asks something: a permission prompt, a menu or a question.
   Read `screen`. Answer a menu or prompt with `{"keys": ["1"]}` or `{"keys": ["Enter"]}`, and a
   question with `{"text": "…"}`. If the right answer is unclear or risky, ask me.
8. To follow up, `POST /v1/sessions/{id}/input` with `{"text": "…"}`, then wait as in step 5.
9. Report to me what the agent did and how you verified it, for example with
   `/v1/git/diff?dir=…` or test output from `last_message`. Do not just relay that it is done.

## Rules

- **Send each message once.** If a request was slow or a wait ended while the agent still works,
  check the session state. Never send the same text again; the agent would do the work twice.
- **Do not wait forever.** If a session is still working after about 15 minutes of waiting, tell
  me what it is doing (from `screen` or `last_message`) and stop waiting, unless I asked you to
  wait until it finishes.
- **Choose the runner:**
  - `tmux` (default) for work I may want to watch or continue myself;
  - `headless` for quick questions;
  - `keep_alive: false` for one-off tasks: the session closes after its answer, and
    `last_message` stays readable. In tmux this works for Claude Code and Codex; for other
    agents use `headless`.
- **Sessions you cannot type into** have `controllable: false` and a `note` saying why. Do not
  look for workarounds; start a new session in that folder if you need one, or ask me.
- **Resume only where the conversation happened:** `resume` works only with the same `cwd` the
  conversation was held in.
- **Bypass only when asked.** Use `bypass_permissions: true` only when I asked for it, or when the
  workspace is clearly meant for it. Otherwise answer permission prompts deliberately.
- **Do not stop what you did not start.** Stop sessions you started with `DELETE` when they are
  no longer needed. Leave my sessions alone unless I ask.
- **Errors:** read `error.message`, it says what to do.
  - `401`: wrong or missing token. Check the secret name, then tell me.
  - `403`: not allowed in that folder. Do not retry; tell me.
  - `409 busy`: a turn is still running. Wait first.
  - `409 path_changed`: the file changed while it was read. Retry once.
  - `429`: wait `Retry-After` seconds.
  - `503`, `agent_exited`, `delivery_failed`: tell me; retrying the same call will not help.
- **Incomplete lists:** if a session list has `warnings`, some sources failed. Do not conclude that
  nothing is running.
- **No secrets in chat:** never print, echo or write tokens into chat or files.
- **Changes go through agents:** the file and git endpoints are read-only. Changes are made by
  agent sessions.
````

## Tips

- Name your tmux-started sessions' folders well: session ids are built from the folder name and
  the agent (e.g. `sgb-backend-claude-3f9a1c`), which helps both you and the bot.
- Start your own agents with `safe-grokbot-bridge launch claude` in a project folder. The bot can
  then see and continue the conversations you started yourself.
- The bridge has no push notifications yet, so the bot polls with `wait`. For long tasks, ask the
  bot to check back later or use a Grok Bot routine.
