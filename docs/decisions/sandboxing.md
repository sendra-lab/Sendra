# Sandboxing

Background: what a `pre_request`/`post_request` script can see and do is in
[Scripting](../reference/scripting.md).

**A script cannot touch the filesystem or the network.** Sendra registers no
functions, no types, no packages and no modules into the interpreter — the
entire Sendra-shaped surface is `request`/`response`, holding strings,
integers and arrays. There is nothing to audit because there is nothing
registered.

On top of that:

- `import`, and the module system with it, is removed at compile time. This is
  the one that matters: Rhai's *default* module resolver reads `.rhai` files off
  disk, so `import` is the one filesystem path a stock engine has.
- `eval` is disabled. Not a filesystem or network capability, but it would
  defeat the guarantee that a script's syntax is checked before the request is
  sent.
- A script is stopped after ten million operations, so a `while true` nobody
  meant to write gets a named error rather than hanging the process. This is a
  backstop, not a defence against a hostile script: scripts come out of your own
  request files.

`print` and `debug` go to **stderr**, alongside the `→` labels and every error,
so they do not end up inside the single JSON document `--json` promises stdout
holds. A script that printed and then threw keeps its lines: they are usually
the ones that explain the throw.

That choice belongs to the CLI, not to the library. `sendra-core` has no
`println!` or `eprintln!` anywhere in it — running a script returns the lines it
printed alongside its verdict, and `sendra-cli` decides they are stderr. A
`sendra-tui` reusing the same crate will put them somewhere a redrawn frame does
not wipe out, without a library writing over its interface.
