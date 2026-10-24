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

## Public documentation

Everything under `docs/` is source content for Sendra's public documentation
site. It is user-facing documentation, not repository or contributor
documentation. Write it for people using Sendra in their own projects; do not
put build instructions, contribution guidance, repository layout, or links to
implementation files there. Keep contributor and repository-specific guidance
in this file or alongside the relevant code and workflow.

Changes under `docs/` are published when they reach `main`, so review them as
content intended for Sendra users.

## Request file paths in public docs

Sendra doesn't require request files to live anywhere in particular, but the
docs assume the layout a user's own project will have: request and collection
files in a `requests/` directory.

```
sendra run requests/req.yaml
sendra run requests/collection.yaml "List users"
```

This repository has no `requests/` directory. The sample files it ships live
in `examples/`, so when you write or edit public docs:

- Use `requests/…` for generic placeholder files (`req.yaml`, `collection.yaml`)
  in commands and prose, since that's what a reader will type in their own
  project.
- Avoid links to `examples/`, source files, and test fixtures in public docs;
  those paths only exist in a repository checkout. Use a generic
  `requests/` path or an inline example instead.
- Keep `examples/` paths in repository-specific material, such as the root
  README and comments in the sample files themselves. Those commands are for
  people running Sendra from a clone of this repository.
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
