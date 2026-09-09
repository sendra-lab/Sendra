# Reference

The full schema and behavior reference for Sendra, split by topic. Start at
[README.md](../README.md) for a five-minute tour if you haven't sent a
request with Sendra yet — this is the lookup material for once you have.

- [Request and collection file shape](reference/requests.md) — the fields a
  request or collection file can have, repeated headers, and authentication
  (`auth: bearer`/`basic`/`api_key`).
- [Running and testing requests](reference/running-and-testing.md) —
  `sendra run`, `sendra test`, and how a `test` run's summary counts requests.
- [JSON output](reference/json-output.md) — the `--json` document both
  subcommands can print, field by field.
- [Configuration](reference/configuration.md) — `.sendra/config.yaml`:
  headers, timeout, redirects, TLS, proxy, cookie jar.
- [Environments and variables](reference/environments.md) — `.sendra/environments/*.yaml`
  and `{{variable}}`/`${OS_VAR}` substitution.
- [Assertions](reference/assertions.md) — the `assertions:` block, including
  the richer operators (`greater_than`, `matches`, `not:`, and so on).
- [Capturing values and chaining requests](reference/capturing.md) — the
  `capture:` block, and how a value moves from one request's response into a
  later request's substitution.
- [CLI overrides](reference/cli-overrides.md) — every flag that changes one
  invocation without editing a file: `-H`, `--var`, `--timeout`, `--insecure`,
  `--proxy`, `--client-cert`/`--client-key`, `--cookie-jar`.
- [Scripting](reference/scripting.md) — `pre_request`/`post_request`: what
  each can see, in what order, compiled before anything is sent.
- [Exit codes](reference/exit-codes.md) — the one table for the whole binary.
- [JSON Schema / editor support](reference/json-schema.md) — pointing an
  editor at `schema/*.schema.json` for autocomplete and inline validation.
- [Development](reference/development.md) — building, testing and linting
  this repository.

For the reasoning behind a design decision rather than the shape of it, see
[docs/decisions/](decisions/README.md).
