# Running safe-grokbot-bridge as a service

Run the bridge as the user who runs the agents: agents use that user's logins, and tmux sessions
must live in that user's tmux server. Use the real paths from `command -v safe-grokbot-bridge`
and `command -v cloudflared` in the files below; the examples assume `~/.local/bin` and
`/usr/bin`. Secrets stay in files under `~/.config/safe-grokbot-bridge/` with mode 600.

## Linux and WSL 2: systemd user units

WSL 2 needs systemd first. Check with `ps -p 1 -o comm=` (it prints `systemd` when it is on).
Otherwise, after asking the user, add this to `/etc/wsl.conf` with `sudo`:

```ini
[boot]
systemd=true
```

and let the user run `wsl.exe --shutdown` from Windows. Warn them first: it ends every program
in WSL, this conversation too if it runs there. They open WSL again afterwards.

`~/.config/systemd/user/safe-grokbot-bridge.service`:

```ini
[Unit]
Description=safe-grokbot-bridge

[Service]
ExecStart=%h/.local/bin/safe-grokbot-bridge serve
Restart=on-failure
RestartSec=5
# Services get a minimal PATH; include where claude, codex, node, git and tmux live.
Environment=PATH=%h/.local/bin:%h/.cargo/bin:/usr/local/bin:/usr/bin:/bin
# Stop only the bridge itself. It stops its own headless turns, while a tmux server it started
# runs in this service's control group and must survive restarts and upgrades.
KillMode=process

[Install]
WantedBy=default.target
```

`~/.config/systemd/user/safe-grokbot-bridge-tunnel.service`:

```ini
[Unit]
Description=Cloudflare Tunnel for safe-grokbot-bridge

[Service]
ExecStart=/usr/bin/cloudflared tunnel --no-autoupdate run --token-file %h/.config/safe-grokbot-bridge/tunnel-token
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

Enable the tunnel's unit only once `tunnel-token` exists; without it `cloudflared` keeps failing.

```bash
systemctl --user daemon-reload
systemctl --user enable --now safe-grokbot-bridge
systemctl --user enable --now safe-grokbot-bridge-tunnel   # after the token file exists
loginctl enable-linger "$USER"        # keep them running without a login session
systemctl --user restart safe-grokbot-bridge                 # after a config change
journalctl --user -u safe-grokbot-bridge -u safe-grokbot-bridge-tunnel -n 50 --no-pager
```

Without systemd, run `safe-grokbot-bridge serve` in a tmux window of its own.

## macOS: launchd agents

launchd does not expand `~`, so write the user's home folder in full (shown as `/Users/me`).
`PATH` must contain the folders of the agents (`which claude codex node git tmux`).

`~/Library/LaunchAgents/com.github.janreges.safe-grokbot-bridge.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.github.janreges.safe-grokbot-bridge</string>
  <key>ProgramArguments</key>
  <array><string>/Users/me/.local/bin/safe-grokbot-bridge</string><string>serve</string></array>
  <key>EnvironmentVariables</key>
  <dict><key>PATH</key><string>/Users/me/.local/bin:/Users/me/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>/tmp/safe-grokbot-bridge.log</string>
</dict>
</plist>
```

`~/Library/LaunchAgents/com.github.janreges.safe-grokbot-bridge-tunnel.plist` (Homebrew installs
`cloudflared` in `/opt/homebrew/bin` on Apple silicon):

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.github.janreges.safe-grokbot-bridge-tunnel</string>
  <key>ProgramArguments</key>
  <array>
    <string>/opt/homebrew/bin/cloudflared</string><string>tunnel</string><string>--no-autoupdate</string>
    <string>run</string><string>--token-file</string><string>/Users/me/.config/safe-grokbot-bridge/tunnel-token</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>/tmp/safe-grokbot-bridge-tunnel.log</string>
</dict>
</plist>
```

```bash
launchctl load ~/Library/LaunchAgents/com.github.janreges.safe-grokbot-bridge.plist
launchctl load ~/Library/LaunchAgents/com.github.janreges.safe-grokbot-bridge-tunnel.plist
launchctl kickstart -k "gui/$(id -u)/com.github.janreges.safe-grokbot-bridge"   # restart
tail -n 50 /tmp/safe-grokbot-bridge.log
```

## Windows: Task Scheduler

If the agents live in WSL, use the Linux setup inside WSL instead.

- Create a task that runs `safe-grokbot-bridge.exe serve` with the trigger *At log on*, as the
  user, with *Run only when user is logged on*. On the *Settings* tab, clear *Stop the task if it
  runs longer than*; its default of 3 days would stop the bridge.
- Without a tmux-compatible multiplexer, sessions run headless. For live sessions install
  [psmux](https://github.com/psmux/psmux) (`winget install psmux`) and set
  `[sessions] tmux = "psmux"`; this is best effort.
- Agents installed through npm are `.cmd` shims. Point `command` at them, for example
  `command = 'C:\Users\me\AppData\Roaming\npm\codex.cmd'`.
- The tunnel token goes into `%APPDATA%\safe-grokbot-bridge\tunnel-token`. The user saves it in
  PowerShell without showing it:

  ```powershell
  $dir = "$env:APPDATA\safe-grokbot-bridge"; New-Item -ItemType Directory -Force $dir | Out-Null
  $t = Read-Host -AsSecureString "Tunnel token"
  [System.Net.NetworkCredential]::new('', $t).Password | Set-Content -NoNewline "$dir\tunnel-token"
  ```

  A second task at log on runs
  `cloudflared.exe tunnel --no-autoupdate run --token-file "%APPDATA%\safe-grokbot-bridge\tunnel-token"`,
  with the same settings.

## When the bridge runs

- **Linux:** the systemd user units start at boot, without a login, once lingering is enabled.
- **WSL 2:** the units run while the distribution runs. Windows does not start it at boot, and
  systemd services do not keep it running. The user creates a task in Task Scheduler: trigger
  *At log on*, action *Start a program* `wsl.exe` with the arguments
  `-d <distribution> --exec sleep infinity` (the distribution's name is in `$WSL_DISTRO_NAME`),
  and on the *Settings* tab *Stop the task if it runs longer than* cleared. The bridge is then
  reachable while the user is logged on to Windows and the computer is awake.
- **macOS:** a launchd agent runs while the user is logged in. To have the bridge back after a
  restart without a login, turn on automatic login (not available with FileVault). Agents need
  the user's session, Claude Code for example keeps its login in the keychain, so the bridge does
  not run as a system daemon.
- **Windows:** the tasks run while the user is logged on.
