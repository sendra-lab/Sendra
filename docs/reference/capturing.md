# Capturing values and chaining requests

A request can pull values out of its response and hand them to the requests
after it, under an optional `capture` key: variable names mapped to JSON paths.

```yaml
name: Log in
method: POST
url: '{{base_url}}/login'
capture:
  auth_token: $.token
  user_id: $.user.id
```

Every name captured this way is usable as `{{name}}` in **every request after
this one, in file order**, through exactly the substitution an environment file
feeds — there is one variable syntax, not two:

```yaml
name: Fetch the user
method: GET
url: '{{base_url}}/users/{{user_id}}'
headers:
  Authorization: 'Bearer {{auth_token}}'
```

```sh
sendra run examples/capture-chain.yaml    # a real capture-then-use flow
```

**File order is real order.** A request sees the captures of every request
before it and none of the captures of any request after it. A `{{name}}`
referenced before the request that captures it is a `VariableNotFound` — the
same failure as a typo, which is what it usually is.

**Nothing persists.** Captured values live for one `sendra run` or `sendra test`
invocation and no longer; nothing is written to disk, and a fresh process starts
with nothing captured. Running the second request of a chain on its own fails
loudly rather than quietly reusing a stale token.

**What a path may select.** Exactly one value, and one that has a text form to
substitute:

| Selected               | Captures as                                    |
| ---------------------- | ---------------------------------------------- |
| a string               | the string, unquoted — `"ada"` becomes `ada`   |
| a number               | its value: `42` is `42`, `1.50` is `1.5`       |
| a boolean              | `true` / `false`                               |
| nothing                | a capture failure — the path matched no value  |
| more than one value    | a capture failure — there is no single answer  |
| `null`, array, object  | a capture failure — nothing to substitute      |

`null` has no text form that is not a guess between `""` and `null`. An array or
an object has one, compact JSON, but the reason substitution is safe at all is
that a substituted value cannot change the shape of what it lands in, and
pushing `{"a":1}` into a URL is exactly that hazard.

Numbers going through `serde_json` means `1.50` in a body captures as `1.5` —
the value, not the spelling. That differs from an environment file, where
`port: 8080` is the *string* `8080` and nothing is normalised. An endpoint whose
exact digits matter should send them as a JSON string.

**A bare string is not the only source.** It is the default, and unchanged,
but a `capture` entry can also be an object naming a response header or the
status code instead of a JSON path:

```yaml
capture:
  auth_token: $.token          # unchanged: bare string = JSON path
  session_id:
    header: Set-Cookie          # a response header, matched case-insensitively
  request_status:
    status: true                 # the numeric status, as a string
```

A `header:` capture reads from the *final* response only — the one `capture`
always evaluates against — so with `follow_redirects` on, a `Set-Cookie` set
by an intermediate hop is not reachable this way; disable `follow_redirects`
to capture it from the 3xx response itself, or, for a `Set-Cookie`
specifically, prefer [`cookie_jar`](configuration.md) — it sits underneath
redirect-following rather than downstream of it, so it sees every hop
without giving up automatic redirect-following to do it. A header name that
repeats in the
response (`Set-Cookie` is the common case) is a capture failure rather than a
first-or-last guess, the same rule a JSON path selecting several values
already follows — a capture binds a name to *one* value, and silently picking
between repeats would make the same file behave differently depending on
header order a server happens to send in.

```sh
sendra run examples/capture-header-status.yaml    # header and status capture, chained
```

## When a capture does not work

A capture that produces no value is reported under the request that declared it,
in the same shape as the assertions above it:

```text
capture
  ✓ auth_token from `$.token`
  ✗ user_id from `$.user.id` — matched nothing in the response body
```

**The captured value is not printed.** Every other block shows what it compared,
so the omission is deliberate: a capture exists to carry a token or a session
id, and putting those on a terminal — a CI log, most of the time — would be a
decision the file's author never made. Nothing is hidden by it: under
`sendra run` the body it came from is printed in full just above, and `--json`
carries the values because it already carries that same body.

**A failed capture is a failed check, and the run carries on.** It counts
exactly as a failed assertion or a `post_request` throw does: visible in the
output, `sendra test` exits `4`, and `sendra run`'s exit code is untouched. It
happens *after* a response arrived — the request was sent, the server answered,
and the answer did not hold what the file said it would — which is that
category and not "could not send".

The requests after it are still sent, in keeping with every other per-request
failure in Sendra: one request's problem does not cancel its siblings. A
downstream request that needed the variable then fails on its own terms, with
`VariableNotFound` naming it, and *that* is a "never got a response" — so a run
whose broken capture broke a chain exits `1`, the more serious of the two, with
the original failure reported at the request that caused it:

```text
→ Captures nothing
200 OK  3662 ms

capture
  ✗ auth_token from `$.json.token` — matched nothing in the response body

→ Needs it
error: no variable named `auth_token` in the active environment: no environment
       file was found

→ Independent
200 OK  1771 ms

summary
  3 requests: 0 passed, 1 failed, 1 without assertions, 1 no response
```

**A capture that *works* is not a check.** A login that captures a token and
asserts nothing is counted under `without assertions`, not under `passed`: a
capture is a dependency of the rest of the run, not an expectation about the
response, and counting it as a pass would report a check nobody wrote. The
asymmetry with the paragraph above is the point — a capture only enters the
verdict when it fails.

## Name collisions

**A capture whose name the environment file already defines is refused.**

```text
capture
  ✗ base_url from `$.host` — `.sendra/environments/staging.yaml` already
    defines this variable; rename the capture or the environment entry
```

Neither value silently wins, and that is the whole reason: if the capture won,
`{{base_url}}` would mean the file's value in the requests before the capturing
one and the captured value in the requests after it — the same name, two
meanings, in one run, discoverable only by reading both files and counting
positions. If the environment won, the `capture` block would be a no-op that
still looks like it did something. Refusing says which two lines are in conflict
and costs one rename. It is also the reversible choice: this can be relaxed into
a precedence rule later, while a build that had been silently shadowing could
not be tightened. Sendra rejects duplicate request names in a collection for the
same reason.

The refused capture defines nothing, so the environment's value stands for every
request in the run — the file keeps meaning what it says.

**Two captures of the same name are fine, and the later one wins.** That is not
the same situation: both come from the same mechanism, file order fully decides
which is in force at any point, and re-logging in or reading the next page's
cursor is a real flow that has to be expressible.

## What a capture does not touch

- **The block itself is never substituted.** A `{{var}}` in a capture path or
  name stays those characters. A JSON path selects *which* part of the response
  is read, exactly as it does in an `assertions` block where paths are left
  literal so that `--env` cannot silently redirect a check; and a variable name
  that changed with the environment could not be written as `{{name}}` in the
  request that uses it.
- **Scripts and captures cannot see each other.** A `pre_request` script cannot
  rewrite a `capture` block, a `post_request` script cannot read what was
  captured, and a script cannot stash a value for a later request. All three are
  natural extensions and none of them is here.
- **Assertions and captures cannot see each other either.** They are evaluated
  independently against the same response: a failed assertion does not stop the
  capture beside it, and a failed capture does not fail an assertion. Both are
  reported, and a request that broke both is one failed request.
