# Scripting

A request can carry two inline scripts: `pre_request`, which runs just before it
is sent, and `post_request`, which runs against the response. They are written
as YAML block scalars (`|`) and the language is [Rhai].

```yaml
method: POST
url: https://api.example.com/orders
body: '{"widget": "sprocket"}'
pre_request: |
  request.headers["X-Request-Id"] = "abc-123";
post_request: |
  if response.status != 201 {
    throw "expected 201, got " + response.status;
  }
```

```sh
sendra run examples/scripted-request.yaml
sendra test examples/scripted-request.yaml
```

**No runtime to install.** The interpreter is linked into the `sendra` binary,
so a scripted request works anywhere the binary does — which is the whole reason
for Rhai over an embedded JavaScript engine.

## What `pre_request` can see

`request` is an object map. It arrives fully substituted and with any config
defaults already merged in, so a script sees the request exactly as it would
otherwise be sent, and whatever it leaves behind is what actually goes out.

| Field             | Access     | Type                             |
| ----------------- | ---------- | --------------------------------- |
| `request.method`  | read       | string — `"POST"`                |
| `request.url`     | read/write | string                           |
| `request.headers` | read/write | object map, string to string     |
| `request.body`    | read/write | string, or `()` for no body      |

```rhai
request.headers["X-Signature"] = sign(request.body);
request.headers.remove("Authorization");
request.url = request.url + "?dry_run=1";
request.body = ();
```

**`method` is read-only.** A script may branch on it; assigning to it is an
error, not a silent no-op. It is a closed enum in the schema, so a bad method is
caught by serde today at parse time with a position in the file; the `→` label a
run prints is `METHOD url`, printed before the script runs, so a script that
changed it would make that line a lie about what went over the wire; and a call
that needs to be both a GET and a POST is two requests, which is clearer written
as two.

Anything else is refused rather than ignored. `request.timeout = 5` names a
field a request does not have and is an error, the same way an unknown key in
the YAML is an error. Values are not coerced either: `request.headers["X"] = 5`
is an error asking for `.to_string()`, because a header is a string on the wire
and silently stringifying leaves what a float renders as up to the interpreter.

**`request.headers` is a map, even though a request file can repeat a header
name.** The map is what makes `request.headers["X"] = …` and `.remove("X")`
mean the obvious thing, and a list-of-pairs API would make every script that
touches a header pay in syntax for a case that is rare in a file and rarer in a
script. Two costs follow, and they apply only to a request that *has* a
`pre_request` script — one without a script never goes through this conversion
and keeps every occurrence:

- A header repeated in the file collapses to its **last** value, since a map
  holds one entry per key.
- A script cannot add a second occurrence of a name: assigning to the same key
  twice is one write, not two headers. A genuinely repeated header has to be
  written in the YAML file, using the list shape described in
  [Request file shape](requests.md).

Header names a script passes through also come back sorted alphabetically
rather than in file order, which is what they did before headers became an
ordered list, so an existing script behaves exactly as it did.

Throwing in `pre_request` means the request is never sent.

## What `post_request` can see

`response` is read-only — assigning to it is an error rather than a change that
goes nowhere.

| Field                  | Type                                              |
| ---------------------- | ------------------------------------------------- |
| `response.status`      | integer — `201`                                   |
| `response.status_text` | string — `"Created"`                              |
| `response.headers`     | array of `#{name, value}`                         |
| `response.body`        | string, as it came over the wire                  |
| `response.elapsed_ms`  | integer                                           |

Headers are a list rather than a map because HTTP lets one repeat
(`set-cookie`), and wire order is worth keeping. Lookup is a one-liner:

```rhai
let ct = response.headers.find(|h| h.name.to_lower() == "content-type");
if ct == () || !ct.value.contains("json") { throw "expected a JSON response"; }
```

**`throw` is how a check fails**, rather than a `fail()` function Sendra would
have had to register. It is Rhai's own, it is the idiom anyone reading Rhai docs
will already know, and choosing it means Sendra registers *nothing at all* into
the interpreter — see [Sandboxing](../decisions/sandboxing.md).

The message you throw is what gets printed, verbatim. A script that fails for
some other reason — a method that does not exist, an index off the end of an
array — keeps the interpreter's full error with a line number instead, because
that is a bug in the script and the line is the point.

## Ordering

For one request, in order:

1. Environment substitution (`{{variable}}`, `${OS_VAR}`).
2. Config defaults applied.
3. `pre_request`.
4. The request is sent.
5. `post_request`.
6. `assertions` evaluated.

**Script source is never substituted.** A `{{var}}` or `${VAR}` inside a script
is not expanded — it is just those characters. Substitution is textual, and the
reason it is confined to values is that a value must not be able to change the
structure of the document around it; a script is not a value, it is code, so the
failure mode would not be a malformed URL but a variable's contents being parsed
as program text. A script that needs an environment value reads it off
`request.url` or `request.headers`, which arrive substituted.

**Scripts and assertions are independent.** Both look at the same response,
neither can see the other, and both are reported. A request can use either, both
or neither.

## Both scripts are compiled before the request is sent

A syntax error in `post_request` stops the `POST` that would have created an
order, rather than being discovered after it. A script that does not compile is
a broken file, and finding that out before anything goes over the wire is
strictly better — the same argument that makes a collection with two
identically-named requests an error at parse time.

That split is also how Sendra tells "your script is wrong" from "your API is
wrong":

- **A script that does not compile** — either hook — is exit `1`, counted under
  `no_response`. Nothing was sent, so it is the same category as a missing
  variable or a refused connection.
- **A `pre_request` script that throws** is exit `1` for the same reason: there
  is no response and never will be.
- **A `post_request` script that throws** is a failed check on a response that
  did arrive, so it behaves exactly like a failed assertion: printed under the
  response, invisible to `run`'s exit code, and a failure under `test`.

`print` and `debug` inside a script go to stderr; see
[Sandboxing](../decisions/sandboxing.md) for the reasoning, and for the
guarantees around what a script can and cannot reach.

[Rhai]: https://rhai.rs
