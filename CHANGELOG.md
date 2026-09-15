# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-09-15

First release. Everything below shipped before a version number existed to
attach it to, so this entry summarizes the whole project to date rather than
a diff against a prior release.

### Added

- **Request and collection files** — a request is a plain YAML file: method,
  URL, headers, body (`body`, `json`, `body_file`, `form`, or `multipart`),
  and authentication (`auth: bearer`/`basic`/`api_key`). A file can also hold
  a named collection of several requests.
- **`sendra run`** — send a request or a whole collection (or one named
  request from it) from the shell, with `-H`/`--header`, `--var`,
  `--timeout`, `--repeat`, `-o`/`--output` (`full`/`status`/`body`/`headers`/
  `none`), `--dry-run`, `--quiet`, `--verbose`, and `--json` for a structured
  output document.
- **`sendra test`** — the same request/collection pipeline as `run`, scored
  against `assertions:` and exited non-zero on failure, for use in CI. Supports
  `--junit` for a JUnit XML report alongside the terminal output.
- **Assertions** — an `assertions:` block that checks a response's status,
  headers, and body, including JSON-path checks and a richer operator
  sub-language (`greater_than`, `matches`, `not:`, and more).
- **Capturing and chaining requests** — a `capture:` block that pulls a value
  out of one response (from JSON via JSON path, a response header, or the
  status code) and substitutes it into a later request in the same run.
- **Environments and variable substitution** — `.sendra/environments/*.yaml`
  files resolved via `--env`, with `{{variable}}` and `${OS_VAR}`
  substitution into requests.
- **Project and global configuration** — `.sendra/config.yaml` for default
  headers, timeout, redirect handling, TLS, proxy, and cookie-jar settings,
  resolved project-over-global.
- **CLI overrides** — per-invocation flags that override a file or config
  without editing it: `-H`, `--var`, `--timeout`, `--insecure`, `--proxy`,
  `--client-cert`/`--client-key`, `--cookie-jar`.
- **Scripting** — `pre_request`/`post_request` hooks written in an embedded
  scripting language (Rhai), compiled before anything is sent, with no
  external runtime to install. `hmac_sha256()` and `uuid()` helpers are
  registered into the script engine.
- **OAuth** — an `authorization_code` login flow (`sendra` opens the
  provider's authorization URL in the system browser and runs a local
  callback) for requests that need a token first.
- **`sendra import curl`** — converts a curl command line into a Sendra
  request YAML file.
- **`sendra init`** — scaffolds a new project's `.sendra/` directory.
- **`sendra schema`** — emits the editor-tooling JSON Schemas under `schema/`
  for `request:`/`collection:`/config file autocomplete and validation.
- **The interactive TUI (`sendra tui`, and a bare `sendra`)** — a full-screen
  terminal app, in the same binary as `run`/`test`, for browsing and running
  requests without one shell invocation per request. Includes a welcome
  screen with automatic discovery of nearby collection files, an editable
  request form, multi-collection tabs, per-request run history, an
  environment picker, and a full keybinding cheatsheet. Scripts
  (`pre_request`/`post_request`) are not run from the TUI.
- **Unified binary** — `sendra-cli` and `sendra-tui` build into one `sendra`
  binary, so installing it gets you the CLI and the TUI together, not a
  CLI-only build.
- **Exit codes** — a single, documented exit-code policy across `run` and
  `test` covering success, assertion failure, and error conditions.

[0.1.0]: https://github.com/sendra-lab/Sendra/releases/tag/v0.1.0
