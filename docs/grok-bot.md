# Connecting Grok Bot

[Grok Bot](https://x.ai) agents have a cloud computer with a shell, so they call the bridge with
`curl`. On the bot's side you need one skill, and two secrets per machine.

> **New here?** The [quick setup](setup.md#quick-setup-recommended) connects a machine in two
> steps, with a skill on each side. This page is the reference for the bot's skill and secrets.

## The skill

The bot's skill is
[`skills/safe-grokbot-bridge-client/SKILL.md`](../skills/safe-grokbot-bridge-client/SKILL.md).
It teaches the bot to use the API, finds your machines by their secrets, and connects new
machines: it creates the Cloudflare Tunnel and writes the setup message for the coding agent on
the machine.

To install it, paste this into a chat with your main bot:

```text
Download https://raw.githubusercontent.com/janreges/safe-grokbot-bridge/main/skills/safe-grokbot-bridge-client/SKILL.md
with curl and save its content, unchanged, as a skill named safe-grokbot-bridge-client.
Then tell me how to enable it for my other bots.
```

If your bot cannot save a skill from a file, open that link, copy the whole file, and paste it
into the chat with *"Save this as a skill named safe-grokbot-bridge-client."* Skills are enabled
per bot (under the bot's connected apps → private skills). To update the skill later, repeat
the same request.

The skill works for other clients too, for example an agent on your laptop that works with your
machines: `npx skills add janreges/safe-grokbot-bridge --skill safe-grokbot-bridge-client -g`.
It needs `bash`, `curl` and the secrets below as environment variables.

## What the bot needs per machine

- **URL**, e.g. `https://pc1-bridge.example.com` (Cloudflare Tunnel) or `https://vps.example.com`
  (direct TLS).
- **Token**, created with `safe-grokbot-bridge init` or `safe-grokbot-bridge token`. Use a
  separate token per machine.
- **Pin**, only for a self-signed certificate (direct TLS). `safe-grokbot-bridge check` prints it.

## Store them as secrets

Grok Bot keeps API keys in a **secure secret card**. Ask one of your bots to store a secret and
paste the value into the card it shows. The value is not shown in chat. It is exported as an
environment variable to the shell sessions on the bot's cloud computer, so `curl` can use it.
Grok Bot's *Vault* is for website logins, not for API tokens.

The skill finds a machine by its secrets' names: the machine's name in capitals, with `-`
written as `_` (`eva-pc` → `SGB_EVA_PC_URL`):

| Secret | Example value |
|---|---|
| `SGB_PC1_URL` | `https://pc1-bridge.example.com` |
| `SGB_PC1_TOKEN` | `sgb_…` |
| `SGB_VPS_URL` | `https://vps.example.com` |
| `SGB_VPS_TOKEN` | `sgb_…` |
| `SGB_VPS_PIN` | `sha256//…` (only for self-signed TLS) |
| `SGB_PC1_ACCESS_ID`, `SGB_PC1_ACCESS_SECRET` | Cloudflare Access service token (only with the [optional second factor](setup.md#step-5-optional-a-second-factor-with-cloudflare-access)) |
| `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID` | Cloudflare access, only for connecting new machines ([permissions](setup.md#step-1-give-grok-bot-access-to-cloudflare-once)) |

Keep in mind:

- All bots of one Grok account share the same cloud computer and its secrets, so bots are no
  security boundary. The real boundary is what each bridge config allows: workspaces,
  capabilities and `allow_bypass_permissions`.
- A chat that was started before a secret was saved may not see it. Start a new chat.
- Never paste a token into a chat message. Chats are stored, and the token would end up in the
  conversation.

## Tips

- Name your tmux-started sessions' folders well: session ids are built from the folder name and
  the agent (e.g. `sgb-backend-claude-3f9a1c`), which helps both you and the bot.
- Start your own agents with `safe-grokbot-bridge launch claude` in a project folder. The bot can
  then see and continue the conversations you started yourself.
- The bridge has no push notifications yet, so the bot polls with `wait`. For long tasks, ask the
  bot to check back later or use a Grok Bot routine.
