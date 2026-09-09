# Why `test` ignores an unasserted status

Background: the mechanics and the four outcome categories (`passed`, `failed`,
`without assertions`, `no response`) this decision assumes are in
[Testing](../reference/running-and-testing.md).

**A request can be checked two ways, and the categories are about the checking,
not about which mechanism did it.** An `assertions` block and a `post_request`
script are independent features that both look at the same response, so a
request with a script and no assertions is a pass when the script is happy — not
an unchecked one — and a `post_request` script that throws is counted in
`failed` alongside a failed assertion.

That is deliberate rather than convenient. A thrown script and a failed
assertion say the same thing to whoever reads the run — "the response was not
what this file said it should be" — so a fifth count would be a number every
consumer immediately added to `failed`. The distinction actually worth drawing
is "is the file wrong or is the API wrong", and it is already drawn one step
earlier: a script that does not *compile* is a broken file and lands in
`no response`, because both hooks are compiled before the request is sent.
Drawing it a second time at runtime would mean sorting a deliberate `throw` from
an accidental type error, which blurs the moment a `throw` happens inside a
helper function — and guessing wrong would file "your API is broken" under "your
script is broken". One reliable split beats two when one of them is guesswork.

`without assertions` keeps its name, and every request it counts genuinely has
no assertions; it is now the narrower of the two readings, since a request with
a script is counted as checked.

**A request with no assertions is not a pass.** It is a request nobody said
anything about, and it gets its own count for that reason. Folding it into
`passed` would let a collection with no assertions anywhere report a perfect
green run, which is the most misleading thing a test command can do; folding it
into `failed` would break the build every time somebody added a request before
writing expectations for it. The count is the honest answer — "these ran, and
nothing was checked" — and what to do about it is yours.

**A status nobody asserted does not fail a test run.** `Unasserted 404` in
the example run comes back `404` and the run still exits `4` because of the
*assertion* that failed elsewhere, not because of it. This is the debatable
one, so, plainly: `test`'s contract is that the file says what it expects and
`test` reports whether it got it. Failing on a bare `404` means asserting
something the file never wrote down — inventing an expectation on the author's
behalf — which is the same class of mistake as an assertion silently ignored
because of a typo, only inverted. Sendra refuses to guess everywhere else in
its schema, and the check is one line to write when you want it:

```yaml
assertions:
  status: 200
```

It also keeps a real use intact: a request that is in the collection to *reach*
an endpoint — a login, a setup call — rather than to be checked. And the
raw-status question already has a command that answers it, and answers it well:
`sendra run`, exit `3`. Nothing is lost by `test` declining to answer it a
second time with a different number. The safeguard against the decision hiding a
problem is the summary itself: `without assertions` is printed, so a run whose
expectations were never written is visibly not the same thing as a run that
passed.
