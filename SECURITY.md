# Security policy

## Reporting a problem

Report privately through GitHub security advisories:
<https://github.com/williamrogersdev/mulchie/security/advisories/new>

Please do not open a public issue for a vulnerability. Include the version
(`mulchie --version`), your OS, and steps to reproduce. You will get a first reply
within a few days. Fixes ship in a patch release and the advisory credits you
unless you prefer otherwise.

## Supported versions

Only the latest release gets fixes.

## What is in scope

Mulchie runs on your machine, reads process and disk information, and can delete
directories and signal processes. These are the areas that matter most:

- **Deletion safety.** Anything that makes Mulchie delete a path it should not: a
  directory outside the allowed set, a worktree with uncommitted or unpushed work,
  a path that is a symlink out of its root, or a path that changed between the scan
  and the delete.
- **Local API.** The engine listens on `127.0.0.1` with a bearer token. In scope: a way
  to reach it from another machine, a way to call it without the token, a token
  that is readable by other users, and requests from a web page in your browser
  (for example DNS rebinding or cross-origin requests) that the API accepts.
- **Token handling.** Where the token is stored, its file permissions, and whether it
  can leak into logs, process arguments or environment.
- **MCP server.** Tools that act without the safeguards the CLI has.
- **Release integrity.** Installer checksum verification, signing and notarization,
  and the update signature check.

## Out of scope

- Attacks that need you to already run code as your user.
- Reports that Mulchie can read process information. That is its job.
- Findings in a third-party dependency without a path to exploit Mulchie. Report those upstream.
