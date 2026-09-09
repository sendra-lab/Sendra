# Configuration

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
follow_redirects: true # true (default, 10 hops), false, or a custom hop count
insecure: false # true disables TLS certificate verification — see below
proxy: http://proxy.example.com:8080 # route every request through this proxy
client_cert: # present a client certificate for mutual TLS — see below
  cert: ./client.pem
  key: ./client-key.pem
cookie_jar: false # true stores and resends cookies automatically — see below
```

The schema stays small on purpose — the fields above are the whole of it
today. Unknown keys are rejected, like everywhere else in Sendra, so `timeout`
instead of `timeout_seconds` is an error you see rather than a setting that
quietly never applies.

**`insecure: true` disables TLS certificate verification for every request
this run sends.** It exists for a self-signed or otherwise untrusted
endpoint — an internal staging host, say — where there is no CA chain to
verify against, not for routine use against the public internet: it removes
the one thing standing between a request and a man-in-the-middle. Whenever
this resolves to `true`, from either the config file or `--insecure`, Sendra
prints a one-line warning to stderr before sending anything, and `-q` does
not suppress it — see [CLI overrides](cli-overrides.md) for `--insecure`
itself and the reasoning behind that.

**`proxy: <url>` routes every request through an HTTP proxy**, `http://user:pass@host:port`
included — credentials in the URL are read by the underlying HTTP client,
nothing Sendra parses itself. Setting it, from either the config file or
`--proxy`, takes over proxying for the run entirely: the standard
`HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` environment variables Sendra otherwise
respects by default (matching curl and most other HTTP tooling) are not
consulted once an explicit proxy is configured. No `proxy:` key and no
`--proxy` is the plain "follow the environment" default.

**`client_cert: {cert, key}` presents a client certificate for mutual
TLS** — the case where the *server* wants proof of who the *client* is, not
just the other way around. Both `cert` and `key` are required together: a
config naming only one is a parse error, and a CLI override supplying only
`--client-cert` or only `--client-key` against a config that supplies neither
is refused when the client is built. PEM only, not PKCS#12/`.pfx`: Sendra's
HTTP client is built against `rustls` alone, and accepting a PKCS#12 file
would mean shipping a second TLS backend just for that one format — `openssl
pkcs12 -export ... ` (or an existing `.pfx`) can always be split back into a
`cert.pem`/`key.pem` pair with `openssl pkcs12 -in bundle.pfx -clcerts
-nokeys -out client.pem` / `-nocerts -nodes -out client-key.pem`.

`cert`/`key` resolve **relative to this config file's own directory**, not
the current working directory — the same rule a request's `body_file:` uses
for the file that names it. A project config checked into version control
should mean the same certificate on every machine it runs on, regardless of
which directory the command happened to be typed from. `--client-cert`/
`--client-key`, covered in [CLI overrides](cli-overrides.md), resolve relative
to the working directory instead, matching every other CLI-supplied path.

Orthogonal to `--insecure`: a client certificate is about proving who *this
client* is to the server, while `--insecure` is about whether *the server's*
certificate gets checked. Nothing stops the two from applying to the same
run — a self-signed internal endpoint that also demands a client
certificate needs both.

**`cookie_jar: true` stores cookies received via `Set-Cookie` and sends them
back automatically on later requests to the same host.** It exists for
login-flow-style APIs that rely on a session cookie rather than a bearer
token. **Off by default, deliberately**: this matches curl, which does not
carry cookies between requests unless you pass `-c`/`-b` yourself, and Sendra
follows the same convention rather than defaulting to "on" because it would
be convenient for the case above. See [CLI overrides](cli-overrides.md) for
`--cookie-jar` itself.

In-memory only, for the duration of one invocation — nothing is written to
disk, and nothing survives between separate `sendra run`/`sendra test`
invocations, the same rule [captured values](capturing.md)
already follow. A request whose own `headers:` sets `Cookie` is left alone:
the underlying HTTP client only fills in the jar's `Cookie` header when the
request does not already carry one, so an explicit header always wins
outright rather than being merged with whatever the jar holds.

**Prefer this over manually capturing `Set-Cookie` for cookie-shaped values
specifically.** [Capturing values](capturing.md)
can only see the *final* response's headers once redirects have been
followed, so a `Set-Cookie` set on an intermediate hop of a redirect chain is
invisible to it. `cookie_jar` has no such limit: cookie handling sits
underneath redirect-following, so a cookie set on any hop — not just the
final response — is captured and resent. For a login flow that redirects
through an intermediate hop before setting its session cookie, `cookie_jar`
is the one of the two that actually works.

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

`--timeout`, `-H`/`--header`, `--insecure`, `--proxy`,
`--client-cert`/`--client-key` and `--cookie-jar` all override this file for
one invocation without editing it — see
[CLI overrides](cli-overrides.md), and
[Precedence, start to finish](../decisions/precedence-chain.md) for where they sit
relative to everything else.
