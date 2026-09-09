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
