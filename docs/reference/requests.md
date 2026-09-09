# Request and collection file shape

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
pre_request: | # optional, runs just before the request is sent — see below
  request.headers["X-Request-Id"] = "abc-123";
post_request: | # optional, runs against the response
  if response.status != 200 { throw "expected 200, got " + response.status; }
capture: # optional, values handed to the requests after this one — see below
  auth_token: $.token
```

Unknown top-level keys are rejected rather than silently ignored, so a typo in a
field name is an error you see immediately.

## Sending the same header more than once

HTTP allows a header name to repeat, and some APIs need it: several
`Set-Cookie`-shaped headers, several `X-Forwarded-For` values, or any
API-specific header a client may send twice. A YAML mapping cannot have two
keys of the same name, so a header value may be **either a scalar or a list of
scalars** — a list sends one header per entry, in the order written:

```yaml
headers:
  Accept: application/json # scalar → one header
  X-Forwarded-For: # list → two headers, in this order
    - 1.2.3.4
    - 5.6.7.8
```

Header order is preserved exactly as written, both between different names and
among the repeats of one name. Two entries with the same name *and* the same
value are accepted rather than rejected: Sendra rejects ambiguity, not
redundancy, and repeating a value is an explicit (if pointless) choice, not a
thing it has to guess about.

Two other consequences of headers being an ordered list rather than a map:

- A **config default is still suppressed** by a request header of the same
  name, compared case-insensitively, exactly as before — repetition is a
  request-level choice, not licence for a config default to duplicate
  something the request already set. See
  [Configuration](configuration.md).
- Two header names that only collide **after variable substitution**
  (`{{prefix}}-Key` and `X-Key`, with `prefix` set to `X`) used to be an error,
  because a map would have silently dropped one value. Both are now simply
  sent, since nothing is lost.

## Authentication

`auth:` resolves credentials into the header (or, for `api_key` in `query`
form, query parameter) that goes on the wire, so you don't hand-write
`Bearer <token>`, base64-encode `user:pass`, or add a `headers:`/`query:`
entry yourself. Exactly one of `bearer`, `basic`, `api_key` or `oauth` may be
set:

```yaml
auth:
  bearer: '{{token}}'

# or

auth:
  basic:
    user: '{{username}}'
    pass: '{{password}}'

# or

auth:
  api_key:
    in: header # or: query
    name: X-API-Key # or a query param name
    value: '{{api_key}}'

# or

auth:
  oauth:
    grant_type: client_credentials # or: password
    token_url: https://auth.example.com/oauth/token
    client_id: '{{client_id}}'
    client_secret: '{{client_secret}}'
    scope: read write # optional
    # required only for grant_type: password
    username: '{{username}}'
    password: '{{password}}'
```

- `bearer` sets `Authorization: Bearer <bearer>`.
- `basic` sets `Authorization: Basic <base64(user:pass)>`.
- `api_key` sets a named header or query parameter to a static value.
  `in: query` merges its `name`/`value` onto `url` through the same
  mechanism a request's own `query:` map does — the same percent-encoding,
  and the same "the more structured source wins" rule on a name collision
  with the URL's own query string.
- `oauth` acquires a bearer token from `token_url` before the request is
  sent, then sets `Authorization: Bearer <token>` — the same header `bearer`
  sets directly, just with the token fetched for you rather than written in
  the file. Only the `client_credentials` and `password` grants are
  supported; `grant_type: password` additionally requires `username` and
  `password`, which is rejected at parse time if either is missing.

  Acquiring a token is a real HTTP call, so it is the one part of `auth:`
  that can be slow or fail on its own — a bad `client_secret`, an
  unreachable `token_url`, or a token response with no `access_token` all
  fail with a clear error naming `token_url` and why, rather than a
  confusing downstream `401`. In a collection, that failure is scoped to the
  one request that needed it, the same way an unresolved `{{var}}` is —
  sibling requests using other auth (or none) are unaffected.

  Requests that share the same `token_url`/`client_id`/`grant_type`/`scope`
  acquire **one** token between them for the run, rather than one each — the
  same authentication used four times in a collection is one HTTP call to
  `token_url`, not four. A token is reused until it is close to its
  server-reported `expires_in` (or indefinitely, if the server does not
  report one) and reacquired automatically once it is. None of this is
  written to disk: the cache lives only for the one `sendra` invocation,
  the same "no persistence between separate runs" rule captured variables
  and the cookie jar already follow.

  Not supported: the `authorization_code` grant (it needs a browser
  redirect and a local callback listener — a different shape of problem for
  a headless CLI) and `refresh_token` (no cached token is refreshed; an
  expired one is simply reacquired the same way the first one was).

A request may not set `auth` *and* an explicit header (or, for `api_key` in
`query` form, query parameter) of the same name it would itself set: `auth`
and a hand-written `Authorization`/`X-API-Key`/etc. entry are both trying to
control the same thing, so that's rejected at parse time rather than
silently picking one.

By the time a `pre_request` script or `sendra`'s own request builder sees the
request, `auth` has already been resolved down to a plain header or query
parameter — there is no separate `request.auth` API. See
[`examples/auth.yaml`](../../examples/auth.yaml) for the `bearer`/`basic`/
`api_key` forms run against httpbin.org, and
[`examples/oauth.yaml`](../../examples/oauth.yaml) for both `oauth` grants
(against your own OAuth provider — there is no public demo server for
either grant the way httpbin.org serves `/bearer`):

```sh
cargo run -p sendra-cli -- run examples/auth.yaml
cargo run -p sendra-cli -- run examples/oauth.yaml --env <name>
```

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
