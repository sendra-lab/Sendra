# Sendra

[![CI](https://github.com/dubemoyibe-star/Sendra/actions/workflows/ci.yml/badge.svg)](https://github.com/dubemoyibe-star/Sendra/actions/workflows/ci.yml)

Sendra is a terminal-native HTTP client, think Postman, but your requests are
plain YAML files that live in your repo next to the code they exercise, and you
send them from the shell. A request is just a file: method, URL, headers, body.
That makes requests reviewable in a pull request, diffable over time, and
shareable without exporting anything. A file holds either one request or a
named collection of them, sent and printed. Environments, variables, scripting,
assertions and an interactive TUI are all planned and deliberately absent for
now.

## Layout

```
sendra/
  sendra-core/     library: request/response types, YAML loading, config, HTTP execution
  sendra-cli/      binary `sendra`: argument parsing, output, exit codes
  examples/        sample request and collection files
```

`sendra-core` knows nothing about clap or terminal output. A `sendra-tui` crate
will sit alongside `sendra-cli` later and reuse `sendra-core` directly, so core
returns typed errors (`SendraError`) rather than formatted messages.

## Smoke test

```sh
cargo run -p sendra-cli -- run examples/get-request.yaml
```

That sends a real request to `https://httpbin.org/get` and prints the status,
headers and body. There is also `examples/post-request.yaml`, which posts a JSON
body, and `examples/collection.yaml`, which holds four requests in one file:

```sh
cargo run -p sendra-cli -- run examples/collection.yaml              # all four
cargo run -p sendra-cli -- run examples/collection.yaml "Post JSON"  # just one
```

## Request file shape

```yaml
name: Get user # optional, used as a display label
method: GET # GET | POST | PUT | PATCH | DELETE | HEAD | OPTIONS
url: https://api.example.com/users/1
headers: # optional
  Accept: application/json
body: null # optional, sent verbatim as a raw string
```

Unknown top-level keys are rejected rather than silently ignored, so a typo in a
field name is an error you see immediately.

## Collection file shape

A collection is several named requests in one file — the endpoints of a single
API, say — under a top-level `requests` key:

```yaml
name: Example API # optional, a label for the collection as a whole
requests:
  - name: List users # required here: it is how you select a request
    method: GET
    url: https://api.example.com/users
    headers:
      Accept: application/json
  - name: Create user
    method: POST
    url: https://api.example.com/users
    body: '{"name": "ada"}'
```

Each entry uses exactly the same fields as a standalone request file, so a
request can be lifted into a collection, or pulled back out into its own file,
verbatim. The only extra rule is that `name` is required inside a collection,
must be unique, and `requests` must not be empty; all three are checked when the
file is loaded, before anything is sent.

`requests` is a list rather than a map of name-to-request so that entries stay
identical to single-request files, and so that file order — which is the order
`sendra run` sends them in — survives parsing.

**Which shape is a file?** The presence of a top-level `requests` key, and
nothing else: no separate extension, no CLI flag. It cannot be ambiguous,
because the single-request shape rejects unknown top-level keys and so could
never have carried a `requests` key of its own.

## Running requests

```sh
sendra run req.yaml                    # the one request in the file
sendra run collection.yaml             # every request in it, in file order
sendra run collection.yaml "List users"  # one named request
```

Requests in a collection are sent sequentially, in file order, and each response
is printed as it arrives. A request that fails does not stop the ones after it —
you see every result, and the exit code reports the worst of them.

Asking for a name that is not in the collection is an error that lists the names
that are (`no request named X (available: ...)`), as is passing a name to a file
that holds a single request.

## Configuration

Defaults that apply to every request live in a config file. There are two, both
optional:

| Scope   | Location                                                                       |
| ------- | ------------------------------------------------------------------------------ |
| Project | `.sendra/config.yaml`, searched for from the current directory upwards          |
| Global  | `config.yaml` in your platform's config directory (see below)                   |

```yaml
headers: # merged into every request; a header in the request file wins
  User-Agent: sendra
  Accept: application/json
timeout_seconds: 20 # whole-request timeout: connect, send and body read
```

Those two keys are the whole schema for now. Unknown keys are rejected, like
everywhere else in Sendra, so `timeout` instead of `timeout_seconds` is an error
you see rather than a setting that quietly never applies.

**Finding the project config.** Sendra walks up from the directory you ran it
in, looking for `.sendra/config.yaml`, the same way git looks for `.git`. So a
config at the repository root applies from anywhere inside the repository. The
nearest one wins; configs further up are not stacked on top of each other. The
search starts at the working directory, not at the request file's directory, so
`sendra run ../other-project/req.yaml` still uses *your* defaults.

**Finding the global config.** `$XDG_CONFIG_HOME/sendra/config.yaml` when
`XDG_CONFIG_HOME` is set to an absolute path, on any platform. Otherwise the
platform's own config directory: `~/.config/sendra/config.yaml` on Linux,
`~/Library/Application Support/sendra/config.yaml` on macOS, and
`%APPDATA%\sendra\config.yaml` on Windows.

**How they combine.** Project over global, **key by key** — not file by file. A
project config that sets only `timeout_seconds` still inherits the global
config's `headers`, and one that overrides a single default header keeps the
rest. Anything neither file mentions falls back to the built-in defaults: no
extra headers, and a 30-second timeout. No config file anywhere is a perfectly
ordinary state, not a warning.

Config headers are defaults, so a request file always wins a conflict:

```yaml
# .sendra/config.yaml
headers:
  Authorization: Bearer dev-token
```

```yaml
# req.yaml — sent with Bearer other-token, plus any other config headers
method: GET
url: https://api.example.com/me
headers:
  Authorization: Bearer other-token
```

Names are compared case-insensitively, because that is how HTTP header names
work: a config `Authorization` and a request `authorization` are one header, and
the request's value is the one sent.

There are no CLI flags to override config yet — the file is the only input.

## Exit codes

- `0` — every request was sent and no response status was an error
  (1xx, 2xx, 3xx).
- `1` — some request never got a response: the file was missing or malformed, no
  request by that name, a header was invalid, or the request never completed
  (DNS, TLS, connection).
- `2` — bad command-line usage (from clap).
- `3` — every request completed but at least one server answered `4xx` or `5xx`.
  The responses print exactly as they would otherwise; only the exit code
  differs, so `sendra run req.yaml && deploy.sh` does not proceed on a 404.

For a collection, these are aggregates over the whole run: the worst outcome
wins, ranked `0` < `3` < `1`. One 4xx anywhere in the collection exits `3`, and
one request that could not be sent at all exits `1` — "never got a response" is
a bigger problem than "got a 500", so it takes precedence.

The alternative — letting the last request decide — would make the exit code
depend on the order the file happens to list requests in, so reordering a
collection could change whether a script proceeds. Worst-wins keeps exit `0`
meaning the same thing for a collection as for a single request: a promise that
nothing in the run failed.

```sh
sendra run examples/mixed-status-collection.yaml   # prints 200, 404, 500; exits 3
```

`3` is separate from `1` on purpose: "could not send" and "sent, got a 500" call
for different handling in a script. Pass `--allow-error-status` to opt out and
exit `0` on any status, for inspecting an error response without failing the
surrounding script:

```sh
sendra run examples/get-request.yaml --allow-error-status
```

Codes `4` and up are reserved for later commands (`sendra test` will need its
own outcome for failing assertions). The full table lives next to the `Exit`
enum in `sendra-cli/src/main.rs`.

## Development

```sh
cargo build --workspace --all-targets
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Those four are exactly what CI runs, on Linux, Windows and macOS, for every
push to `main` and every pull request against it — so a clean local run is a
green build. Clippy is `-D warnings`: a warning fails the build.

The test suite is hermetic. It parses YAML, checks exit-code logic, and resolves
config against directory trees built under a temporary directory rather than
against your real `~/.config`; the tests that name a URL point at a closed local
port so they fail before connecting. Nothing under `cargo test` touches the network, which is what makes
CI trustworthy rather than merely usually-green. The `examples/` files do hit
`httpbin.org`, and are run by hand — deliberately never in CI.
