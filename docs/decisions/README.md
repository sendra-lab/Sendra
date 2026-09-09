# Design decisions

The rationale behind the design choices in Sendra's schema and CLI that could
reasonably have gone another way — an ADR-style collection, one file per
decision. For the shape of a feature (what a field is called, what it does),
see [docs/reference.md](../reference.md); these are the "why", not the "what".

- [Why `test` ignores an unasserted status](why-test-ignores-unasserted-status.md) —
  why a `404` nobody wrote an assertion against does not fail a `sendra test`
  run, and why a `post_request` script counts as a check the same way an
  assertion does.
- [Exit codes: why the numbers are split the way they are](exit-codes.md) —
  why `4` is a separate number from `3`, why an unsendable request under
  `test` exits `1` rather than `4`, and how these codes rank against each
  other for a whole collection.
- [Sandboxing](sandboxing.md) — the guarantees behind `pre_request`/
  `post_request`: no filesystem, no network, nothing registered into the
  script interpreter beyond the request/response themselves.
- [Precedence, start to finish](precedence-chain.md) — every layer that can
  decide a value for a request — defaults, config, environment, captures, CLI
  overrides, `pre_request` — stated as one chain.
