---
title: Running headless
description: "Run the Mulchie engine and API without the menu bar app, on Linux with a systemd user unit or on macOS with a launchd agent."
---

The core, CLI, API and MCP server run without the app. Use this on Linux, on a build server, or on a Mac without a menu bar.

## Start the engine

```bash
mulchie serve
```

This runs the engine and the local API in the foreground. It handles SIGTERM and SIGINT and exits cleanly.

Exactly one engine runs per user. `mulchie serve` and the app both take a lock on `~/.config/mulchie/engine.lock`. The one that loses the lock becomes a client of the other, so you can run either without conflicts.

## Install as a service

```bash
mulchie service install
mulchie service status
mulchie service uninstall
```

<!-- VERIFY: `mulchie service` is described in docs/ARCHITECTURE.md; confirm subcommands, unit names and paths against the shipped CLI. -->

This sets up a systemd user unit on Linux and a launchd agent on macOS.

## Linux: systemd user unit

If you prefer to write the unit yourself, create `~/.config/systemd/user/mulchie.service`:

```ini
[Unit]
Description=Mulchie engine
After=default.target

[Service]
ExecStart=%h/.local/bin/mulchie serve
Restart=on-failure

[Install]
WantedBy=default.target
```

<!-- VERIFY: ExecStart path depends on where the install script puts the binary. -->

Then enable it:

```bash
systemctl --user daemon-reload
systemctl --user enable --now mulchie.service
journalctl --user -u mulchie.service -f
```

To keep it running after you log out, enable lingering with `loginctl enable-linger $USER`.

## macOS: launchd agent

Create `~/Library/LaunchAgents/com.mulchie.engine.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.mulchie.engine</string>
  <key>ProgramArguments</key>
  <array>
    <string>/opt/homebrew/bin/mulchie</string>
    <string>serve</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
</dict>
</plist>
```

<!-- VERIFY: label and binary path. Homebrew installs to /opt/homebrew/bin on Apple silicon and /usr/local/bin on Intel. -->

```bash
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.mulchie.engine.plist
```

Do not run the app and a launchd engine expecting two engines: the second becomes a client of the first.

## Notifications

On macOS, `serve` sends low-disk notifications with `osascript`. On Linux it uses `notify-send` when it is installed.

## Use it from another tool

With the engine running, the CLI, `mulchie mcp` and the [HTTP API](/docs/http-api/) all talk to it. Read commands work without an engine by falling back to a local scan.
