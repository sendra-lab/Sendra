# Running and testing requests

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

A response body whose `content-type` is `application/json` — or anything with a
`+json` suffix, such as `application/problem+json` — is printed indented, with
the server's key order preserved:

```text
200 OK  412 ms
content-type: application/json

{
  "id": 7,
  "name": "ada",
  "tags": [
    "a",
    "b"
  ]
}
```

The `content-type` is the only thing consulted; a body that merely starts with a
`{` is printed as it arrived. So is a body that claims to be JSON and does not
parse — a truncated response is exactly when the raw bytes are worth seeing, so
it is printed rather than swallowed.

`sendra test` sends the same requests the same way and answers a different
question about them; see below.

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
- The exit code comes from the checks the file declared — its assertions and its
  `post_request` script.

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

| Category             | Meaning                                                     |
| -------------------- | ----------------------------------------------------------- |
| `passed`             | Got a response, checked it, and everything it checked held.  |
| `failed`             | Got a response and one or more of its checks did not hold.   |
| `without assertions` | Got a response and checked nothing at all.                   |
| `no response`        | Never got a response, so there was nothing to check against. |

The last three are printed only when they are not zero, so a clean run reads
`4 requests: 4 passed` and nothing competes with it. A request that declared
nothing prints a dimmed `no assertions` where its results would have gone, so
the `without assertions` count has something to point at.

For the reasoning behind these categories — why a `post_request` script counts
as a check the same way an assertion does, and why a status nobody asserted
does not fail a `test` run — see
[Why `test` ignores an unasserted status](../decisions/why-test-ignores-unasserted-status.md).

**A `post_request` script that throws fails a test run** the same way a failed
assertion does, and prints the message it threw:

```text
→ Create order
500 Internal Server Error  212 ms

post_request
  ✗ expected 201, got 500

summary
  1 request: 0 passed, 1 failed
```

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

See [Exit codes](exit-codes.md) for `4` and how it ranks against `1`.
