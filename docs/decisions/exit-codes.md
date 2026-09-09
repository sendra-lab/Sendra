# Exit codes: why the numbers are split the way they are

Background: the full table of codes and what each one means is in
[Exit codes](../reference/exit-codes.md).

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

A failed check never enters `run`'s answer: `sendra run` prints assertion
results, `post_request` results and capture results, and reads none of them when
deciding what to return, so a run that reports "2 failed" still exits `0`. That is permanent,
not a stage on the way to unifying the two commands — wiring checks into `run`'s
exit code would silently change what every existing
`sendra run req.yaml && deploy.sh` means the moment an `assertions` block or a
`post_request:` block is added to `req.yaml`, and `sendra test` exists so that
nobody has to. See
[Why `test` ignores an unasserted status](why-test-ignores-unasserted-status.md)
for the whole argument, and `exit_for_response` in `sendra-cli/src/exit.rs` for
the single place that decision lives.

The table in [Exit codes](../reference/exit-codes.md) lives next to the `Exit`
enum in `sendra-cli/src/exit.rs`, and `Summary` beside it is where `test`'s half
of it is decided.
