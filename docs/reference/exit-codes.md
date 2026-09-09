# Exit codes

One table for the whole binary, not one per subcommand: `run` and `test` answer
different questions, but they answer them to the same shell, and a number that
means one thing under one command and something else under the other is a trap
for anyone writing `case $? in` around either.

| Code | `run` | `test` | Meaning                                                             |
| ---- | :---: | :----: | ------------------------------------------------------------------- |
| `0`  |   ·   |   ·    | Nothing went wrong — see below for what each command means by that. |
| `1`  |   ·   |   ·    | Some request never got a response.                                  |
| `2`  |   ·   |   ·    | Bad command-line usage (from clap).                                 |
| `3`  |   ·   |        | `run` only: every request got a response, at least one was 4xx/5xx. |
| `4`  |       |   ·    | `test` only: every request got a response, at least one failed a check. |

- `0` — for `run`, every request was sent and no response status was an error
  (1xx, 2xx, 3xx). For `test`, every request got a response and every assertion
  that was declared, passed.
- `1` — some request never got a response: the file was missing or malformed, no
  request by that name, `--env` named an environment with no file behind it, a
  `{{variable}}` or `${VAR}` had no value, a header was invalid, or the request
  never completed (DNS, TLS, connection). The same meaning under both commands,
  which is why it is the same number.
- `2` — bad command-line usage (from clap). `sendra test --allow-error-status`
  is one of these; see [Testing](running-and-testing.md).
- `3` — `sendra run` only: every request completed but at least one server
  answered `4xx` or `5xx`. The responses print exactly as they would otherwise;
  only the exit code differs, so `sendra run req.yaml && deploy.sh` does not
  proceed on a 404.
- `4` — `sendra test` only: every request got a response, but at least one
  failed a check it declared: an assertion that did not hold, a `post_request`
  script that threw, or a `capture` entry that produced no value.

Codes `5` and up are free.

For the reasoning behind these choices — why `4` is a separate number from `3`,
why an unsendable request under `test` exits `1` rather than `4`, and how
these codes rank against each other for a whole collection — see
[Exit codes](../decisions/exit-codes.md).
