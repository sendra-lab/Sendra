# Sendra

[![CI](https://github.com/dubemoyibe-star/Sendra/actions/workflows/ci.yml/badge.svg)](https://github.com/dubemoyibe-star/Sendra/actions/workflows/ci.yml)

Sendra is a terminal-native HTTP client, think Postman, but your requests are
plain YAML files that live in your repo next to the code they exercise, and you
send them from the shell. A request is just a file: method, URL, headers, body.
That makes requests reviewable in a pull request, diffable over time, and
shareable without exporting anything. A file holds either one request or a
named collection of them, sent and printed, against variables from an
environment file so the same request can point at staging or at production, and
a file can declare what it expects the response to look like — which
`sendra test` then passes or fails your build on. Scripting and an interactive
TUI are both planned and deliberately absent for now.

## Layout

```
sendra/
  sendra-core/     library: request/response types, YAML loading, config, environments, HTTP execution
  sendra-cli/      binary `sendra`: argument parsing, output, exit codes, `run` and `test`
  examples/        sample request and collection files
  .sendra/         this repo's own project config and environments
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
output. It still exits `0` — see [Assertions](#assertions).

`examples/test-collection.yaml` is the same idea under `sendra test`, which
does not exit `0`:

```sh
cargo run -p sendra-cli -- test examples/test-collection.yaml
```

Four requests: two that pass, one whose assertion is wrong on purpose, and one
that asserts nothing and comes back `404`. It exits `4` — see
[Testing](#testing).

## Request file shape

```yaml
name: Get user # optional, used as a display label
method: GET # GET | POST | PUT | PATCH | DELETE | HEAD | OPTIONS
url: https://api.example.com/users/1
headers: # optional
  Accept: application/json
body: null # optional, sent verbatim as a raw string
assertions: # optional, checked against the response — see below
  status: 200
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

`sendra test` sends the same requests the same way and answers a different
question about them; see [Testing](#testing).

## Testing

`sendra run` reports what came back. `sendra test` reports whether it was what
the file said it should be, and puts that answer in the exit code:

```sh
sendra test req.yaml                 # the one request in the file
sendra test collection.yaml          # every request in it, in file order
sendra test collection.yaml --env ci # against .sendra/environments/ci.yaml
```

Everything about *sending* is the same as `run`: the same file shapes, the same
config, the same `--env` and `{{variable}}` substitution, the same sequential
order, and the same rule that one broken request does not stop the ones after
it. The same assertion results print under each response, in the same format.
Three things differ:

- Responses print as a status line only — no headers, no body. `test` answers a
  question about a whole collection, and burying that answer under four JSON
  bodies would make the summary the hardest line to find in its own output. Use
  `sendra run` when you want to look at a response.
- A summary of the whole run prints at the end.
- The exit code comes from the assertions.

`examples/test-collection.yaml`, run against httpbin, prints exactly this:

```text
→ Status and body
200 OK  1657 ms

assertions
  ✓ status is 200
  ✓ header `content-type` is `application/json`
  ✓ body contains `"url"`
  ✓ `$.url` is "https://httpbin.org/get"
  4 passed

→ Wrong expectation
200 OK  1390 ms

assertions
  ✓ status is 200
  ✓ `$.json.project` is "sendra"
  ✗ `$.json.stage` is "collections" — got "test"
  2 passed, 1 failed

→ Unasserted 404
404 Not Found  1598 ms

no assertions

→ Expected 500
500 Internal Server Error  1422 ms

assertions
  ✓ status is 500
  1 passed

summary
  4 requests: 2 passed, 1 failed, 1 without assertions
```

**Four categories, and they do not overlap.** Every request lands in exactly
one, so the counts always add up to the total:

| Category             | Meaning                                                   |
| -------------------- | --------------------------------------------------------- |
| `passed`             | Got a response, declared assertions, and all of them held. |
| `failed`             | Got a response, declared assertions, one or more did not.  |
| `without assertions` | Got a response and declared nothing to check.              |
| `no response`        | Never got a response, so there was nothing to check against. |

The last three are printed only when they are not zero, so a clean run reads
`4 requests: 4 passed` and nothing competes with it. A request that declared
nothing prints a dimmed `no assertions` where its results would have gone, so
the `without assertions` count has something to point at.

**A request with no assertions is not a pass.** It is a request nobody said
anything about, and it gets its own count for that reason. Folding it into
`passed` would let a collection with no assertions anywhere report a perfect
green run, which is the most misleading thing a test command can do; folding it
into `failed` would break the build every time somebody added a request before
writing expectations for it. The count is the honest answer — "these ran, and
nothing was checked" — and what to do about it is yours.

**A status nobody asserted does not fail a test run.** `Unasserted 404` above
comes back `404` and the run still exits `4` because of the *assertion* that
failed, not because of it. This is the debatable one, so, plainly: `test`'s
contract is that the file says what it expects and `test` reports whether it got
it. Failing on a bare `404` means asserting something the file never wrote down
— inventing an expectation on the author's behalf — which is the same class of
mistake as an assertion silently ignored because of a typo, only inverted.
Sendra refuses to guess everywhere else in its schema, and the check is one line
to write when you want it:

```yaml
assertions:
  status: 200
```

It also keeps a real use intact: a request that is in the collection to *reach*
an endpoint — a login, a setup call — rather than to be checked. And the
raw-status question already has a command that answers it, and answers it well:
`sendra run`, exit `3`. Nothing is lost by `test` declining to answer it a
second time with a different number. The safeguard against the decision hiding a
problem is the summary itself: `without assertions` is printed, so a run whose
expectations were never written is visibly not the same thing as a run that
passed.

**`--allow-error-status` does not apply to `sendra test`,** and passing it is an
error rather than a no-op:

```text
error: `--allow-error-status` does not apply to `sendra test`.

  `test` decides its exit code from assertions, not from response statuses: a
  4xx or 5xx that no assertion mentions does not fail a test run in the first
  place, so there is nothing here for the flag to forgive.
```

There is nothing for it to suppress, and a flag accepted and quietly discarded
reads, to whoever typed it, exactly like one that worked.

**No request-name argument.** `sendra run <file> <name>` exists to send one
request out of a collection and look at it. `test` produces a verdict over a
file, and a verdict over one hand-picked request is a different, narrower thing;
it can be added later if it turns out to be wanted.

See [Exit codes](#exit-codes) for `4` and how it ranks against `1`.

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

## Environments and variables

An environment is a flat file of variables at
`.sendra/environments/<name>.yaml`, found by the same upward walk as
`.sendra/config.yaml`:

```yaml
# .sendra/environments/staging.yaml
base_url: https://staging.api.example.com
api_key: ${API_KEY} # read from your shell, never written down here
```

Requests reference them with `{{name}}`, in the `url`, in header names and
values, and in the `body`:

```yaml
method: POST
url: '{{base_url}}/users'
headers:
  Authorization: 'Bearer {{api_key}}'
body: '{"tenant": "{{tenant}}"}'
```

Point the same file at production by changing which environment is loaded, and
nothing in the request file moves.

**Quote a value that starts with `{{`.** In YAML a bare `{` opens a flow
mapping, so `url: {{base_url}}/users` is a syntax error before Sendra sees it.
`url: '{{base_url}}/users'` is fine. A `{{...}}` in the middle of a value —
`url: https://x/{{id}}` — needs no quotes.

**Keeping secrets out of git.** A value written as `${VAR}` is read from your OS
environment at send time, so the file names the secret without containing it and
can be committed like any other request file. Sendra never reads a `.env` file:
exporting the variable is the whole mechanism, which means it works the same in
a shell, in CI, and under any secret manager that can export one.

**Nothing resolves to an empty string.** A `{{var}}` with no such variable, or a
`${VAR}` that is not exported, is an error naming what is missing, raised while
that request is being built — so none of its bytes go out:

```
error: no variable named `base_url` in `.sendra/environments/default.yaml` (available: api_key, host)
error: environment variable `API_KEY` is not set (referenced by `api_key` in `.sendra/environments/default.yaml`)
```

The alternative, sending `Authorization: Bearer ` and letting the server answer
`401`, turns a one-line fix into a debugging session.

**In a collection, one broken request fails alone.** Substitution happens as
each request is reached, not as a check over the whole file first, so a missing
variable is treated exactly like a refused connection: that request is reported
as a failure, the requests around it are still sent, every result still prints,
and the exit code is the worst of them.

```
→ First
200 OK  412 ms
...
→ Broken
error: no variable named `nope` in `.sendra/environments/default.yaml` (available: api_key, base_url)

→ Third
200 OK  388 ms
...
```

`--allow-error-status` does not suppress this. That flag forgives a *status*,
and a request that could not be built has no status — like a DNS or connection
failure, it exits `1` either way.

**Which environment is loaded.** `--env <name>`, on `sendra run` and on
`sendra test` alike:

```sh
sendra run req.yaml --env staging   # .sendra/environments/staging.yaml
sendra run req.yaml --env prod      # .sendra/environments/prod.yaml
sendra run req.yaml                 # .sendra/environments/default.yaml, if there is one
sendra test req.yaml --env ci       # same rule, same walk-up, same errors
```

The name is a filename, not a keyword — `staging`, `prod`, `local`, `ci` and
`default` are all just files in `.sendra/environments/`, found by the same
upward walk, nearest one wins.

Two rules about environments that are not there, and they are deliberately
different from each other:

- **No `--env` and no `default.yaml` is fine.** You get the empty environment,
  and a request with no `{{...}}` in it behaves exactly as it did before
  environments existed. Most projects have no `.sendra/` at all, and requiring
  a flag to run a file with no variables in it would be absurd.
- **`--env <name>` with no such file is an error, and nothing is sent.**

  ```
  error: no environment named `stagng`: no `.sendra/environments/stagng.yaml` in `/repo` or any parent directory
  ```

  Under `sendra test` this is exit `1`, not `4`: nothing was sent, so no
  assertion was evaluated, and the run cannot have failed on its expectations.

  The difference is not the file, it is what you asked for. Omitting `--env`
  asks for a default; `--env staging` asserts that `staging` exists. Sendra
  already answers a failed assertion of that shape loudly —
  `sendra run collection.yaml Nope` is an error listing the names that do
  exist, while omitting the name runs everything — and this is the same
  pattern. The alternative fails in the two ways that matter: with `{{var}}` in
  the file you get an error naming the *variable*, sending you to hunt for a
  typo in your request file when the typo is on your command line; with no
  variables in the file you get no error at all, exit `0`, and a flag that was
  silently ignored.

**What substitution touches, and what it does not.** Only `url`, `headers`,
`body` and the values inside `assertions`. Not `method`, which is a closed set
with no useful placeholder, and not `name`, which is what
`sendra run <file> <name>` selects on — a label that changed with the
environment could not be typed on the command line. Inside `assertions`, the
keys that select part of the response are excluded too; see
[Assertions](#assertions).

Substitution runs on the parsed request, over string values only, rather than as
a find-and-replace on the file text before parsing. A value is therefore only
ever a value: a token containing `:`, a multi-line key, a body starting with `-`
cannot change the shape of the document they land in. That is also why the
leading-`{{` quoting rule above exists, and it is the one thing a text-level
pass would have made easier.

Substitution happens **before** config headers are applied, so the request that
`Config::apply` merges into is the one that will actually be sent, and a
templated header name is matched against config by its resolved name. The
consequence: **config headers are not templated.** A `{{var}}` in
`.sendra/config.yaml` is sent verbatim. A config applies to every project
directory beneath it and is resolved without reference to any environment, so
templating it is a decision to take on its own rather than to inherit from this
one.

Two further rules, both deliberate:

- **No layering.** Environments are flat files; there is no "staging extends
  base". A nested mapping in an environment file is a parse error rather than
  something half-supported.
- **One pass, no recursion.** A resolved value is copied in verbatim and never
  re-scanned, so a value that itself contains `{{...}}` is data, not a further
  reference.

Values are strings, and an unquoted scalar substitutes as exactly the text you
wrote: `port: 8080` is `8080`, `version: 1.0` is `1.0`. Nothing takes a round
trip through a number on the way in, so `1.0` can never arrive as `1`.

## Assertions

A request can say what it expects of the response, under an optional
`assertions` key:

```yaml
method: GET
url: https://httpbin.org/get
assertions:
  status: 200 # the exact status code
  headers:
    content-type: application/json # present, and exactly this value
    x-request-id: # present, value not checked
  body_contains: '"url"' # a case-sensitive substring of the body
  json: # JSON path -> the single value it must select
    $.url: https://httpbin.org/get
    $.headers.Accept: application/json
```

Every key is optional, and every *entry* is one assertion — the block above is
six of them. All six are checked and all six are reported: a failing assertion
never hides the ones after it. Unknown keys are rejected, like everywhere else
in Sendra's schema, because an assertion that is silently ignored because of a
typo reads exactly like one that is passing.

Results print under the response they are about, so in a collection run each
block sits with its own request:

```text
assertions
  ✓ status is 200
  ✓ header `content-type` is `application/json`
  ✗ header `x-request-id` is present — not present (the response has: date, content-type, server)
  ✓ body contains `"url"`
  ✓ `$.url` is "https://httpbin.org/get"
  ✗ `$.origin` is "203.0.113.1" — got "104.28.220.44"
  4 passed, 2 failed
```

A request with no `assertions` block prints exactly what it printed before this
feature existed — an empty report produces no output at all.

**Assertions do not affect `sendra run`'s exit code.** The run above exits `0`:
six assertions, two of them failed, exit `0`. That is deliberate and permanent.
`sendra run` sends requests and reports what came back; `sendra test` is the
command whose job is to pass or fail on expectations. Doing it in `run` would
silently change what every existing `sendra run req.yaml && deploy.sh` means the
moment someone adds an `assertions` block to `req.yaml`.

The same file under `sendra test` exits `4`, and that is the only difference
between the two commands worth remembering. See [Testing](#testing).

**Header names are matched case-insensitively, values exactly.** HTTP header
names are case-insensitive, so which casing a server picks is not something a
request file should have to know. Values are compared whole:
`content-type: application/json` does **not** match
`application/json; charset=utf-8`. A substring match would quietly accept
`application/json-seq` too, so when a server decorates a value, assert the whole
value or drop to presence-only (`content-type:` with nothing after it). A
repeated header — `set-cookie` — passes if any of its values matches.

**A JSON path must select exactly one value.** `$.users[*].id` against three
users is a question with no single answer; it fails, saying how many it matched,
rather than silently comparing against the first. Paths are RFC 9535 JSON path,
evaluated by [`jsonpath-rust`](https://docs.rs/jsonpath-rust). The expected
value is written as ordinary YAML and compared as JSON, so `42` is a number,
`'42'` is a string, and a mapping or sequence compares whole.

**A body that is not JSON fails every JSON assertion, and nothing else.** It is
a failed assertion with the parser's own message, not a crash and not a
load-time error — whether the body parses is a property of a response that does
not exist until the request has been sent:

```text
  ✗ `$.user.id` is 42 — the response body is not JSON: expected value at line 1 column 1 (content-type: text/html; charset=utf-8)
```

The `content-type` is reported to explain the failure, never to decide whether
to try: a JSON body served as `text/plain` is still a JSON body, and refusing to
look at it would fail an assertion that is plainly true. A JSON path that does
not parse is reported the same way, and reported first — it is wrong about every
response there could ever be, so it is the one you have to go and fix.

**Assertion values are substituted; assertion keys are not.** `{{var}}` works in
a header's expected value, in `body_contains`, and in the strings of an expected
JSON value, so an assertion can move between environments with the request it
belongs to. Header names, JSON paths and the keys inside an expected object stay
literal: an environment is meant to change what a response is compared against —
a tenant, an id, a host — not which part of the response is being looked at. A
missing variable in an assertion fails that request before it is sent, exactly
like a missing variable in its URL.

## Exit codes

One table for the whole binary, not one per subcommand: `run` and `test` answer
different questions, but they answer them to the same shell, and a number that
means one thing under one command and something else under the other is a trap
for anyone writing `case $? in` around either.

| Code | `run` | `test` | Meaning                                                             |
| ---- | :---: | :----: | ------------------------------------------------------------------- |
| `0`  |   ·   |   ·    | Nothing went wrong — see below for what each command means by that. |
| `1`  |   ·   |   ·    | Some request never got a response.                                  |
| `2`  |   ·   |   ·    | Bad command-line usage (from clap).                                 |
| `3`  |   ·   |        | `run` only: every request got a response, at least one was 4xx/5xx. |
| `4`  |       |   ·    | `test` only: every request got a response, at least one had a failing assertion. |

- `0` — for `run`, every request was sent and no response status was an error
  (1xx, 2xx, 3xx). For `test`, every request got a response and every assertion
  that was declared, passed.
- `1` — some request never got a response: the file was missing or malformed, no
  request by that name, `--env` named an environment with no file behind it, a
  `{{variable}}` or `${VAR}` had no value, a header was invalid, or the request
  never completed (DNS, TLS, connection). The same meaning under both commands,
  which is why it is the same number.
- `2` — bad command-line usage (from clap). `sendra test --allow-error-status`
  is one of these; see [Testing](#testing).
- `3` — `sendra run` only: every request completed but at least one server
  answered `4xx` or `5xx`. The responses print exactly as they would otherwise;
  only the exit code differs, so `sendra run req.yaml && deploy.sh` does not
  proceed on a 404.
- `4` — `sendra test` only: every request got a response, but at least one had
  an assertion that did not hold.

Codes `5` and up are free.

**Why `4` and not `3` for a failing assertion.** Reusing `3` would have made one
number mean "the server said 500" under `run` and "the server said exactly what
you asked for, and it was wrong" under `test`. They are different events and
they want different handling.

**Why a `test` run with an unsendable request exits `1` and not `4`.** Both are
failures and both are non-zero, but they are not the same failure: `4` means the
API did not meet the expectations, `1` means Sendra could not get far enough to
find out. In CI one says "fix your API" and the other says "fix your test
setup", and a single generic non-zero would have thrown that away. When a run
contains both, `1` wins — see the ranking below.

For a collection, these are aggregates over the whole run: the worst outcome
wins, ranked `0` < `3` < `4` < `1`. One 4xx anywhere in a `run` exits `3`, one
failing assertion anywhere in a `test` exits `4`, and one request that could not
be sent at all exits `1` — "never got a response" is a bigger problem than "got
a 500" or "got the wrong body", so it takes precedence. (`3` and `4` never meet:
one is only ever produced by `run` and the other only by `test`.)

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

A failed assertion never enters `run`'s answer: `sendra run` prints assertion
results and does not read them when deciding what to return, so a run that
reports "2 failed" still exits `0`. That is permanent, not a stage on the way to
unifying the two commands — wiring assertions into `run`'s exit code would
silently change what every existing `sendra run req.yaml && deploy.sh` means the
moment an `assertions` block is added to `req.yaml`, and `sendra test` exists so
that nobody has to. See [Assertions](#assertions) for the whole argument, and
`exit_for_response` in `sendra-cli/src/main.rs` for the single place that
decision lives.

The table above lives next to the `Exit` enum in `sendra-cli/src/main.rs`, and
`Summary` beside it is where `test`'s half of it is decided.

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
config and environments against directory trees built under a temporary
directory rather than against your real `~/.config`; the tests that name a URL
point at a closed local port so they fail before connecting. Nothing under
`cargo test` touches the network, which is what makes CI trustworthy rather than
merely usually-green. The `examples/` files do hit `httpbin.org`, and are run by
hand — deliberately never in CI.

No test calls `std::env::set_var` either. It is process-global, so one test
setting a variable is visible to every test running beside it; the `${VAR}` path
is tested by passing a stand-in OS environment to `Environment` instead, the
same way config resolution takes its directories as arguments. The tests that do
read the real environment only read it, and only for a name nothing could have
set.
