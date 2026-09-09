# Sendra

[![CI](https://github.com/sendra-lab/sendra/actions/workflows/ci.yml/badge.svg)](https://github.com/sendra-lab/sendra/actions/workflows/ci.yml)

Sendra is a terminal-native HTTP client, think Postman, but your requests are
plain YAML files that live in your repo next to the code they exercise, and you
send them from the shell. A request is just a file: method, URL, headers, body.
That makes requests reviewable in a pull request, diffable over time, and
shareable without exporting anything. A file holds either one request or a
named collection of them, sent and printed, against variables from an
environment file so the same request can point at staging or at production, and
a file can declare what it expects the response to look like — which
`sendra test` then passes or fails your build on. A request can also carry
inline scripts that run just before it is sent and just after it comes back,
with no Node.js or other runtime to install: the interpreter is in the binary.
An interactive TUI is planned and deliberately absent for now.

## Layout

```
sendra/
  sendra-core/     library: request/response types, YAML loading, config, environments, scripting, capture, HTTP execution
  sendra-cli/      binary `sendra`: argument parsing, output, exit codes, `run` and `test`
    main.rs          `main()`, and the module declarations
    cli.rs           the clap definitions: subcommands, arguments, `--help` text
    run.rs           the pipeline both subcommands share, and the two handlers
    output/          everything printed to the terminal
      mod.rs           `Reporter`, `Format`, `Detail`: which rendering a run gets
      human.rs         the terminal rendering: response, assertions, summary
      json.rs          the `--json` schema: the records the document is built from
      errors.rs        `error:` and `hint:` lines, and the one clap usage error
    exit.rs          `Exit`, `Outcome`, `Summary`: exit-code policy, no I/O
    test_support.rs  fixtures shared by more than one module's tests
    tests/           integration tests that run the built binary and read its output
  examples/        sample request and collection files
  .sendra/         this repo's own project config and environments
  schema/          generated JSON Schemas for editor tooling — see docs/reference/json-schema.md
  xtask/           generates schema/*.schema.json from sendra-core's types; not published
  docs/
    reference.md     full schema and behavior reference, by topic
    decisions/       the design rationale behind choices that could have gone another way
```

`sendra-core` knows nothing about clap or terminal output. A `sendra-tui` crate
will sit alongside `sendra-cli` later and reuse `sendra-core` directly, so core
returns typed errors (`SendraError`) rather than formatted messages.

## Install

No packaged release yet — Sendra is pre-v1 and there is no binary to download.
For now, build it from source:

```sh
git clone https://github.com/sendra-lab/Sendra.git
cd sendra
cargo build --workspace --release
./target/release/sendra run examples/get-request.yaml
```

Or run it straight through Cargo without a separate build step, which is what
the rest of this tour does:

```sh
cargo run -p sendra-cli -- run examples/get-request.yaml
```

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

`examples/scripted-request.yaml` carries both hooks: a `pre_request` script that
adds a header, and a `post_request` script that checks the response it comes
back in.

```sh
cargo run -p sendra-cli -- run examples/scripted-request.yaml
```

`examples/capture-chain.yaml` is two requests where the second needs something
only the first can tell it: a token and an id captured from one response,
substituted into the next request's header and URL.

```sh
cargo run -p sendra-cli -- run examples/capture-chain.yaml
```

`examples/capture-header-status.yaml` extends that into three requests to show
the two other `capture:` sources — a response header and the status code —
chained alongside the original JSON-path form.

```sh
cargo run -p sendra-cli -- run examples/capture-header-status.yaml
```

`examples/environment-request.yaml` uses variables instead of literals, and
needs a secret in your shell to run:

```sh
API_KEY=live-token cargo run -p sendra-cli -- run examples/environment-request.yaml
```

It reads `base_url` and `api_key` from `.sendra/environments/default.yaml` in
this repository and sends them to `httpbin.org/headers`, which echoes back what
it received, so you can see the resolved values on the wire. Leave `API_KEY`
unset and the run fails before connecting, naming the variable.

`--env` picks a different environment for the same request file. This
repository ships `staging.yaml` and `prod.yaml` beside `default.yaml`, pointing
at two different echo services:

```sh
API_KEY=live-token cargo run -p sendra-cli -- run examples/environment-request.yaml --env staging
API_KEY=live-token cargo run -p sendra-cli -- run examples/environment-request.yaml --env prod
```

The request file names no host at all — only `{{base_url}}` — so the two runs
come back from `httpbin.org` and `postman-echo.com` respectively, with the
resolved value echoed in the `X-Sendra-Base-Url` header of each response.

`examples/assertions.yaml` checks the response it gets back, and prints a
pass/fail line per check under it:

```sh
cargo run -p sendra-cli -- run examples/assertions.yaml
```

Two of its assertions are meant to fail, so one run shows both halves of the
output. It still exits `0` — see
[Assertions](docs/reference/assertions.md).

`examples/test-collection.yaml` is the same idea under `sendra test`, which
does not exit `0`:

```sh
cargo run -p sendra-cli -- test examples/test-collection.yaml
```

Four requests: two that pass, one whose assertion is wrong on purpose, and one
that asserts nothing and comes back `404`. It exits `4` — see
[Testing](docs/reference/running-and-testing.md).

`examples/repeated-headers.yaml` sends a header more than once — a list of
values instead of a scalar, since a YAML mapping cannot repeat a key — beside
an ordinary one, against httpbin.org/headers, which echoes both back:

```sh
cargo run -p sendra-cli -- run examples/repeated-headers.yaml
```

`examples/structured-bodies.yaml` is a collection of four requests, one for
each structured way to specify a body — `json`, `body_file`, `form` and
`multipart` — instead of a hand-escaped `body:` string. Each posts to
httpbin.org/post, which echoes back exactly what it received:

```sh
cargo run -p sendra-cli -- run examples/structured-bodies.yaml               # all four
cargo run -p sendra-cli -- run examples/structured-bodies.yaml "JSON body"   # just one
```

That's the shortest path from zero to a working request. The rest of what
Sendra can do — the full request/collection/config schema, every CLI flag,
`--json`'s exact shape, scripting, and the reasoning behind the harder design
calls — lives in `docs/`, not in this file:

## Learn more

- **[docs/reference.md](docs/reference.md)** — the full schema and behavior
  reference: request/collection shape, config, environments, assertions
  (including the operator sub-language), capture, scripting, every CLI flag,
  `--json` output, exit codes, and editor/JSON Schema support.
- **[docs/decisions/](docs/decisions/README.md)** — the design rationale
  behind choices that could reasonably have gone another way: why `sendra
  test` ignores a status nobody asserted, how the exit codes are split and
  ranked, the script sandboxing guarantees, and the full precedence chain
  from a hardcoded default to a CLI override.

Both grew out of the same source material as this file — nothing was
shortened or dropped, only moved to where it's easier to find once you
already know your way around.
