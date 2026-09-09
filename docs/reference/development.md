# Development

```sh
cargo build --workspace --all-targets
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p xtask -- check
```

Those five are exactly what CI runs, on Linux, Windows and macOS, for every
push to `main` and every pull request against it — so a clean local run is a
green build. Clippy is `-D warnings`: a warning fails the build.

The test suite is hermetic. It parses YAML, checks exit-code logic, and resolves
config and environments against directory trees built under a temporary
directory rather than against your real `~/.config`; the tests that name a URL
point at a closed local port on `127.0.0.1`, so they fail before connecting or —
in the one `--json` test that really sends — while connecting. Nothing under
`cargo test` reaches a network, which is what makes CI trustworthy rather than
merely usually-green. The `examples/` files do hit `httpbin.org`, and are run by
hand — deliberately never in CI.

No test calls `std::env::set_var` either. It is process-global, so one test
setting a variable is visible to every test running beside it; the `${VAR}` path
is tested by passing a stand-in OS environment to `Environment` instead, the
same way config resolution takes its directories as arguments. The tests that do
read the real environment only read it, and only for a name nothing could have
set.

See also [`CONTRIBUTING.md`](../../CONTRIBUTING.md) for the same checks framed
as a pre-PR checklist, and for code conventions and commit-message guidance.
