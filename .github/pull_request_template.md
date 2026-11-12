## What changed and why

## How I tested it

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] Added or updated a test
- [ ] Updated `CHANGELOG.md` if users would notice

This merges by itself when the `CI passed` check is green, once auto-merge is on: `gh pr merge --auto --squash`.

## Notes for the reviewer

Anything that deletes files or signals processes: which safety rule applies, and which test covers it.
