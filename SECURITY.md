# Security policy

`safe-grokbot-bridge` lets a remote client make coding agents work on your machine, so its
security matters. Thank you for reporting problems responsibly.

## Reporting a vulnerability

Please do **not** open a public issue. Report it privately through
[GitHub's private vulnerability reporting](https://github.com/janreges/safe-grokbot-bridge/security/advisories/new).

Include the version (`safe-grokbot-bridge --version`), your platform, the relevant config
(with token hashes removed), and steps to reproduce. You can expect a first answer within a
week. Fixes are released as soon as possible, and you are credited unless you prefer otherwise.

## Supported versions

Only the latest release receives security fixes.

## Security model in brief

The full threat model is in [docs/design.md](docs/design.md#6-security-model).

- A token is as powerful as what its bridge allows: reading the configured folders, and, where
  `input`/`start` are allowed, making agents work there, possibly without permission prompts
  where `allow_bypass_permissions` is set. Treat tokens like SSH keys and rotate them by editing
  `[[tokens]]`.
- Tokens are 256-bit random values. The config stores only their SHA-256, compared in constant
  time.
- Plain HTTP is accepted only on loopback. Public listeners require TLS.
- Failed authentications block the client address for a while. Requests are rate-limited per
  token, and every request is logged with the token name.
- The bridge never runs arbitrary commands. Git and agents are started from argument vectors
  (never a shell) with validated arguments, and git runs with options that keep repository config
  from executing programs.

Out of scope:

- a compromised client (for Grok Bot: the account and its cloud computer);
- the behaviour of the agents themselves, including prompt injection through repository content.

Agents keep their own permission systems. Use a dedicated machine or VM when you need hard
isolation.

## Hardening checklist

- Keep `listen` on `127.0.0.1` and publish the bridge through a Cloudflare Tunnel. Add an Access
  service-token policy as a second factor.
- Grant `input` and `start` only on the folders that need them. Keep `allow_bypass_permissions`
  off unless the machine is dedicated to agents.
- Use one token per client and machine, and remove unused ones.
- Run the bridge as your normal user, never as root.
- Watch the logs for `request rejected` and `blocking client` lines.
