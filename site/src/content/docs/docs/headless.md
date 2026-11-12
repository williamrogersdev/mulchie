---
title: Running headless
description: "Run the Mulchie engine without the menu bar app: mulchie serve, or mulchie service install for a launchd agent on macOS or a systemd user unit on Linux."
---

Use this page to run Mulchie on Linux, on a build server, or on a Mac without the menu bar app. The engine, CLI, API and MCP server all run without the app, and the app is not available as a download yet.

## Start the engine

```sh
mulchie serve
```

This runs the engine and the local API in the foreground. It stops cleanly on SIGINT or SIGTERM. The port is `api_port` from the config, or `MULCHIE_PORT` for this process.

Exactly one engine runs per user. `mulchie serve` and the app both take a lock on `engine.lock` in the config directory. If an engine is already running, a second `mulchie serve` exits with status 1.

## Run it at login

```sh
mulchie service install
mulchie service status
mulchie service uninstall
```

`install` writes the service file and loads it. It runs `mulchie serve` by the absolute path of the binary, so it does not depend on your shell's `PATH`. It does not replace an existing file with different content unless you pass `--force`. Use `--dry-run` on `install` and `uninstall` to see the file and the commands without running them.

On macOS the menu bar app already runs the engine, so a service is only for a Mac without the app.

### Linux: systemd user unit

`mulchie service install` writes `~/.config/systemd/user/mulchie.service`. The unit it writes is equivalent to this one, with `ExecStart` set to the binary's path:

```ini
[Unit]
Description=Mulchie engine (monitor, build queue, reclaim)
Documentation=https://mulchie.com/docs/
After=default.target

[Service]
Type=simple
ExecStart=%h/.local/bin/mulchie serve
Restart=on-failure
RestartSec=5
Nice=10
NoNewPrivileges=yes

[Install]
WantedBy=default.target
```

To manage it yourself:

```sh
systemctl --user daemon-reload
systemctl --user enable --now mulchie.service
journalctl --user -u mulchie.service -f
```

To keep it running after you log out, enable lingering with `loginctl enable-linger $USER`.

### macOS: launchd agent

`mulchie service install` writes `~/Library/LaunchAgents/dev.mulchie.serve.plist` and loads it with `launchctl bootstrap`. The agent starts at login, restarts if `mulchie serve` exits with an error, and logs to `~/Library/Logs/mulchie-serve.log`.

To remove it yourself:

```sh
launchctl bootout "gui/$(id -u)/dev.mulchie.serve"
```

A reference copy of the plist, with the placeholders `@MULCHIE_BIN@` and `@HOME@` to fill in, is `packaging/launchd/dev.mulchie.serve.plist` in the repository.

## Check it

```sh
mulchie service status
mulchie doctor
```

`service status` shows whether the service is installed, current and loaded. `doctor` also checks the engine itself: the lock holder, port, health and the age of the last sample.

## Notifications

On macOS, `serve` sends low-disk notifications with `osascript`. On Linux it uses `notify-send` when it is installed, and otherwise sends none.

## Limits and updates need the lock holder

Session limits act, and update checks run, only in the engine that holds the lock. On a machine with one engine that is the engine you started. If the app and a service both run, the one that lost the lock is a client of the other. See [Sessions and limits](/docs/sessions/#what-limits-do-not-do) and [Update checks](/docs/updates/).

## Use it from another tool

With the engine running, the CLI, `mulchie mcp` and the [HTTP API](/docs/http-api/) all talk to it. Read commands also work without an engine by looking at the machine locally. `signal`, `group` and the build queue need the engine.
