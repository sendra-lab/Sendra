# CLI overrides

## Available overrides

Flags that change one invocation without touching a file, on `sendra run` and
`sendra test` alike:

```sh
sendra run requests/req.yaml -H "X-Trace-Id: abc123" --var base_url=http://localhost:8080 --timeout 5
```

| Flag                    | Repeatable | Overrides                                                        |
| ------------------------ | ---------- | ----------------------------------------------------------------- |
| `-H`/`--header "Name: value"` | yes  | config headers, the request's own `headers:`, a resolved `auth:` |
| `--var name=value`       | yes        | the active environment file's value for `name`                    |
| `--timeout <seconds>`    | no         | the resolved config `timeout_seconds`                             |
| `--insecure`             | no         | the resolved config `insecure` (can only turn it *on*)             |
| `--proxy <url>`          | no         | the resolved config `proxy`                                       |
| `--client-cert <path>`   | no         | the resolved config `client_cert.cert`                             |
| `--client-key <path>`    | no         | the resolved config `client_cert.key`                              |
| `--cookie-jar`           | no         | the resolved config `cookie_jar` (can only turn it *on*)            |

`-H` and `--var` are invocation-only: there is no config-file equivalent for
either. `--timeout`, `--insecure`, `--proxy`, `--client-cert`,
`--client-key` and `--cookie-jar` each *do* have one (`timeout_seconds`,
`insecure`, `proxy`, `client_cert.cert`, `client_cert.key`, `cookie_jar`;
see [Configuration](configuration.md)), and CLI wins over both config files for
all six, the same "most specific wins" rule every override here follows.
`--client-cert` and `--client-key` are resolved independently of each other,
so a `--client-cert` on the command line can pair with a `client_cert.key` a
config file set, and vice versa. Only Sendra ending up with just one half
of the pair, from any mix of sources, is refused. Nothing here changes how a config or
environment file itself resolves; see
[Precedence, start to finish](../decisions/precedence-chain.md) for how every one
of these flags fits with everything else.

## Header overrides

`-H`/`--header` wins every conflict a header can be in. It is compared
case-insensitively, like every other header rule in Sendra, and it *replaces*
rather than merges: a repeated header the request file wrote, a config
default, and the `Authorization` header a resolved `auth:` block produced are
all dropped in favour of the `-H` value if the name matches.

```sh
# req.yaml sets auth: bearer: original, so this invocation sends Bearer overridden instead
sendra run requests/req.yaml -H "Authorization: Bearer overridden"
```

Passing `-H` more than once for the *same* name keeps only the last value:

```sh
sendra run requests/req.yaml -H "X-Trace-Id: first" -H "X-Trace-Id: second"   # sends only "second"
```

Request files can send the same header more than once. On the command line,
repeating `-H` for the same header name replaces the earlier value; only the
last value is sent.

A malformed value (no `:` in `-H`, no `=` in `--var`) is refused immediately
with exit code `2`, the same way any other bad argument is, rather than
surfacing later as a confusing substitution or request-building failure.

## Variable overrides

`--var` behaves exactly like part of the environment file for this run. It
sets a variable whether or not `--env` was passed at all, and overrides the
same name in the file that *was* loaded when both are present:

```sh
sendra run requests/req.yaml --var base_url=http://localhost:8080          # no environment file needed
sendra run requests/req.yaml --env staging --var base_url=http://localhost # wins over staging.yaml's base_url
```

Sendra treats this value like one from the environment file. If a `capture`
block uses the same variable name, Sendra reports a name collision rather than
silently replacing either value; see [Name collisions](capturing.md#name-collisions).

```sh
sendra test requests/req.yaml --var token=cli-value   # requests/req.yaml also has `capture: { token: $.token }`
```

```text
capture
  ✗ token from `$.token` — the active environment already defines this
    variable; rename the capture or the `--var`/environment entry
```

Neither value silently takes priority. This keeps `{{name}}` from referring to
one value before a capture and a different value afterward.

## Timeout

`--timeout <seconds>` overrides the resolved config timeout for this
invocation, covering the whole request (connect, send and body read):

```sh
sendra run requests/req.yaml --timeout 5   # gives up after 5s, whatever config.yaml says
```

## TLS verification

`--insecure` overrides the resolved config `insecure`, but only upward.
There is no flag to turn verification back on if a config file sets
`insecure: true`. Change that config setting if you need verification enabled:

```sh
sendra run requests/req.yaml --insecure   # against a self-signed staging host, say
```

Whenever this resolves to `true` (from `--insecure` or from `insecure: true`
in either config file), Sendra prints a one-line warning to stderr before
sending anything, and keeps printing it even under `-q`/`--json`: see
[Configuration](configuration.md) for the full reasoning on why this one
notice is not narration `-q` trims away.

## Proxy

`--proxy <url>` overrides the resolved config `proxy` outright, taking
over proxying for the run entirely; see
[Configuration](configuration.md) for how that interacts with the standard
proxy environment variables:

```sh
sendra run requests/req.yaml --proxy http://proxy.example.com:8080
```

## Client certificates

`--client-cert <path>`/`--client-key <path>` override the resolved config
`client_cert.cert`/`client_cert.key`, each independently of the other; see
[Configuration](configuration.md) for the full `client_cert:` reasoning,
including why PEM is the only format accepted and why the two orthogonal
`insecure`/`client_cert` settings can both apply to the same run. Resolved
**relative to the current working directory**, unlike the config-file form,
which resolves relative to the config file's own directory:

```sh
sendra run requests/req.yaml --client-cert ./client.pem --client-key ./client-key.pem
```

If a certificate or key file is missing or invalid, the error includes its
path to help you locate the problem.

## Cookie jar

`--cookie-jar` overrides the resolved config `cookie_jar`, but only upward,
the same one-directional shape as `--insecure`, with no
`--no-cookie-jar` to force it back off over a config that set `cookie_jar:
true`:

```sh
sendra test login-flow.yaml --cookie-jar
```

See [Configuration](configuration.md) for the full reasoning, including why
this defaults to off, how it interacts with a request's own `Cookie`
header, and why it is worth preferring over manual `Set-Cookie` capture for
cookie-shaped values. Under `--repeat`, each pass gets its own empty jar:
the same "each repeat is a clean run" rule already covers
[captured variables](capturing.md).

For how all of these fit together with config files, environment files and
captured variables, see
[Precedence, start to finish](../decisions/precedence-chain.md).
