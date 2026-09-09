# Assertions

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
between the two commands worth remembering. See
[Testing](running-and-testing.md).

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

## Richer assertions: acceptable sets, patterns, timing, negation, comparisons

Beyond exact status, exact header value, body substring and JSON equality,
`assertions` also has:

```yaml
assertions:
  status_in: [200, 201, 204] # passes if the status is any of these
  body_matches: '"id":\s*\d+' # a regex matched anywhere in the body
  elapsed_ms_under: 2000 # strictly faster than this many milliseconds
  not:
    status: 404 # every key above, inverted — see below
  json:
    $.count: { greater_than: 5 }
```

```sh
sendra run examples/richer-assertions.yaml    # every one of these against httpbin
```

**`not:` wraps a whole assertions block, not one assertion.** It takes the same
keys as the top level (minus `not` itself — `not: {not: {...}}` is a parse
error, not a double negative) and negates each one independently:
`not: {status: 404, body_contains: error}` means "status is not 404" *and*
"body does not contain `error`", not the pair negated together. Reported
wording is symmetric in both directions:

```text
  ✓ status is not 404
  ✗ body does not contain `error` — found in the 214-byte body
```

**A hard error is not something `not:` can turn into a pass.** A malformed
JSON path, an invalid regular expression, a body that is not JSON, a path
selecting zero or several values, a comparison or `length` operator applied to
a value of the wrong type — these are facts about the request or the file, not
a condition to be true or false, so they fail the same way whether or not
they are wrapped in `not:`.

**`json:` takes operators beyond equality, alongside the bare values it
already supports.** A path's value is read as an operator instead of a plain
equality check when it is a YAML mapping with exactly one of these keys:

```yaml
json:
  $.count: { greater_than: 5 } # numeric, and greater_than_or_equal,
  $.count: { less_than: 5 } #  less_than, less_than_or_equal
  $.tags: { contains: b } # substring of a string, or array membership
  $.tags: { length: 2 } # array/string length — equality,
  $.tags: { length: { greater_than: 1 } } #   or a nested comparison
  $.id: { matches: '^[0-9a-f-]{36}$' } # regex against this one string value
```

A multi-key object (`{id: 1, name: ada}`) is never ambiguous and is always
equality — only a single-key mapping using one of the names above is read as
an operator. That means a *literal* expected value shaped like `{greater_than:
5}` is not expressible; in exchange, a path's value never needs a second key
to say which kind of check it is. There is no `not_equal`: `not: {json:
{$.count: 5}}` already says "not equal to 5" precisely, and a dedicated
operator would only be a shorter spelling of that — unlike the comparison
operators, which `not:` cannot reach at all (negating `greater_than` gives
`less_than_or_equal`, not `less_than`, and there is no way to spell "less than
5" purely through negation).

`matches` is `body_matches` narrowed to one selected value instead of the
whole body — useful when the pattern only means something at a specific
path (`$.user.email` looks like an email; `$.id` looks like a UUID) and
searching the entire body for it would risk a false match somewhere else in
the response.
