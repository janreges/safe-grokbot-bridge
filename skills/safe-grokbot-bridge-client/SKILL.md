---
name: safe-grokbot-bridge-client
description: Work with my computers through safe-grokbot-bridge, and connect new ones. Read project folders and git history there, see which Claude Code, Codex and Hermes sessions are running, give them tasks and follow-ups, answer their prompts, and start new sessions. Also connect a new machine by creating its Cloudflare Tunnel and writing the message for the machine's coding agent. Use it whenever a task involves code, repositories or coding agents on my machines, or when I want to connect a machine.
---

# safe-grokbot-bridge client

Each of my machines runs a bridge with an HTTP API. You cannot run commands on my machines
yourself. You can read the folders I allowed, and you can work through coding agents (Claude
Code, Codex, Hermes) that run there. You need `bash` and `curl`.

## Machines

Every machine has secrets in your environment: `SGB_<NAME>_URL` and `SGB_<NAME>_TOKEN`, and
optionally `SGB_<NAME>_PIN` (self-signed TLS) and `SGB_<NAME>_ACCESS_ID` with
`SGB_<NAME>_ACCESS_SECRET` (Cloudflare Access). `<NAME>` is the machine's name in capitals, with
`-` written as `_`. List the machines without printing a secret:

```bash
env | sed -n 's/^SGB_\([A-Z0-9_]*\)_URL=.*/\1/p' | tr 'A-Z_' 'a-z-'
```

A machine that is missing may have been added after this chat started; ask me to start a new
chat.

## Calling the API

Define this helper in your shell (once per shell session). It never prints a secret, and passes
secrets to curl through a pipe, so they do not appear on a command line:

