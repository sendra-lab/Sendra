# JSON output

`--json` replaces the terminal output with one JSON object describing the whole
run, on stdout. Both subcommands take it:

```sh
sendra run collection.yaml --json | jq '.requests[] | select(.response.status >= 400)'
sendra test collection.yaml --json > results.json
```

**Stdout holds the document and nothing else.** The `→ label` lines and every
error message stay on stderr, where they already were, so a redirected stdout is
a file `jq` can read and a terminal still shows what went wrong as it happens.

**Exit codes are unchanged.** `--json` is a different serialisation of the same
result, not part of deciding it: the [table](exit-codes.md) applies to both
renderings, and a run reports the same number either way.

**One object per invocation, not one per request.** A stream of objects would
make `sendra run collection.yaml --json | jq .` a stream of documents rather
than a document, and `test`'s summary would have nowhere to live in it. The cost
is that nothing is printed until the run is over.

## `sendra run --json`

```json
{
  "requests": [
    {
      "label": "Get user",
      "response": {
        "status": 200,
        "status_text": "OK",
        "elapsed_ms": 412,
        "headers": [
          { "name": "content-type", "value": "application/json" }
        ],
        "body": "{\"id\":7,\"name\":\"ada\"}"
      },
      "error": null,
      "post_request": null,
      "assertions": {
        "total": 2,
        "passed": 1,
        "failed": 1,
        "results": [
          {
            "kind": "status",
            "expectation": "status is 200",
            "passed": true,
            "failure": null
          },
          {
            "kind": "json_path",
            "expectation": "`$.name` is \"ada\"",
            "passed": false,
            "failure": "got \"grace\""
          }
        ]
      }
    }
  ]
}
```

- `requests` — one entry per request the run attempted, in file order.
- `label` — the request's `name`, or `METHOD url` when it has none. The same
  label the `→` line on stderr shows.
- `response` and `error` — always both present, exactly one of them `null`. A
  request either came back or it did not; `error` carries the message and its
  causes joined with `: `, so a connection failure names both the request and
  the reason.
- `body` — the raw body, exactly as it arrived. The indenting described under
  [Running requests](running-and-testing.md) is for a terminal; rewriting the
  server's bytes inside a document about them would misreport what came back.
  `jq` has `fromjson` when you want it parsed.
- `headers` — a list of `{name, value}` objects rather than one object keyed by
  name, because HTTP lets a header repeat (`set-cookie`) and a map would drop
  all but one of them. Wire order is preserved.
- `assertions` — always an object, with an empty `results` list for a request
  that declared none. `kind` is one of `status`, `header`, `body_contains` or
  `json_path` — the keys the `assertions` block is written with.
  `expectation` and `failure` are the same strings the terminal prints.
- `post_request` — `null` for a request that declared no script, which is a
  different thing from a script that ran and passed; otherwise
  `{"passed": true, "failure": null}` or
  `{"passed": false, "failure": "expected 201, got 500"}`. The same pair, in the
  same spelling, that each assertion result carries, because it is the same kind
  of statement about the same response. There is no matching `pre_request` key:
  a `pre_request` script that fails means the request was never sent, which
  `error` already says, and one that succeeds has nothing to report beyond the
  request that went out.
- `capture` — `null` for a request that declared no `capture` block, which is a
  different thing from a block that captured nothing; otherwise an object with
  two keys. `values` is a plain name-to-value object, so chaining a captured
  token into another tool is `.requests[0].capture.values.auth_token` rather
  than a search through a list. `failures` is a list of
  `{variable, path, failure}` for the entries that produced no value, empty when
  they all did — `failure` is core's own wording, the same string the terminal
  shows. The values are here even though the terminal does not print them: the
  document already carries every response body verbatim, so they are text that
  is in the output twice rather than a secret this key newly exposes.

## `sendra test --json`

The same document, plus a `summary` object holding the counts the terminal run
ends with:

```json
{
  "requests": [ "..." ],
  "summary": {
    "total": 4,
    "passed": 2,
    "failed": 1,
    "without_assertions": 1,
    "no_response": 0
  }
}
```

Every count is present, zeroes included — the terminal leaves a zero out, and a
script reading `.summary.failed` should not have to know that. The four
categories are the ones described under [Testing](running-and-testing.md) —
including the rule that a `post_request` failure is counted in `failed` — and
they still add up to `total`.

Two differences from the terminal output are worth stating:

- `summary` is **absent** under `run`, rather than null. `run` has no summary;
  it is not a summary that is empty.
- `requests` carries whole responses under `test` — headers and body included —
  where the terminal shows a status line only. That brevity is a decision about
  what is readable on a screen, and a program reading the output has no such
  problem.

**When the run never starts, stdout stays empty.** A missing file, a config that
does not parse, a `--env` naming an environment that is not there: these fail
before the first request, so there is no document to write. The error is on
stderr and the exit code is `1`, as it is without the flag.

**No stability promise yet.** This is v1 and Sendra has no external consumers;
the shape above is the one to script against today, and it will grow keys before
it is frozen.
