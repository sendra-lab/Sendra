# Sendra

Sendra is a terminal-native HTTP client, think Postman, but your requests are
plain YAML files that live in your repo next to the code they exercise, and you
send them from the shell. A request is just a file: method, URL, headers, body.
That makes requests reviewable in a pull request, diffable over time, and
shareable without exporting anything. This is the first slice: one request per
file, sent and printed. Environments, variables, scripting, assertions and an
interactive TUI are all planned and deliberately absent for now.

## Layout

```
sendra/
  sendra-core/     library: request/response types, YAML loading, HTTP execution
  sendra-cli/      binary `sendra`: argument parsing, output, exit codes
  examples/        sample request files
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
body.

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

## Exit codes

- `0` — the request was sent and a response came back. Note that an HTTP error
  status (404, 500) is still `0`: the request itself succeeded. Assertions, in a
  later version, are what will make a bad status a failing run.
- `1` — the file was missing or malformed, a header was invalid, or the request
  never completed (DNS, TLS, connection).
- `2` — bad command-line usage (from clap).

## Development

```sh
cargo build --workspace
cargo test --workspace
```
