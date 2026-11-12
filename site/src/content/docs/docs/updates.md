---
title: Update checks
description: "What Mulchie contacts to look for updates, what is on by default, how to turn it all off, and the opt-in advisory check that sends package names to OSV.dev."
---

Use update checks to learn that Mulchie, an agent tool you use or a package on your machine has a newer version. Mulchie reports it and names the command to run. **It never installs or upgrades anything.**

## What it does

```sh
mulchie updates              # the last check and what it found
mulchie updates --check      # ask the engine to check now
```

Checks run in the engine, on a separate thread, only in the engine that holds the lock. The first one waits at least five minutes after the engine starts. After that they repeat every `updates.interval_hours` (default 24), sooner after an error. The last result is saved in `updates.json` in the config directory, so a restart shows it without asking the network again. Findings of the kinds `mulchie_update`, `agent_update`, `homebrew_outdated` and `advisory` carry the result. Over MCP, use `list_updates` and `check_updates`. Over HTTP, `GET /v1/updates` and `POST /v1/updates/check`.

## What is contacted

| Check | What Mulchie contacts | On by default |
| --- | --- | --- |
| Mulchie itself (`updates.mulchie`) | The GitHub releases API: `api.github.com/repos/williamrogersdev/mulchie/releases/latest`. | Yes |
| Agent tools (`updates.agents`) | The npm registry (`registry.npmjs.org`), for the latest version of each tool it finds installed: Claude Code, Codex and Gemini CLI. Cursor's `cursor-agent` has no documented source, so its latest version is not checked. | Yes |
| Homebrew (`updates.homebrew`) | Nothing over the network. It runs `brew outdated` against Homebrew's local data, and only when the local `homebrew/core` tap exists. Otherwise the check is skipped. | Yes |
| Advisories (`updates.advisories`) | `api.osv.dev`. See below. | **No** |

Every request is HTTPS with certificate verification, a 15 second timeout, a cap on the size of the reply, and the user agent `mulchie/<version>`. Nothing else identifies you. Plain HTTP is refused.

### Agent tools

Mulchie looks for the tool in your `PATH` and the usual install places, skipping directories inside your workspaces and scan roots. It does **not run the tool** to ask its version. It reads the installed version from package metadata: the `package.json` of the npm package above the program's real path, or the version folder of the install (`Caskroom/<cask>/<version>`). With neither, the installed version is unknown and nothing is claimed. Versions are compared as semantic versions. A version that does not parse never produces an "outdated" claim.

### Homebrew

`brew outdated` runs with a minimal `PATH`, `HOMEBREW_NO_AUTO_UPDATE=1` and `HOMEBREW_NO_INSTALL_FROM_API=1`. Mulchie only runs it when the local core tap exists, because without one and with API installs off Homebrew would fetch the tap. When it is skipped (the setting is off, `brew` is not found, or there is no local core tap), nothing is run, the earlier Homebrew result and its findings are dropped, and the report's `homebrew_skipped` says why: `off`, `no_brew` or `no_core_tap`. If `brew outdated` runs and fails, the last result is kept and the error is listed.

### Advisories (opt-in)

Turn them on only if you accept this: Mulchie reads the `Cargo.lock`, `package-lock.json` and `go.mod` at the root of each workspace in use and sends the **package names and versions** they list to `api.osv.dev` (the OSV.dev vulnerability database), then fetches the details of any advisory that matches. That tells a third party what your projects depend on. It sends no paths, repository names or file contents. The number of packages and workspaces per run is capped (20000 packages, 50 workspaces).

```sh
mulchie config set updates.advisories true
```

A match is a finding of kind `advisory`, one per workspace and advisory, with the fixed version when OSV lists one.

## Turn it off

```sh
mulchie config set updates.check off        # no request and no command at all
mulchie config set updates.advisories false # the default
mulchie config set updates.agents false     # one source at a time
mulchie config set updates.interval_hours 168
```

With `updates.check` set to `off`, Mulchie makes no network request and runs no version command or `brew`. The interval is 6 to 720 hours.

A Mulchie built without the `network` feature (`cargo build --no-default-features`) cannot make these requests at all: every check reports that the build has no network support and nothing leaves the machine. The release tarballs, the app and a plain `cargo install` use the default features, which include it.

## The menu bar app

The app (built from source for now; there is no app download yet) has its own updater, separate from this. Choosing **Check for Updates…** in its menu fetches `latest.json` from the project's GitHub Releases and, if you agree, downloads and installs the new version. It makes that request only when you choose it, and only in builds that have an update key. See [Install](/install/#update).
