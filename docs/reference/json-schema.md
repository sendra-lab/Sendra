# JSON Schema / editor support

`schema/*.schema.json` are [JSON Schema](https://json-schema.org/) documents
for the file shapes described in this reference, generated from
`sendra-core`'s actual Rust types via
[`schemars`](https://docs.rs/schemars) — not hand-written, so they cannot drift
from what the code actually accepts without CI catching it (see below).
Point an editor at them for autocomplete, inline validation and
hover-documentation while writing a request file:

| File                            | Covers                                    |
| -------------------------------- | ------------------------------------------ |
| `schema/request.schema.json`     | A single-request file (the common case)    |
| `schema/collection.schema.json`  | A collection file (`requests:` at the top) |
| `schema/config.schema.json`      | `.sendra/config.yaml`                      |
| `schema/environment.schema.json` | `.sendra/environments/*.yaml`              |

**Getting the files.** None of this requires a clone of this repository —
that would only work for someone building Sendra from source, and the actual
audience for editor tooling is anyone who has `sendra` installed. Three ways
to get them, in order of how little they assume you have:

1. **No download at all** — point your editor at the hosted, raw copy:
   `https://raw.githubusercontent.com/sendra-lab/Sendra/<ref>/schema/request.schema.json`
   (and similarly for the other three). `<ref>` is currently a commit SHA,
   pinned rather than `main`, so the schema your editor validates against
   cannot change out from under you between one session and the next —
   **TODO: switch `<ref>` to a release tag once the package/release phase
   ships one**; there is no tag yet to point at. Find the current SHA at
   <https://github.com/sendra-lab/Sendra/commits/main>, or with
   `git rev-parse HEAD` in a checkout.
2. **`sendra schema`** — if you have the binary installed but not the repo,
   this writes the same four files into `./schema/` (or `--output <dir>`),
   baked into the binary at build time, so it works offline:
   ```sh
   sendra schema
   ```
   Safe to re-run after a `sendra` upgrade to pick up a newer schema — unlike
   `sendra init`, it overwrites rather than refusing, since nothing under
   `schema/` is meant to hold anything of yours.
3. **A checkout** — the committed `schema/*.schema.json` files directly, as
   below.

**VS Code**, with the [YAML extension](https://marketplace.visualstudio.com/items?itemName=redhat.vscode-yaml)
installed, add to `.vscode/settings.json`. Local paths (options 2 or 3 above):

```jsonc
{
  "yaml.schemas": {
    "./schema/collection.schema.json": ["**/*collection*.yaml"],
    "./schema/request.schema.json": ["examples/*.yaml"],
    "./schema/config.schema.json": [".sendra/config.yaml"],
    "./schema/environment.schema.json": [".sendra/environments/*.yaml"]
  }
}
```

Or the hosted URL (option 1 — no local file needed at all):

```jsonc
{
  "yaml.schemas": {
    "https://raw.githubusercontent.com/sendra-lab/Sendra/<ref>/schema/collection.schema.json": ["**/*collection*.yaml"],
    "https://raw.githubusercontent.com/sendra-lab/Sendra/<ref>/schema/request.schema.json": ["examples/*.yaml"],
    "https://raw.githubusercontent.com/sendra-lab/Sendra/<ref>/schema/config.schema.json": [".sendra/config.yaml"],
    "https://raw.githubusercontent.com/sendra-lab/Sendra/<ref>/schema/environment.schema.json": [".sendra/environments/*.yaml"]
  }
}
```

Order matters when globs could overlap: the YAML extension uses the last
matching entry, so put the more specific `*collection*.yaml` glob ahead of a
broader one that would otherwise catch it as a plain request. Adjust the globs
to match how your own project names its files — a single-request file has no
naming convention Sendra enforces, so there is no one glob that always finds
every request file and none other.

**Known limitation: structural validation only.** JSON Schema can check field
names, types and required-ness, and — via `oneOf`/`anyOf`/`const`/`minimum` —
a few of Sendra's own business rules that happen to be expressible as pure
structure: a `capture` entry's `status: false` is flagged (`"const": true`),
and `follow_redirects` rejects a negative number (`"minimum": 0`). It
**cannot** express most of what
[`Request::validate`](../../sendra-core/src/request/mod.rs) and
[`Collection::validate`](../../sendra-core/src/collection.rs) enforce at parse time,
because those are cross-field rules, not shape rules:

- exactly one of `body` / `json` / `body_file` / `form` / `multipart` may be set
- exactly one of `auth.bearer` / `auth.basic` may be set
- `auth:` and an explicit `Authorization` header cannot both be set
- a multipart part needs exactly one of `value` / `path`
- every request in a collection needs a `name`, and names must be unique
- `client_cert.cert` and `client_cert.key` are required together

A file can pass editor validation and still be rejected by `sendra run`/`sendra
test` for one of these — the schema's own `description` fields say so at each
relevant property, but an editor's squiggly-underline pass is not a substitute
for actually running the file. This is also why `assertions.json`'s and
`assertions.not.json`'s values show up in the schema as "any value": a JSON
path there may map to a bare value *or* an operator object
(`{ greater_than: 5 }`, say), and that distinction is read by
`Assertions::evaluate`, not by anything schema-shaped.

**Keeping the schema honest.** `schema/*.schema.json` are committed, generated
files — see [`xtask/src/main.rs`](../../xtask/src/main.rs). CI runs
`cargo run -p xtask -- check`, which regenerates every schema in memory from
the current types and fails the build if a committed file would change, so a
schema can never silently go stale against the Rust type it describes.
Regenerate them yourself after changing a schema-relevant type:

```sh
cargo run -p xtask -- generate
```

`schemars` is behind `sendra-core`'s `schema` feature, off by default — an
ordinary build of `sendra-core` or `sendra-cli` never pulls it in; only `xtask`
enables it. `environment.schema.json` is the one file that is hand-written
rather than derived: environment files parse straight into a
`BTreeMap<String, String>` (see [`sendra-core/src/environment/mod.rs`](../../sendra-core/src/environment/mod.rs)),
so there is no dedicated Rust type for `schemars` to point at. It is still
checked by the same anti-drift step, against a literal in `xtask` rather than
against a type.

`sendra schema` (see [`sendra-cli/src/schema.rs`](../../sendra-cli/src/schema.rs))
embeds the same four committed files with `include_str!`, so there is no
separate "embedded copy" that could go stale on its own: it is the exact same
bytes, read at compile time instead of at generation time. A test in that
module reads each file from disk independently at test time and compares it
against what got embedded, as a regression guard against a future change
accidentally hardcoding a literal instead.
