# Contributing to Sendra

Thanks for considering a contribution. Sendra is young and its conventions
are still settling, so please open an issue before a large PR — it saves
both of us rework.

## Getting set up

```
git clone https://github.com/<org>/sendra.git
cd sendra
cargo build --workspace
cargo test --workspace
```

## Before opening a PR

Run the same checks CI runs:

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

A PR that fails any of these won't be merged, so it's faster to check
locally first.

## Code conventions

- `sendra-core` has no terminal I/O of any kind (no `println!`/`eprintln!`,
  no `clap`, no color formatting). It returns data; `sendra-cli` decides
  what to do with it. This split is what lets a future TUI reuse the core
  engine — please don't add a shortcut that breaks it.
- Unknown fields in any YAML schema are rejected (`deny_unknown_fields`),
  not silently ignored. New schema fields should follow the same rule.
- Errors are typed (`SendraError`/`thiserror`), not stringly-typed or
  `anyhow`-based in `sendra-core`.
- Tests are hermetic — no real network calls in the test suite. Use
  `wiremock`/a stubbed transport for anything that needs to look like an
  HTTP response.

## Commit messages

Explain *why*, not just *what*, especially for a design decision that
could reasonably have gone the other way. Future readers (including us)
benefit far more from the reasoning than from a restated diff.

## Reporting bugs

Open an issue with the request/collection YAML that reproduces it (with
secrets redacted) and the exact command you ran. A repro beats a
description every time.