```bash
sgb() { # sgb <machine> <METHOD> <path> [json-file]
  local m; m=$(printf '%s' "$1" | tr '[:lower:]-' '[:upper:]_')
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
- **Write messages that stand alone, and ask for the answer in the reply.** An agent that someone
  is already working with does not know our conversation either. The bridge adds a short note
  saying the message comes from a remote client. You read `last_message`, so do not ask the
  agent to report to other sessions or elsewhere.
- **No reaction is not a reason to send again.** A Claude Code session that runs without
  permission prompts may hold a message from outside for local approval and drop it after a few
  minutes, and the bridge cannot see that. If you sent a message to a session and
  `last_message_at` has not changed after a few minutes of waiting, tell me which session it is
  and that the message may be waiting for approval there. Do not send it again. Do not suggest
  changing Claude Code settings unless I ask how to make this unattended; then explain that
  `crossSessionInbound: accept`, set on purpose in my user settings or with `--settings` for one
  session (a project or local settings file cannot grant it), is the option, and that it applies
  to all messages the session receives from other sessions.
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
  no longer needed; a headless session counts against the machine's session limit until then.
  Leave my sessions alone unless I ask.
- **Errors:** read `error.message`, it says what to do.
  - `401`: wrong or missing token. Check the secret name, then tell me.
  - `403`: not allowed in that folder. Do not retry; tell me.
  - `409 busy`: a turn is still running. Wait first.
  - `409 path_changed`: a file changed while it was read (retry once), or the folder of a
    headless session was replaced (start a new session).
  - `429`: wait `Retry-After` seconds.
  - `503`, `agent_exited`, `tmux_failed`, `delivery_failed`: tell me; retrying the same call will
    not help.
- **Incomplete lists:** if a session list has `warnings`, some sources failed. Do not conclude that
  nothing is running.
- **No secrets in chat:** never print, echo or write tokens into chat or files.
- **Changes go through agents:** the file and git endpoints are read-only. Changes are made by
  agent sessions.

## Connect a new machine

When I ask you to connect a machine, go through these steps. Keep each message short.

1. **Agree on the names.** The machine's name: letters, digits and `-`, such as `pc1`. The domain
   I have on Cloudflare, and the machine's host name, by default `<name>-bridge.<domain>`. Remind
   me once that the bridge gives you remote access to that machine, so it should be an isolated
   one, such as a virtual machine or a dedicated WSL 2 distribution:
   https://github.com/janreges/safe-grokbot-bridge/blob/main/docs/deployment.md#where-to-run-it
2. **Check your Cloudflare access** without printing anything secret:
   `[ -n "$CLOUDFLARE_API_TOKEN" ] && [ -n "$CLOUDFLARE_ACCOUNT_ID" ] && echo present`.
   If it is missing, tell me to create an API token in the Cloudflare dashboard (*My Profile →
   API Tokens → Create Token → Create Custom Token*) with these permissions, limited to my domain
   under *Zone Resources*, and an expiry date if I like:
   - *Account* · *Cloudflare Tunnel* · *Edit*
   - *Zone* · *Zone* · *Read*
   - *Zone* · *DNS* · *Edit*

   and to copy my *Account ID* from the account's overview page. Then create a secure secret card
   with `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` for me to fill in, and ask me to repeat
   my request in a new chat: secrets reach only chats started after they were saved. Stop there.
3. **Create the tunnel.** Use this helper. It sends the token in a header, never on a command
   line, and hides tokens in the responses:

   ```bash
   cf() { # cf <METHOD> <path> [json-file]
     curl -sS --max-time 60 -X "$1" -H 'Content-Type: application/json' \
          -H @<(printf 'Authorization: Bearer %s\n' "$CLOUDFLARE_API_TOKEN") \
          "https://api.cloudflare.com/client/v4$2" ${3:+--data-binary "@$3"} |
       sed -E 's/"(token|tunnel_secret|TunnelSecret)" *: *"[^"]*"/"\1":"(hidden)"/g'
   }
   ```

   With the names agreed in step 1, and the ids from the answers:
   1. Find the zone: `cf GET "/zones?name=<domain>"`.
   2. Look for what exists: `cf GET "/accounts/$CLOUDFLARE_ACCOUNT_ID/cfd_tunnel?name=sgb-<name>&is_deleted=false"`
      and `cf GET "/zones/<zone id>/dns_records?name=<host name>"`. If either one exists, show it
      to me and ask before you change anything.
   3. Create the tunnel: `cf POST "/accounts/$CLOUDFLARE_ACCOUNT_ID/cfd_tunnel" <file>` with
      `{"name": "sgb-<name>", "config_src": "cloudflare"}`.
   4. Route the host name to the bridge, which listens on the machine's loopback:
      `cf PUT "/accounts/$CLOUDFLARE_ACCOUNT_ID/cfd_tunnel/<tunnel id>/configurations" <file>`
      with `{"config": {"ingress": [{"hostname": "<host name>", "service": "http://127.0.0.1:4765"}, {"service": "http_status:404"}]}}`.
   5. Create the DNS record: `cf POST "/zones/<zone id>/dns_records" <file>` with
      `{"type": "CNAME", "name": "<host name>", "content": "<tunnel id>.cfargotunnel.com", "proxied": true}`.

   Never print the tunnel token; I copy it from the dashboard myself. Until the machine connects,
   the host name answers with Cloudflare error 530, which is expected.
4. **Hand over to the machine.** Give me this message, filled in, in a code block I can copy:

   ```text
   Use the safe-grokbot-bridge skill to set up this machine for Grok Bot as "<name>": Cloudflare Tunnel "sgb-<name>" with the host name https://<host name>, routed to http://127.0.0.1:4765.
   ```

   Tell me to paste it on the machine into Claude Code, Codex or Hermes with the
   `safe-grokbot-bridge` skill installed
   (https://github.com/janreges/safe-grokbot-bridge/blob/main/docs/setup.md#quick-setup-recommended).
   When the agent asks for the tunnel token, I copy it in the Cloudflare dashboard: *Zero Trust →
   Networks → Tunnels → sgb-<name>*, install a connector, and the command shown ends with the
   token. When the agent is done, I come back and tell you "<name> is ready".
5. **Finish.** When I say the machine is ready, create a secure secret card with
   `SGB_<NAME>_URL` and `SGB_<NAME>_TOKEN` for me to fill in: the URL is `https://<host name>`,
   and the token comes from the machine, where its agent put it on my clipboard or left it in
   `bridge-token.txt`. Then ask me to start a new chat and say "Test <name>". There, run
   `sgb <name> GET /v1/info` and tell me which folders and agents I can use, and what you may do
   in each.

A second factor with Cloudflare Access is described in step 5 of the setup guide:
https://github.com/janreges/safe-grokbot-bridge/blob/main/docs/setup.md#step-5-optional-a-second-factor-with-cloudflare-access
