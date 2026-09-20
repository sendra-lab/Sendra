---
description: "Use the sendra tui terminal app (TUI) to browse, run and edit requests, switch environments, review run history and find every keybinding."
---
# The interactive TUI

`sendra tui` opens a full-screen terminal app for browsing and running the
requests in a collection (or a single request file), instead of one shell
invocation per request. It reuses `sendra-core` directly (the same request
model, YAML loading, environment resolution, capture and HTTP execution
`run`/`test` use) with one exception: `pre_request`/`post_request` scripts
are not run from the TUI (see below). Everything else behaves identically
whether you send it from the TUI or from the command line.

## Installing and launching

The TUI ships inside the same `sendra` binary as `run`/`test`: there is
nothing separate to install. Building or installing `sendra` (see the
[README](../../README.md#install)) gets you both.

```sh
sendra tui                       # start with nothing loaded
sendra tui requests/collection.yaml
sendra tui requests/request.yaml  # a single request file works too
sendra                           # bare invocation, identical to `sendra tui` with no path
```

A bare `sendra`, with no subcommand at all, launches the TUI with nothing
preloaded. This is deliberate, not a fallback: typing just `sendra` is meant
to be a reasonable way to start.

The TUI requires an interactive terminal on stdout. If stdout has been
redirected or piped, it refuses immediately with a plain error rather than
drawing frames nobody can see:

```
sendra-tui requires an interactive terminal on stdout; it looks like stdout
has been redirected or piped. Run it directly in a terminal.
```

## The welcome screen

Launching with no path (`sendra tui` or a bare `sendra`) shows a welcome
screen instead of an error. It looks in the current directory for `.yaml`/
`.yml` files sitting directly inside it (not subdirectories, and not
`.sendra/`) and offers whatever it finds as a picker:

- **Something found nearby**: a selectable list of the candidate files.
  Move with `↑`/`↓` or `j`/`k`, `Enter` (or `r`) opens the highlighted one.
- **Nothing found**: a plain message with the two things you can still do:
  `o` to type a path, `e` to open the environment picker.

`o` is always available from the welcome screen, whether or not candidates
were discovered, to type a path that isn't in the current directory.

## Browsing and running requests

Once a collection is loaded, the main screen shows the request list on one
side and a detail/response pane on the other.

- `↑`/`k`, `↓`/`j`: move the selection.
- `Enter` / `r`: run the selected request.
- `PgUp`/`PgDn`/`Home`/`End`: scroll the response panel. Arrow keys and
  `j`/`k` always move the request-list selection, never the response scroll,
  regardless of whether a response is currently showing. The response
  panel's scroll lives entirely on its own four keys so the same physical key
  never means two different things depending on invisible state.
- `c`: reveal or hide captured values (and, while merely browsing,
  auth-derived header/query values shown in the request preview). Starts
  masked every time; toggling never persists and is never auto-revealed on
  the next run.
- `/`: filter the request list by name, live as you type. `↑`/`↓` moves
  within the filtered list, `Enter` runs the highlighted match, `Esc` clears
  the filter and shows every request again. The underlying selection is
  never reinterpreted while filtering: closing the filter leaves you on the
  same real request you were looking at.
- `i`: edit the selected request.
- `n`: add a new, mostly-empty request to the collection and immediately
  open it in edit mode.
- `d`: delete the selected request, with a confirmation prompt first.
- `e`: open the environment picker.
- `h`: open the run-history browser for the selected request.
- `o`: open another collection, as a new tab.
- `]` / `[`: next / previous collection tab (see below).
- `Ctrl+w`: close the current collection tab.
- `?`: open the keybinding cheatsheet; `q` / `Ctrl+C` quits (asking for
  confirmation first if anything open would be lost).

A request's status, once it has run, shows in the detail pane alongside its
headers and body. Requests that have never been run yet just show their
resolved request preview.

A request's `pre_request`/`post_request` scripts, if it has any, are **not**
run when sent from the TUI; only `sendra run`/`sendra test` run them. Every
other part of the send pipeline (environment substitution, config header
merging, auth/query/body resolution, assertions, capture) is identical
either way; this is the one honest gap, not a silent one. See
[Scripting](scripting.md).

## Editing a request

`i` opens the selected request in an edit form covering every part of the
request that has a corresponding editor. Editing is entirely in-memory until
you save; `Esc` discards every change and restores exactly the state
browsing was in, no matter what was typed.

- `Tab` / `Shift+Tab`: move between fields, in this order: name, method,
  URL, each header row, the body (if editable), each auth field (if any),
  each assertion row, each capture row, then back to name.
- `Left`/`Right`: move the cursor within a text field, or toggle a
  closed-set field (API key location, OAuth grant type, an assertion's
  operator, a capture's kind) when the focused field is one of those instead
  of free text.
- `Up`/`Down`: move within the body field specifically (only meaningful
  while the body has focus).
- `Enter`: insert a newline (body field only).
- `Backspace`/`Delete`: edit the focused text field.
- `Ctrl+n` / `Ctrl+d`: add / delete a header row.
- `Ctrl+a` / `Ctrl+x`: add / delete an assertion row.
- `Ctrl+p` / `Ctrl+k`: add / delete a capture row.
- `Ctrl+l`: start an interactive OAuth login (only reachable when editing an
  `auth.oauth` block whose grant type is `authorization_code`; see below).
- `Ctrl+s`: save. Refuses (staying in edit mode, with the problem shown)
  when the method is invalid, an assertion value doesn't parse, or a
  `json:`-typed body doesn't parse as JSON.
- `Esc`: cancel, discarding the edit.

**Method, URL, name.** Plain text fields. An invalid method is flagged live
and blocks saving; the name is optional: clearing it back to blank saves the
request as genuinely nameless again, not as an empty string.

**Headers.** A list of key/value rows, added and removed with `Ctrl+n`/
`Ctrl+d`, each a separate text field for the key and the value.

**Body.** Editable as raw text when the request has a plain `body:`, a
`json:` body, or no body at all. A `json:` body is shown pretty-printed and
parsed back into JSON on save; a bad-JSON edit blocks saving with the parse
error shown, the same way an invalid method does. `body_file`, `form` and
`multipart` bodies are shown read-only, with a note explaining why: a
`body_file` names a file some other tool may have open, and `form`/
`multipart` are structured, list-of-parts bodies that would need their own
row editor; both are honest gaps, not silent ones.

**Auth.** Only a request that already has an `auth:` block can edit one here;
this editor never introduces a new auth type on a request that had none.
Whichever of `bearer`/`basic`/`api_key`/`oauth` is set shapes which fields
appear:

- **Bearer**: one token field.
- **Basic**: username and password fields.
- **API key**: name, value, and a `Left`/`Right`-toggled location
  (`header`/`query`).
- **OAuth**: grant type (`client_credentials`/`password`/
  `authorization_code`, cycled with `Left`/`Right`), token URL, client ID,
  client secret and scope always shown; `username`/`password` only reachable
  under `password`; `authorization_url`/`redirect_uri` only reachable under
  `authorization_code`. These fields are the static configuration sent to the
  token endpoint; editing them never fetches or validates a token itself.

**OAuth `authorization_code` login.** Because this grant needs a real human
in a real browser, it cannot acquire a token just from the fields above the
way `client_credentials`/`password` can. With the auth editor focused on an
`authorization_code` block, `Ctrl+l` starts an interactive login: it opens
your browser to the configured authorization URL, runs a local callback
listener bound to the configured redirect URI, waits (up to five minutes) for
the provider to redirect back, and exchanges the resulting code for a token.
The acquired token is cached in memory for the rest of the session: every
later run of a request sharing that `oauth:` config reuses it automatically,
with no separate step. As a side effect, `client_credentials`/`password`
tokens are cached the same way and also survive between runs in the same
session, subject to their own expiry.

**Assertions.** Only `json:` path assertions (and their `not:` negated form)
have a row editor here: `Ctrl+a`/`Ctrl+x` add/remove a row of path,
operator, expected value and a negate toggle. The operator (`equals`,
`greater_than`, `greater_than_or_equal`, `less_than`, `less_than_or_equal`,
`contains`, `length`, `matches`) is cycled with `Left`/`Right`. Every other
assertion kind (`status`, `status_in`, `headers`, `body_contains`,
`body_matches`, `elapsed_ms_under`) is preserved exactly as loaded but has no
editor here; an intentional gap, not a silent one.

**Captures.** A full row editor: `Ctrl+p`/`Ctrl+k` add/remove a row of name,
kind (`json path`/`header`/`status`, cycled with `Left`/`Right`) and, for the
two kinds that need one, the path or header name to read.

## Multi-collection tabs

`o` opens another collection alongside the ones already open, as a new tab.
Each tab is a fully independent session: its own selection, run state, edit
session, environment, dirty markers and run history never bleed into another
tab's.

- `]` / `[`: next / previous tab, wrapping around.
- `Ctrl+w`: close the active tab. If it has nothing at stake (no edit in
  progress, no unsaved new request, no run in flight), it closes
  immediately; otherwise a confirmation prompt asks first. Closing the very
  last remaining tab never leaves you with zero tabs: it resets that tab
  back to the welcome screen instead.

The tab bar itself only appears once a second collection is open: a single
open collection draws with no tab bar at all, and the `]`/`[`/`Ctrl+w` hints
in the status bar only appear once a second tab exists to switch to or
close.

## Run history

`h`, while browsing, opens the run-history browser for the currently selected
request: every past run of that specific request made during this session,
most recent first. History is in-memory only: it is never written to disk,
and it is gone the moment the process exits or the tab closes, since a
response can carry arbitrary (and possibly sensitive) response bodies never
asked to be persisted.

- `↑`/`k`, `↓`/`j`: move the selection.
- `Space`: expand or collapse the highlighted entry in place (a quick
  status-code-and-summary glance without leaving the list; more than one
  entry can be left expanded at once).
- `Enter`: view the highlighted entry's full result, in the same response
  panel a live run uses.
- `Esc`: back to the list (from the full-result view), or close the browser
  (from the list).
- `c`, while viewing one entry's full result: reveal or hide that entry's
  captured values, independent of the live run's own reveal state.

**Capped at 20 entries per request.** Once a request's history passes 20
runs in this session, the oldest are dropped to make room for the newest,
generous for what browsing actually needs, while bounding memory for a
long session that re-sends the same request many times. Dropping is never
silent: the browser says how many older runs were dropped once it happens.

Deleting a request drops its history outright along with it; there is no
longer a request left for it to be about.

## Environments

`e` opens the environment picker: every environment `sendra-tui` found at
startup (from `.sendra/environments/`), sorted by name. An environment file
that exists but failed to load is shown as a visible in-app error rather
than silently missing from the list.

- `↑`/`k`, `↓`/`j`: move the selection.
- `Enter`: select this environment as the one the detail pane resolves
  variables against.
- `i`: edit this environment's variables.
- `Esc`: close, without changing which environment is active.

Editing an environment's variables (`i`) opens a row editor: name/value
pairs, the same shape a request's own headers use:

- `Tab`/`Shift+Tab`: move between fields.
- `Left`/`Right`: move the cursor.
- `Backspace`/`Delete`: edit the focused field.
- `Ctrl+n`: add a variable row.
- `Ctrl+d`: delete the focused row (asks to confirm first).
- `Ctrl+s`: save; refuses, with the problem shown, on an empty or
  duplicated variable name.
- `Esc`: cancel, discarding every change.

## Confirmations

Every destructive action (deleting a request, deleting an environment
variable, closing a tab, quitting with something unsaved) asks first,
through the same yes/no prompt:

- `y` / `Enter`: confirm.
- `n` / `Esc`: cancel.

Quitting (`q` / `Ctrl+C`) checks every open tab, not just the active one: an
edit left open, a request added but not yet saved, a pending delete, an
environment edit in progress, or a run still in flight in *any* tab is
enough to trigger the prompt. With nothing at stake anywhere, quitting exits
immediately with no prompt at all. Pressing the quit key a second time while
the prompt is already open confirms and exits, the same low-friction
"ask once" behavior the dedicated `y`/`Enter` gives.

## Opening another collection

`o` opens a path-input prompt:

- `Left`/`Right`: move the cursor.
- `Backspace`/`Delete`: edit the typed path.
- `Enter`: open the typed path as a new tab.
- `Esc`: cancel, opening nothing.

A failed load (a bad path, invalid YAML) keeps the prompt open with the error
shown, rather than discarding what was typed.

## The keybinding cheatsheet

`?`, from anywhere in the app, opens a full reference of every binding in the
current context: global keys, then whichever screen or mode is active. `?`
or `Esc` closes it again. It draws on top of every other overlay, including
the quit confirmation, so "what can I press right now" always has an answer.

The reference below mirrors that in-app cheatsheet, the same table this
document's own keybinding sections above are drawn from.

### Global: always available

| Key | Action |
| --- | --- |
| `q` / `Ctrl+C` | quit (asks first if anything is unsaved) |
| `?` | open/close the cheatsheet |

### Browsing (a collection is loaded, nothing else open)

| Key | Action |
| --- | --- |
| `↑`/`k`, `↓`/`j` | move selection |
| `Enter` / `r` | run the selected request |
| `i` | edit the selected request |
| `n` | add a new request |
| `d` | delete the selected request |
| `e` | open the environment picker |
| `h` | open the run-history browser |
| `o` | open another collection |
| `/` | filter the request list by name |
| `]` / `[` | next / previous collection tab |
| `Ctrl+w` | close the current collection tab |
| `PgUp`/`PgDn`/`Home`/`End` | scroll the response panel |
| `c` | reveal / hide captured values |

### Filtering the request list (after pressing `/`)

| Key | Action |
| --- | --- |
| (type) | narrow the list to matching names, live |
| `↑`/`↓` | move selection within the filtered list |
| `Enter` | run the highlighted (filtered) request |
| `Esc` | clear the filter and show every request again |

### Welcome screen (no collection path given yet)

| Key | Action |
| --- | --- |
| `↑`/`k`, `↓`/`j` | choose a discovered collection |
| `Enter` / `r` | open the chosen collection |
| `o` | type a collection path instead |
| `e` | open the environment picker |

### Editing a request

| Key | Action |
| --- | --- |
| `Tab` / `Shift+Tab` | next / previous field |
| `Left`/`Right` | move cursor (or toggle an enum field) |
| `Up`/`Down` | move within the body field (body focused only) |
| `Enter` | newline (body field only) |
| `Backspace`/`Delete` | edit text |
| `Ctrl+n` / `Ctrl+d` | add / delete a header row |
| `Ctrl+a` / `Ctrl+x` | add / delete an assertion row |
| `Ctrl+p` / `Ctrl+k` | add / delete a capture row |
| `Ctrl+l` | log in (`auth.oauth` grant_type: `authorization_code` only) |
| `Ctrl+s` | save |
| `Esc` | cancel |
| (any other character) | type into the focused field |

### Environment picker

| Key | Action |
| --- | --- |
| `↑`/`k`, `↓`/`j` | move selection |
| `Enter` | select this environment |
| `i` | edit this environment's variables |
| `Esc` | close |

### Editing an environment's variables

| Key | Action |
| --- | --- |
| `Tab` / `Shift+Tab` | next / previous field |
| `Left`/`Right` | move cursor |
| `Backspace`/`Delete` | edit text |
| `Ctrl+n` | add a variable row |
| `Ctrl+d` | delete the focused row (asks to confirm) |
| `Ctrl+s` | save |
| `Esc` | cancel |
| (any other character) | type into the focused field |

### Run-history browser

| Key | Action |
| --- | --- |
| `↑`/`k`, `↓`/`j` | move selection |
| `Enter` | view this entry's full result |
| `Space` | expand / collapse this entry |
| `Esc` | close |

### Viewing one history entry

| Key | Action |
| --- | --- |
| `PgUp`/`PgDn`/`Home`/`End` | scroll the response |
| `c` | reveal / hide captured values |
| `Esc` | back to the history list |

### Any confirmation (delete request, delete variable, close tab, quit)

| Key | Action |
| --- | --- |
| `y` / `Enter` | confirm |
| `n` / `Esc` | cancel |

### Open-collection prompt

| Key | Action |
| --- | --- |
| `Left`/`Right` | move cursor |
| `Backspace`/`Delete` | edit text |
| `Enter` | open the typed path |
| `Esc` | cancel |
| (any other character) | type into the path field |
