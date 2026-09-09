# Environments and variables

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

`--var name=value` sets one variable for this invocation, with or without an
environment file at all — see [CLI overrides](cli-overrides.md).

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
[Assertions](assertions.md).

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

## A default `auth:` for the whole environment

`auth` is one reserved top-level key — every other key is still an ordinary
variable. It carries a default [authentication](requests.md#authentication)
block, in the exact same `bearer`/`basic`/`api_key`/`oauth` shape a request's
own `auth:` uses, applied to every request run against this environment that
sets no `auth:` of its own:

```yaml
# .sendra/environments/staging.yaml
base_url: https://staging.api.example.com
auth:
  bearer: ${API_TOKEN}
```

```yaml
# req.yaml — no auth: of its own, so staging.yaml's applies
method: GET
url: '{{base_url}}/me'
```

**A request's own `auth:` fully replaces the environment's — never merged.**
A request that wants different credentials writes its own `auth:` block, in
full, the same "one thing owns this setting" stance a request's `auth:`
already takes against an explicit `Authorization` header. Setting `auth:` on
a request is therefore how to opt out of an environment's default entirely,
not how to add to it.

`{{var}}` inside the environment's own `auth:` resolves against that same
environment's variables, exactly like `base_url` above does — so
`${API_TOKEN}` in the example is read from the OS environment the same way
any other `${VAR}` reference is.

A request with no `auth:` block but an explicit header or query parameter of
the name the environment's default would itself set (`Authorization` for
`bearer`/`basic`, or an `api_key`'s own `name`) is rejected the same way an
explicit `auth:` conflicting with a hand-written header already is: two
things claiming ownership of one setting.

An environment-level `auth.oauth` shares the same in-run token cache as a
request-level one — see [Authentication](requests.md#authentication) — so
every request in the run that falls back to it acquires one token between
them, exactly as if they had all written the same `oauth:` block themselves.

There is no equivalent at the config-file level (`.sendra/config.yaml`):
config is deliberately environment-agnostic tool-wide settings (issue 3's
original design), while an auth scheme is inherently tied to which
environment it authenticates against — the same reasoning that puts
`base_url` in an environment file rather than in config.
