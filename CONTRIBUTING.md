# Contributing to Sendra

Thanks for considering a contribution. Sendra is young and its conventions
are still settling, so please open an issue before a large PR. It saves
both of us rework.

## Branching and pull requests

`main` is the protected, stable branch. It only moves via merges from `dev`.
`dev` is the integration branch: create your feature/fix branch from `dev`,
and open your PR against `dev`, not `main`.

```
dev -> create feature/fix branch -> make changes -> PR into dev
```

## Getting set up

```
git clone https://github.com/sendra-lab/Sendra.git
cd sendra
git checkout dev
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
  no `clap`, no color formatting, no ratatui). It returns data; `sendra-cli`
  and `sendra-tui` each decide what to do with it. This split is what lets
  both reuse the same core engine, so please don't add a shortcut that breaks
  it.
- Unknown fields in any YAML schema are rejected (`deny_unknown_fields`),
  not silently ignored. New schema fields should follow the same rule.
- Errors are typed (`SendraError`/`thiserror`), not stringly-typed or
  `anyhow`-based in `sendra-core`.
- Tests are hermetic: no real network calls in the test suite. Use
  `wiremock`/a stubbed transport for anything that needs to look like an
  HTTP response.

## Request file paths in docs and examples

Sendra doesn't require request files to live anywhere in particular, but the
docs assume the layout a user's own project will have: request and collection
files in a `requests/` directory.

```
sendra run requests/req.yaml
sendra run requests/collection.yaml "List users"
```

This repository has no `requests/` directory. The sample files it ships live
in `examples/`, so when you write or edit docs:

- Use `requests/…` for generic placeholder files (`req.yaml`, `collection.yaml`)
  in commands and prose, since that's what a reader will type in their own
  project.
- Use `examples/…` only when naming one of the real sample files in this
  repository (for example `examples/get-request.yaml`), including in links and
  in the header comments of the files under `examples/` themselves, which are
  run from a clone of this repo.
- Don't create a `requests/` directory here just to make a doc example
  runnable, and don't rewrite `examples/…` paths to `requests/…`; the first
  would add an empty convention to the repo and the second would break
  commands people copy from a clone.

## Commit messages

Explain _why_, not just _what_, especially for a design decision that
could reasonably have gone the other way. Future readers (including us)
benefit far more from the reasoning than from a restated diff.

## Reporting bugs

Open an issue with the request/collection YAML that reproduces it (with
secrets redacted) and the exact command you ran. A repro beats a
description every time.
