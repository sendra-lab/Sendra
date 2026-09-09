# Precedence, start to finish

Every layer that can decide a value for a request is introduced in its own
place — [Configuration](../reference/configuration.md),
[Environments and variables](../reference/environments.md),
[Capturing values](../reference/capturing.md),
[CLI overrides](../reference/cli-overrides.md) — one pair at a time. Stated as
a single chain, weakest to strongest:

```
hardcoded default
  → global config
    → project config
      → environment file (--env, or the default one)
        → captured variables (as they accumulate through the run)
          → CLI overrides (-H, --var, --timeout, --insecure, --proxy,
                           --client-cert, --client-key, --cookie-jar)
```

A few things are true of every step in that chain and worth stating once
rather than once per pair:

- **Later beats earlier, and that is the whole rule.** Project config beats
  global config key by key ([Configuration](../reference/configuration.md));
  an environment file's variables beat nothing below them because nothing
  below them is a variable, but a request's own `headers:`/`body` beat a
  config default the same way
  ([Configuration](../reference/configuration.md)); a capture beats an
  earlier capture of the same name, and is refused rather than allowed to
  beat an environment file's variable
  ([Name collisions](../reference/capturing.md#name-collisions)); a CLI
  override beats everything, because there is nothing after it.
- **Headers and variables are two different chains**, not one, because a
  header and a `{{var}}` are resolved at different times against different
  inputs — substitution happens once, before a request is ever sent to config
  or a script, and `Config::apply`'s header merge happens after. The table on
  [CLI overrides](../reference/cli-overrides.md) lists what each override
  beats *within its own chain*: `-H` never competes with `--var`, and
  `--timeout`/`--insecure`/`--proxy`/`--client-cert`/`--client-key` compete
  with neither — all five are client-level settings, resolved once when the
  one client the whole run sends through is built, not per request; there is
  no reason for any of them to vary within a single run, the same way there
  is no reason `--timeout` would.
- **A `pre_request` script still runs last of all**, after every override in
  both chains, and can still change or remove anything an override set — the
  same way it can undo a config header. No override in this chain is given
  special protection from a script the request file itself wrote; the request
  file's own script always gets the last, most specific word.
