//! The command line as clap sees it: the subcommands, their arguments, and the
//! `--help` text those arguments carry.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// Parse one `-H`/`--header` value: `Name: value`.
///
/// A clap `value_parser` rather than downstream validation, so a malformed
/// value (`-H "no-colon-here"`) is refused where every other bad argument
/// is — with clap's usual message and exit code 2 — instead of surfacing
/// later as a confusing substitution or request-building failure.
///
/// Both sides are trimmed: the natural way to type this is `"Name: value"`,
/// with the space after the colon that HTTP header folding has always
/// treated as insignificant, and trimming it here means that space never
/// becomes part of the value by accident.
fn parse_header_override(raw: &str) -> Result<(String, String), String> {
    let (name, value) = raw
        .split_once(':')
        .ok_or_else(|| format!("expected `Name: value`, got `{raw}` (no `:` found)"))?;
    let name = name.trim();
    if name.is_empty() {
        return Err(format!("header name is empty in `{raw}`"));
    }
    Ok((name.to_string(), value.trim().to_string()))
}

/// Parse one `--var` value: `name=value`.
///
/// Same reasoning as [`parse_header_override`]: a clap-level parser turns a
/// missing `=` into a clear, immediate CLI error rather than a variable that
/// silently never gets set.
fn parse_var_override(raw: &str) -> Result<(String, String), String> {
    let (name, value) = raw
        .split_once('=')
        .ok_or_else(|| format!("expected `name=value`, got `{raw}` (no `=` found)"))?;
    if name.is_empty() {
        return Err(format!("variable name is empty in `{raw}`"));
    }
    Ok((name.to_string(), value.to_string()))
}

/// Parse `--repeat`'s value: a positive count of passes.
///
/// `0` is refused here, at the clap level, rather than accepted as "run
/// nothing" — a collection run always sends at least the one pass it would
/// have sent without the flag, so `--repeat 0` can only be a typo for `1`
/// (the default, and the same as omitting the flag) or for a larger number,
/// and guessing which is not a trade Sendra makes on your behalf.
fn parse_repeat(raw: &str) -> Result<u32, String> {
    match raw.parse::<u32>() {
        Ok(0) => Err("`--repeat` must be at least 1".to_string()),
        Ok(n) => Ok(n),
        Err(_) => Err(format!("expected a positive integer, got `{raw}`")),
    }
}

/// How much of the response — or, under `--dry-run`, the resolved request —
/// the human-readable output shows: `-o`/`--output`.
///
/// `run` defaults to `Full` and `test` defaults to `Status` when the flag is
/// omitted, preserving each subcommand's output exactly as it was before this
/// flag existed. This is a fact about the *human* rendering only: `--json`
/// already carries the whole response regardless of what this would filter,
/// which is why the two are refused together rather than one silently
/// overriding the other — see `reject_output_with_json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum OutputMode {
    /// Status line, headers and body.
    Full,
    /// The status line alone.
    Status,
    /// The body alone — no status line, no headers. For piping into another
    /// tool: `sendra run x.yaml -o body | jq .`.
    Body,
    /// The headers alone — no status line, no body.
    Headers,
    /// Nothing: suppress the response (or resolved-request) rendering
    /// entirely, for when only the assertions/summary below it matter.
    None,
}

#[derive(Parser)]
#[command(
    name = "sendra",
    version,
    about = "Terminal-native HTTP client — send requests defined in YAML files."
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Send the request, or collection of requests, defined in a YAML file.
    Run {
        /// Path to the request or collection file.
        path: PathBuf,

        /// Name of one request to send, when the file is a collection.
        ///
        /// Omit it to send every request in the collection, in file order.
        /// Passing a name to a file that holds a single request is an error:
        /// there is nothing to choose between.
        request: Option<String>,

        /// Name of the environment to substitute `{{variable}}` values from.
        ///
        /// `--env staging` loads `.sendra/environments/staging.yaml`, found by
        /// walking up from the directory you are in. Omit it and the
        /// environment named `default` is loaded if there is one, or no
        /// environment at all if there is not. Naming an environment that has
        /// no file is an error: the run stops rather than quietly sending
        /// against variables you did not ask for.
        // The reasoning behind those two answers lives on `environment_for`;
        // this doc comment is what `--help` prints, so it stays user-facing.
        #[arg(long, value_name = "NAME")]
        env: Option<String>,

        /// Add or override a header for this invocation only, `Name: value`.
        ///
        /// Repeatable. Wins over everything else a header can come from —
        /// config's default headers, the request file's own `headers:`, and
        /// the `Authorization` header an `auth:` block resolves to — because
        /// it is the most specific, most deliberate override available: typed
        /// for this one run, not left in a file for every run after it.
        ///
        /// Passing `-H` more than once for the *same* name (case-insensitive)
        /// replaces the earlier value rather than adding a repeat. That
        /// differs from the request file's own `headers:`, which does allow a
        /// name to repeat — but a name typed twice on one command line reads
        /// as "I meant to change it", the way retyping a shell variable
        /// reassigns it rather than appending to it, not as a deliberate
        /// multi-value header.
        ///
        /// See the docs for the full precedence chain, start to finish.
        // TODO: link the docs site here once it exists.
        #[arg(short = 'H', long = "header", value_name = "NAME:VALUE", value_parser = parse_header_override)]
        header: Vec<(String, String)>,

        /// Set a substitution variable for this invocation only, `name=value`.
        ///
        /// Repeatable, and usable with or without `--env`. Overrides a value
        /// the active environment file defines for the same name, for the
        /// same "most specific wins" reason `-H` overrides config and request
        /// headers. It is treated as if it were part of the environment file
        /// itself rather than as a separate, higher layer: in particular, a
        /// `capture` block naming the same variable a `--var` already set is
        /// refused exactly as it would be for a name the file defines, rather
        /// than letting a capture silently win or silently lose. See the docs
        /// for the full reasoning.
        // TODO: link the docs site here once it exists.
        #[arg(long = "var", value_name = "NAME=VALUE", value_parser = parse_var_override)]
        var: Vec<(String, String)>,

        /// Override the resolved timeout, in seconds, for this invocation
        /// only.
        ///
        /// Applies to the whole request — connect, send and body read — the
        /// same span `timeout_seconds` in a config file covers. Wins over
        /// both the global and the project config, for the same reason every
        /// other override here does.
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<u64>,

        /// Repeat the entire run this many times, sequentially — one full
        /// pass over every selected request completes before the next
        /// begins. Omit it (or pass `1`) to run once, as always.
        ///
        /// Every pass sends the same requests, in the same order, under the
        /// same environment — but each starts with **no captured
        /// variables**: a capture from pass 1 is never visible to pass 2, the
        /// same way nothing survives between two separate invocations. Each
        /// `--repeat` pass is a clean run, not one continuous chain of `N`
        /// times the requests.
        ///
        /// Passes are never sent concurrently — the requests inside one
        /// pass are not either, and this flag does not change that. The
        /// exit code is worst-wins across every request in every pass: one
        /// failure anywhere fails the whole invocation, exactly as it would
        /// within a single pass. Under `--json`/`--junit`, a request's label
        /// is suffixed with `(iteration N of M)` on every pass but the only
        /// one, so `sendra test collection.yaml --repeat 3` reports three
        /// distinct records per request rather than the same one three
        /// times.
        #[arg(long, value_name = "N", default_value_t = 1, value_parser = parse_repeat)]
        repeat: u32,

        /// Disable TLS certificate verification for this invocation only.
        ///
        /// Wins over `insecure: true`/`false` in either config file, the
        /// same "CLI beats config" rule every other override here follows —
        /// but only in the direction that turns it *on*: there is no
        /// `--secure` to force verification back on over a config that set
        /// `insecure: true`, the same way there is no way to un-set a bare
        /// flag like `--dry-run`.
        ///
        /// A real security-relevant setting, not a convenience default:
        /// meant for a self-signed or otherwise untrusted endpoint — an
        /// internal staging host, say — where there is no CA chain to check
        /// against, not for routine use against the public internet.
        /// Whenever this resolves to true, from either source, a one-line
        /// warning prints to stderr before any request is sent, and *is
        /// not* suppressed by `-q`/`--quiet`: `-q` trims narration, and
        /// whether certificate verification is off for this run is a fact
        /// about what is about to happen on the wire, not narration about
        /// how the pipeline resolved. It still has nothing to do with
        /// `--json`, whose stdout contract is unaffected — the warning is
        /// stderr, like every other diagnostic here.
        #[arg(long)]
        insecure: bool,

        /// Route every request through this HTTP proxy for this invocation
        /// only, `http://host:port` (or `http://user:pass@host:port` for a
        /// proxy that requires credentials — read straight out of the URL
        /// by the underlying HTTP client, nothing Sendra-specific).
        ///
        /// Wins over `proxy:` in either config file. Setting it — from
        /// either source — takes over proxying entirely for this run: the
        /// standard `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` environment
        /// variables Sendra otherwise respects by default, matching curl
        /// and most other HTTP tooling, are not consulted once an explicit
        /// proxy is configured.
        #[arg(long, value_name = "URL")]
        proxy: Option<String>,

        /// Present this client certificate for mutual TLS, for this
        /// invocation only. Requires `--client-key` as well — a cert with no
        /// key (or a key with no cert) is refused when the client is built.
        ///
        /// Wins over `client_cert.cert` in either config file — resolved
        /// independently of `--client-key`, so a `--client-cert` here can
        /// pair with a `client_cert.key` a config file set, and vice versa;
        /// only ending up with just one side, from any mix of sources, is an
        /// error. Resolved relative to the current working directory,
        /// matching every other CLI-supplied path (`--junit`, say) — unlike
        /// `client_cert.cert` in a config file, which resolves relative to
        /// that file's own directory. See the docs for the full reasoning.
        #[arg(long, value_name = "PATH")]
        client_cert: Option<PathBuf>,

        /// Private key matching `--client-cert`, for this invocation only.
        /// Behaves exactly as `--client-cert` does — see there — including
        /// resolving relative to the current working directory and
        /// overriding `client_cert.key` independently of `--client-cert`.
        #[arg(long, value_name = "PATH")]
        client_key: Option<PathBuf>,

        /// Exit 0 even when a response status is 4xx or 5xx.
        ///
        /// Responses are printed either way; this only changes the exit code,
        /// for inspecting an error response without failing the surrounding
        /// script.
        #[arg(long)]
        allow_error_status: bool,

        /// Print one JSON object describing the whole run, instead of the
        /// human-readable output.
        ///
        /// `{"requests": [...]}`, one entry per request in file order, each
        /// carrying its response — status, headers, body, elapsed time — or the
        /// error that stopped it, and its assertion results. Nothing else goes
        /// to stdout in this mode: the `→` labels and every error message stay
        /// on stderr, so `sendra run req.yaml --json > out.json` leaves a file
        /// `jq` can read. Exit codes are exactly the same either way.
        #[arg(long)]
        json: bool,

        /// Show captured values verbatim in `--json` output instead of
        /// redacting them.
        ///
        /// A `capture` block often pulls an auth token or other sensitive
        /// value out of a response, and `--json` is the format that ends up
        /// piped into a CI log — a more structured, more attractive target
        /// than the same value sitting inside an escaped response body.
        /// `capture.values` entries are redacted by default; this flag opts
        /// back into the original behaviour. `capture.failures` is never
        /// affected: it names variables and paths, not the values captured
        /// from them. Meaningless without `--json`; accepted either way.
        #[arg(long)]
        show_captures: bool,

        /// Resolve the request fully and print what would be sent, without
        /// sending it.
        ///
        /// Runs every resolution step exactly as an ordinary `run` does —
        /// environment substitution, config header merging, query/body/auth
        /// resolution, `pre_request` — and stops immediately before the
        /// network call. Prints the method, the final URL with `query`
        /// merged in, every header (config-injected and auth-resolved ones,
        /// after `pre_request` has run), and the final body: exactly what
        /// would go on the wire. `post_request` never runs — there is no
        /// response for it to see.
        ///
        /// Headers and bodies are shown in full, secrets included: this is a
        /// deliberate, single, interactive inspection of what Sendra is
        /// about to do, not a log that accumulates over many CI runs, so the
        /// redaction `--show-captures` guards against does not apply here.
        ///
        /// A resolution failure — a missing `{{variable}}`, a script that
        /// throws — is reported exactly as it would be without the flag,
        /// since no network call ever happens either way. For a collection,
        /// every selected request is resolved and printed in turn.
        #[arg(long)]
        dry_run: bool,

        /// How much of the response the human-readable output shows: `full`
        /// (the default), `status`, `body`, `headers` or `none`.
        ///
        /// Omit it and `run` prints exactly what it always has — status
        /// line, headers and body. `-o body` is for piping a response into
        /// another tool without `--json`'s structure around it:
        /// `sendra run x.yaml -o body | jq .`. The assertions/capture/summary
        /// output below the response is unaffected either way.
        ///
        /// Refused together with `--json`, which already carries the whole
        /// response regardless of what this would filter — a flag that
        /// looked like it worked but silently did nothing would be worse
        /// than an error.
        ///
        /// Under `--dry-run`, applies the same way to the printed resolved
        /// request instead: `full` is method/URL, headers and body (today's
        /// `--dry-run` output), `body`/`headers`/`none` show only that part,
        /// and `status` is refused — a dry run never has a status line.
        #[arg(short = 'o', long = "output", value_enum, value_name = "MODE")]
        output: Option<OutputMode>,

        /// Suppress everything that is not the answer to "did this work":
        /// the `→ <label>` lines (stderr) and the response rendering
        /// (stdout, as `-o none` — see `--output`). Assertions, `capture`
        /// results and the summary still print, since those are the actual
        /// pass/fail information `-q` exists to make easier to find, not
        /// something it hides.
        ///
        /// Implies `-o none`; combining `-q` with an explicit `-o <mode>`
        /// other than `none` is refused as a real conflict — quiet mode
        /// asked for no response, a specific mode asked for one, and
        /// guessing which one you meant is not a trade Sendra makes on your
        /// behalf. `-q -o none` is accepted, since the two agree.
        ///
        /// Has no effect on `--json`'s document, which is data rather than
        /// narration and carries the full response regardless — but it
        /// still suppresses the `→` labels `--json` prints to stderr
        /// alongside it, so `-q --json` is not meaningless and is not
        /// refused.
        ///
        /// Applies to `--dry-run` the same way it applies to a real
        /// response: the resolved request is not printed either.
        #[arg(short = 'q', long = "quiet")]
        quiet: bool,

        /// Print, to stderr and before any request runs, which config and
        /// environment files this invocation actually resolved: the project
        /// config path (or that none was found), the global config path (or
        /// that none was found), the environment selected by name and its
        /// file (or that none was found), and whether `--var`/`-H`
        /// overrides were passed.
        ///
        /// A fixed, specific report — not a general debug log. `--var`/`-H`
        /// entries are named, not valued: an override often carries a token
        /// or a password meant for a request field, and the name already
        /// answers the question `-v` exists to answer ("did my override
        /// apply?") without putting a secret on screen, in shell history, or
        /// in a captured CI log. `--dry-run` already exists, and does show
        /// values in full, for the case that needs them.
        ///
        /// Refused together with `-q`/`--quiet`: the two disagree about the
        /// same stream, one asking for more narration and one for less, and
        /// Sendra will not guess which one you meant.
        ///
        /// Unaffected by `--json`: the report is stderr-only regardless,
        /// since `--json`'s stdout contract is about the result a run
        /// produced, not about how the pipeline resolved to it.
        ///
        /// Works under `--dry-run` exactly as it does otherwise — both go
        /// through the same resolution, and `-v` reports on it before
        /// `--dry-run`'s own resolved-request output, which it leaves
        /// unchanged.
        #[arg(short = 'v', long = "verbose")]
        verbose: bool,
    },

    /// Run every request in a YAML file, or one named request, and pass or
    /// fail on its assertions.
    ///
    /// Sends the same requests `run` sends, under the same config and the same
    /// environment, and prints the same per-request assertion results — then a
    /// summary across the whole run, and an exit code decided by the
    /// assertions rather than by the response statuses. See `sendra help run`
    /// for the shared parts.
    Test {
        /// Path to the request or collection file.
        ///
        /// A single-request file and a collection are both accepted, and a
        /// collection runs every request in it, in file order, unless
        /// `request` names one to run alone.
        path: PathBuf,

        /// Name of one request to test, when the file is a collection.
        ///
        /// Behaves exactly as it does on `run`: omit it to test every request
        /// in the collection, in file order; naming a request that does not
        /// exist is an error; naming one in a file that holds a single
        /// request is an error, since there is nothing to choose between.
        request: Option<String>,

        /// Name of the environment to substitute `{{variable}}` values from.
        ///
        /// Behaves exactly as it does on `run`: `--env staging` loads
        /// `.sendra/environments/staging.yaml`, found by walking up from the
        /// directory you are in; omitting it loads `default` if there is one;
        /// naming an environment that has no file is an error.
        #[arg(long, value_name = "NAME")]
        env: Option<String>,

        /// Add or override a header for this invocation only, `Name: value`.
        /// Behaves exactly as it does on `run` — see `run --help` for the
        /// full reasoning and the repeated-`-H` rule.
        #[arg(short = 'H', long = "header", value_name = "NAME:VALUE", value_parser = parse_header_override)]
        header: Vec<(String, String)>,

        /// Set a substitution variable for this invocation only, `name=value`.
        /// Behaves exactly as it does on `run` — see `run --help`.
        #[arg(long = "var", value_name = "NAME=VALUE", value_parser = parse_var_override)]
        var: Vec<(String, String)>,

        /// Override the resolved timeout, in seconds, for this invocation
        /// only. Behaves exactly as it does on `run` — see `run --help`.
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<u64>,

        /// Repeat the entire run this many times, sequentially. Behaves
        /// exactly as it does on `run` — see `run --help` for the full
        /// reasoning, including the per-pass capture reset and the
        /// `(iteration N of M)` label suffix `--json`/`--junit` add — with
        /// one addition: the summary `test` prints counts every request in
        /// every pass, not just the last one.
        #[arg(long, value_name = "N", default_value_t = 1, value_parser = parse_repeat)]
        repeat: u32,

        /// Disable TLS certificate verification for this invocation only.
        /// Behaves exactly as it does on `run` — see `run --help` for the
        /// full reasoning, including the `-q`/`--json` interaction of the
        /// warning it prints whenever this resolves to true.
        #[arg(long)]
        insecure: bool,

        /// Route every request through this HTTP proxy for this invocation
        /// only. Behaves exactly as it does on `run` — see `run --help`.
        #[arg(long, value_name = "URL")]
        proxy: Option<String>,

        /// Present this client certificate for mutual TLS, for this
        /// invocation only. Behaves exactly as it does on `run` — see
        /// `run --help` for the full reasoning, including the
        /// `--client-key` pairing rule and the config-relative-vs-cwd-relative
        /// path resolution.
        #[arg(long, value_name = "PATH")]
        client_cert: Option<PathBuf>,

        /// Private key matching `--client-cert`, for this invocation only.
        /// Behaves exactly as it does on `run` — see `run --help`.
        #[arg(long, value_name = "PATH")]
        client_key: Option<PathBuf>,

        /// Print one JSON object describing the whole run, instead of the
        /// human-readable output.
        ///
        /// The same document `sendra run --json` writes — `requests`, in file
        /// order, each with its full response and assertion results — plus a
        /// `summary` object holding the counts the terminal output ends with.
        /// Note that `requests` carries whole responses here, headers and body
        /// included, where the terminal output shows only a status line: that
        /// is a decision about what is readable on a screen, and a program
        /// reading the output has no such problem.
        #[arg(long)]
        json: bool,

        /// Show captured values verbatim in `--json` output instead of
        /// redacting them. See `run --help` for the reasoning; the same
        /// default and the same flag apply here.
        #[arg(long)]
        show_captures: bool,

        /// Write a JUnit XML report to this path, in addition to the normal
        /// output.
        ///
        /// One `<testcase>` per request, named after its label. A request
        /// whose checks all held is a plain pass; one that failed a check —
        /// a failing assertion, a `post_request` throw, or a capture that
        /// produced nothing — carries a `<failure>` with every failure's
        /// message; one that never got a response carries an `<error>`
        /// instead, since the tool could not run the test rather than the
        /// test not holding; one that declared no `assertions` block and no
        /// `post_request` script is `<skipped>`. The `<testsuite>` counts
        /// match the summary this run ends with either way.
        ///
        /// Every major CI system (GitHub Actions, GitLab, Jenkins) renders
        /// JUnit XML natively as inline pass/fail annotations, which is what
        /// this is for — the terminal or `--json` output still happens, so
        /// this is additive rather than a replacement.
        #[arg(long, value_name = "PATH")]
        junit: Option<PathBuf>,

        /// Accepted only so that passing it can be refused with an
        /// explanation. Hidden from `--help`, rejected in `main`.
        #[arg(long, hide = true)]
        allow_error_status: bool,

        /// How much of the response the human-readable output shows: `full`,
        /// `status` (the default), `body`, `headers` or `none`. Behaves
        /// exactly as it does on `run` — see `run --help` for the full
        /// reasoning, including the `--json` refusal.
        #[arg(short = 'o', long = "output", value_enum, value_name = "MODE")]
        output: Option<OutputMode>,

        /// Suppress the `→ <label>` lines and the response rendering,
        /// leaving assertions/capture/summary — the pass/fail information —
        /// intact. Behaves exactly as it does on `run`, including the
        /// `-o`/`--dry-run` interactions; see `run --help`.
        #[arg(short = 'q', long = "quiet")]
        quiet: bool,

        /// Print, to stderr and before any request runs, which config and
        /// environment files this invocation actually resolved. Behaves
        /// exactly as it does on `run`, including the `-q` refusal and the
        /// `--json` and `--var`/`-H` reasoning; see `run --help`.
        #[arg(short = 'v', long = "verbose")]
        verbose: bool,
    },

    /// Scaffold `.sendra/config.yaml` and `.sendra/environments/default.yaml`
    /// in the current directory.
    ///
    /// Both files are written with every known field present but commented
    /// out, showing the shape a config and an environment file can take
    /// without imposing any values of its own. Refuses, without writing
    /// anything, if `.sendra/` already exists, rather than filling in
    /// whichever of the two files is missing.
    // TODO: point at the docs site here once it exists, instead of leaving
    // the reasoning implicit.
    Init,

    /// Write the JSON Schema files for request/collection/config/environment
    /// files into `<output>/schema/` (the current directory if `--output` is
    /// not given), for editor autocomplete and inline validation.
    ///
    /// The four files are baked into this binary at compile time, so this
    /// works with no network access and no clone of the Sendra repository —
    /// see the README's "JSON Schema / editor support" section for what each
    /// file covers and its known limits, and for the `yaml.schemas` settings
    /// this command's output is meant to be pointed at.
    ///
    /// Unlike `sendra init`, running this again overwrites rather than
    /// refusing: nothing under `schema/` is meant to hold anything of yours,
    /// so re-running after a `sendra` upgrade is the ordinary way to pick up
    /// a newer schema.
    Schema {
        /// Directory to write `schema/` under. Defaults to the current
        /// directory.
        #[arg(long, value_name = "DIR")]
        output: Option<PathBuf>,
    },

    /// Convert another tool's command into a Sendra request file.
    Import {
        #[command(subcommand)]
        target: ImportTarget,
    },
}

/// What `sendra import` can convert. Its own enum, nested under `Command`,
/// so a second source format can join `Curl` later without every existing
/// `Command` match arm having to learn about it.
#[derive(Subcommand)]
pub(crate) enum ImportTarget {
    /// Convert a curl command line into a Sendra request file.
    ///
    /// Covers the common flags — `-X`, `-H`, `-d`/`--data`/`--data-raw`/
    /// `--data-binary`, `-u`, `-F`, `-b`, `-A` and the URL itself — and
    /// prints a plain-text summary, to stderr, of anything it could not
    /// convert: a flag with no per-request equivalent (`-k`/`--insecure`,
    /// `-x`/`--proxy`, pointing at the `sendra run` flag that matches it) or
    /// a flag this command does not know at all. curl's enormous flag
    /// surface is not fully covered — this is a useful, honest, best-effort
    /// converter, not a complete one.
    Curl {
        /// The curl command to convert, as a single shell-quoted string —
        /// e.g. `sendra import curl 'curl -X POST https://api.example.com
        /// -H "Accept: application/json"'`.
        ///
        /// Omit it to read the command from stdin instead, for a pipe-based
        /// workflow: `pbpaste | sendra import curl`.
        command: Option<String>,

        /// Write the generated YAML to this path instead of stdout.
        ///
        /// Omit it to write to stdout, which composes with shell
        /// redirection: `sendra import curl '...' > request.yaml`.
        #[arg(short = 'o', long = "output", value_name = "PATH")]
        output: Option<PathBuf>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- `--allow-error-status` has no meaning under `test` ---------------

    #[test]
    fn the_cli_definition_is_internally_consistent() {
        use clap::CommandFactory;

        // clap's own check that the two subcommands' arguments are well-formed
        // — cheap, and it catches a duplicated long name or a bad default the
        // moment it is written rather than the first time someone runs the
        // command.
        Cli::command().debug_assert();
    }

    #[test]
    fn test_accepts_allow_error_status_only_so_that_it_can_be_refused() {
        // Not defining the flag at all would also reject it, with clap's
        // generic "unexpected argument". It is defined and hidden so that the
        // refusal can say *why* it does not apply — see
        // `reject_allow_error_status`.
        let cli = Cli::try_parse_from(["sendra", "test", "req.yaml", "--allow-error-status"])
            .expect("the flag must parse, so `main` can refuse it with an explanation");

        match cli.command {
            Command::Test {
                allow_error_status, ..
            } => assert!(
                allow_error_status,
                "the flag must reach `main` to be refused"
            ),
            _ => panic!("`sendra test` should have parsed as `Command::Test`"),
        }
    }

    #[test]
    fn allow_error_status_is_advertised_by_run_and_hidden_by_test() {
        use clap::CommandFactory;

        let mut cli = Cli::command();

        let run_help = cli
            .find_subcommand_mut("run")
            .expect("`run` is a subcommand")
            .render_help()
            .to_string();
        assert!(
            run_help.contains("--allow-error-status"),
            "`run` still offers the flag"
        );

        let test_help = cli
            .find_subcommand_mut("test")
            .expect("`test` is a subcommand")
            .render_help()
            .to_string();
        assert!(
            !test_help.contains("--allow-error-status"),
            "`test` must not offer a flag it refuses: {test_help}"
        );
    }

    #[test]
    fn test_takes_a_path_and_an_env_and_an_optional_request_name() {
        let cli = Cli::try_parse_from(["sendra", "test", "collection.yaml", "--env", "staging"])
            .expect("path and --env are the whole surface when no name is given");

        match cli.command {
            Command::Test {
                path,
                request,
                env,
                header,
                var,
                timeout,
                repeat,
                insecure,
                proxy,
                client_cert,
                client_key,
                json,
                show_captures,
                junit,
                allow_error_status,
                output,
                quiet,
                verbose,
            } => {
                assert_eq!(path, PathBuf::from("collection.yaml"));
                assert_eq!(request, None, "no request name was passed");
                assert_eq!(env.as_deref(), Some("staging"));
                assert!(header.is_empty(), "no -H was passed");
                assert!(var.is_empty(), "no --var was passed");
                assert_eq!(timeout, None, "no --timeout was passed");
                assert_eq!(repeat, 1, "no --repeat was passed");
                assert!(!insecure, "no --insecure was passed");
                assert_eq!(proxy, None, "no --proxy was passed");
                assert_eq!(client_cert, None, "no --client-cert was passed");
                assert_eq!(client_key, None, "no --client-key was passed");
                assert!(!json, "the human output is what you get without --json");
                assert!(!show_captures, "captures are redacted by default");
                assert_eq!(junit, None, "no --junit was passed");
                assert!(!allow_error_status);
                assert_eq!(output, None, "no -o was passed");
                assert!(!quiet, "no -q was passed");
                assert!(!verbose, "no -v was passed");
            }
            _ => panic!("`sendra test` should have parsed as `Command::Test`"),
        }

        // Whether the flag was passed is all `main` needs from it; what it
        // then means is `Reporter`'s.
        let cli = Cli::try_parse_from(["sendra", "test", "collection.yaml", "--json"])
            .expect("`--json` is offered by `test` as well as by `run`");
        assert!(
            matches!(cli.command, Command::Test { json: true, .. }),
            "`--json` must reach `main`"
        );

        // A second positional is now `test`'s too, mirroring `run`: a verdict
        // over one hand-picked request.
        let cli = Cli::try_parse_from(["sendra", "test", "collection.yaml", "One request"])
            .expect("`test` now takes a request name, like `run`");
        assert!(
            matches!(cli.command, Command::Test { request: Some(ref name), .. } if name == "One request"),
            "the request name must reach `main`"
        );
    }

    // --- `--show-captures` -------------------------------------------------

    #[test]
    fn show_captures_defaults_to_false_and_is_offered_by_both_subcommands() {
        let cli =
            Cli::try_parse_from(["sendra", "run", "req.yaml"]).expect("`run` takes just a path");
        assert!(
            matches!(
                cli.command,
                Command::Run {
                    show_captures: false,
                    ..
                }
            ),
            "captures are redacted by default under `run`"
        );

        let cli = Cli::try_parse_from(["sendra", "run", "req.yaml", "--show-captures"])
            .expect("`--show-captures` is offered by `run`");
        assert!(matches!(
            cli.command,
            Command::Run {
                show_captures: true,
                ..
            }
        ));

        let cli = Cli::try_parse_from(["sendra", "test", "req.yaml", "--show-captures"])
            .expect("`--show-captures` is offered by `test`");
        assert!(matches!(
            cli.command,
            Command::Test {
                show_captures: true,
                ..
            }
        ));
    }

    // --- `--junit` -----------------------------------------------------------

    #[test]
    fn junit_is_optional_and_offered_by_test_only() {
        let cli =
            Cli::try_parse_from(["sendra", "test", "req.yaml"]).expect("`--junit` is optional");
        assert!(matches!(cli.command, Command::Test { junit: None, .. }));

        let cli = Cli::try_parse_from(["sendra", "test", "req.yaml", "--junit", "report.xml"])
            .expect("`--junit` takes a path");
        assert!(matches!(
            cli.command,
            Command::Test { junit: Some(ref path), .. } if path == &PathBuf::from("report.xml")
        ));

        // `run` produces no verdict, so it has nothing for a JUnit report to
        // say — the flag is deliberately `test`'s alone.
        let err = expect_cli_error(&["sendra", "run", "req.yaml", "--junit", "report.xml"]);
        assert_eq!(err.exit_code(), 2, "`run --junit` is a usage error");
    }

    // --- `-H`/`--header` ----------------------------------------------------

    #[test]
    fn header_is_repeatable_and_offered_by_both_subcommands() {
        let cli = Cli::try_parse_from([
            "sendra",
            "run",
            "req.yaml",
            "-H",
            "X-Trace-Id: abc",
            "--header",
            "Accept: application/json",
        ])
        .expect("`-H`/`--header` are the same flag, and repeat");

        match cli.command {
            Command::Run { header, .. } => assert_eq!(
                header,
                vec![
                    ("X-Trace-Id".to_string(), "abc".to_string()),
                    ("Accept".to_string(), "application/json".to_string()),
                ],
                "both occurrences must reach `main`, in order"
            ),
            _ => panic!("`sendra run` should have parsed as `Command::Run`"),
        }

        let cli = Cli::try_parse_from(["sendra", "test", "req.yaml", "-H", "X-Trace-Id: abc"])
            .expect("`-H` is offered by `test` too");
        assert!(matches!(
            cli.command,
            Command::Test { header, .. } if header == vec![("X-Trace-Id".to_string(), "abc".to_string())]
        ));
    }

    #[test]
    fn header_trims_the_space_after_the_colon_but_not_the_name() {
        let cli = Cli::try_parse_from(["sendra", "run", "req.yaml", "-H", "X-Trace-Id:   abc  "])
            .expect("a trailing/leading space around the value is not an error");
        match cli.command {
            Command::Run { header, .. } => {
                assert_eq!(header, vec![("X-Trace-Id".to_string(), "abc".to_string())]);
            }
            _ => panic!("`sendra run` should have parsed as `Command::Run`"),
        }
    }

    /// `Cli` derives no `Debug`, so `Result::expect_err`/`unwrap_err` (which
    /// both require it on the `Ok` side) cannot be used against
    /// `Cli::try_parse_from` directly; this unwraps by hand instead.
    fn expect_cli_error(args: &[&str]) -> clap::Error {
        match Cli::try_parse_from(args) {
            Ok(_) => panic!("expected a CLI error parsing {args:?}"),
            Err(err) => err,
        }
    }

    #[test]
    fn a_malformed_header_is_a_clear_cli_error() {
        let err = expect_cli_error(&["sendra", "run", "req.yaml", "-H", "no-colon-here"]);
        assert_eq!(
            err.exit_code(),
            2,
            "clap's usage-error exit code, same as any other bad argument"
        );
        assert!(
            err.to_string().contains("no `:` found"),
            "the message should say what is wrong: {err}"
        );

        let err = expect_cli_error(&["sendra", "run", "req.yaml", "-H", ": no name"]);
        assert!(err.to_string().contains("header name is empty"));
    }

    // --- `--var` -------------------------------------------------------------

    #[test]
    fn var_is_repeatable_and_offered_by_both_subcommands() {
        let cli = Cli::try_parse_from([
            "sendra",
            "run",
            "req.yaml",
            "--var",
            "base_url=https://example.com",
            "--var",
            "token=abc123",
        ])
        .expect("`--var` repeats");

        match cli.command {
            Command::Run { var, .. } => assert_eq!(
                var,
                vec![
                    ("base_url".to_string(), "https://example.com".to_string()),
                    ("token".to_string(), "abc123".to_string()),
                ]
            ),
            _ => panic!("`sendra run` should have parsed as `Command::Run`"),
        }

        let cli = Cli::try_parse_from(["sendra", "test", "req.yaml", "--var", "token=abc123"])
            .expect("`--var` is offered by `test` too");
        assert!(matches!(
            cli.command,
            Command::Test { var, .. } if var == vec![("token".to_string(), "abc123".to_string())]
        ));
    }

    #[test]
    fn a_var_value_may_contain_an_equals_sign() {
        // Only the first `=` is the separator — a value that is itself a
        // `key=value` pair (a query string, say) must survive intact.
        let cli = Cli::try_parse_from(["sendra", "run", "req.yaml", "--var", "q=a=b"])
            .expect("only the first `=` splits name from value");
        match cli.command {
            Command::Run { var, .. } => {
                assert_eq!(var, vec![("q".to_string(), "a=b".to_string())]);
            }
            _ => panic!("`sendra run` should have parsed as `Command::Run`"),
        }
    }

    #[test]
    fn a_malformed_var_is_a_clear_cli_error() {
        let err = expect_cli_error(&["sendra", "run", "req.yaml", "--var", "no-equals-here"]);
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("no `=` found"));

        let err = expect_cli_error(&["sendra", "run", "req.yaml", "--var", "=novalue"]);
        assert!(err.to_string().contains("variable name is empty"));
    }

    // --- `--dry-run` -----------------------------------------------------

    #[test]
    fn dry_run_defaults_to_false_and_is_offered_only_by_run() {
        let cli = Cli::try_parse_from(["sendra", "run", "req.yaml"]).expect("`run` takes a path");
        assert!(matches!(cli.command, Command::Run { dry_run: false, .. }));

        let cli = Cli::try_parse_from(["sendra", "run", "req.yaml", "--dry-run"])
            .expect("`--dry-run` is offered by `run`");
        assert!(matches!(cli.command, Command::Run { dry_run: true, .. }));

        // `test` never offers it: a "dry test" has no response to check
        // expectations against, so the flag is `run`-only. Clap's ordinary
        // unknown-argument error is enough of an explanation here — unlike
        // `--allow-error-status` on `test`, this was never a flag `test`
        // advertised and then had to explain away.
        let err = expect_cli_error(&["sendra", "test", "req.yaml", "--dry-run"]);
        assert_eq!(err.exit_code(), 2);
    }

    // --- `-o`/`--output` ---------------------------------------------------

    #[test]
    fn output_defaults_to_none_and_is_offered_by_both_subcommands() {
        let cli =
            Cli::try_parse_from(["sendra", "run", "req.yaml"]).expect("`-o` is optional on `run`");
        assert!(matches!(cli.command, Command::Run { output: None, .. }));

        let cli = Cli::try_parse_from(["sendra", "test", "req.yaml"])
            .expect("`-o` is optional on `test`");
        assert!(matches!(cli.command, Command::Test { output: None, .. }));

        for (flag, mode) in [
            ("-o", OutputMode::Full),
            ("--output", OutputMode::Status),
            ("-o", OutputMode::Body),
            ("-o", OutputMode::Headers),
            ("-o", OutputMode::None),
        ] {
            let value = match mode {
                OutputMode::Full => "full",
                OutputMode::Status => "status",
                OutputMode::Body => "body",
                OutputMode::Headers => "headers",
                OutputMode::None => "none",
            };

            let cli = Cli::try_parse_from(["sendra", "run", "req.yaml", flag, value])
                .unwrap_or_else(|err| panic!("`{flag} {value}` should parse on `run`: {err}"));
            assert!(matches!(
                cli.command,
                Command::Run { output: Some(got), .. } if got == mode
            ));

            let cli = Cli::try_parse_from(["sendra", "test", "req.yaml", flag, value])
                .unwrap_or_else(|err| panic!("`{flag} {value}` should parse on `test`: {err}"));
            assert!(matches!(
                cli.command,
                Command::Test { output: Some(got), .. } if got == mode
            ));
        }
    }

    #[test]
    fn an_invalid_output_mode_is_a_clear_cli_error() {
        let err = expect_cli_error(&["sendra", "run", "req.yaml", "-o", "bogus"]);
        assert_eq!(err.exit_code(), 2);
        assert!(
            err.to_string().contains("bogus"),
            "clap should name the bad value: {err}"
        );
    }

    // --- `-q`/`--quiet` ----------------------------------------------------

    #[test]
    fn quiet_defaults_to_false_and_is_offered_by_both_subcommands() {
        let cli = Cli::try_parse_from(["sendra", "run", "req.yaml"]).expect("`-q` is optional");
        assert!(matches!(cli.command, Command::Run { quiet: false, .. }));

        let cli = Cli::try_parse_from(["sendra", "run", "req.yaml", "-q"])
            .expect("`-q` is offered by `run`");
        assert!(matches!(cli.command, Command::Run { quiet: true, .. }));

        let cli = Cli::try_parse_from(["sendra", "test", "req.yaml", "--quiet"])
            .expect("`--quiet` is offered by `test`");
        assert!(matches!(cli.command, Command::Test { quiet: true, .. }));
    }

    // --- `-v`/`--verbose` ----------------------------------------------------

    #[test]
    fn verbose_defaults_to_false_and_is_offered_by_both_subcommands() {
        let cli = Cli::try_parse_from(["sendra", "run", "req.yaml"]).expect("`-v` is optional");
        assert!(matches!(cli.command, Command::Run { verbose: false, .. }));

        let cli = Cli::try_parse_from(["sendra", "run", "req.yaml", "-v"])
            .expect("`-v` is offered by `run`");
        assert!(matches!(cli.command, Command::Run { verbose: true, .. }));

        let cli = Cli::try_parse_from(["sendra", "test", "req.yaml", "--verbose"])
            .expect("`--verbose` is offered by `test`");
        assert!(matches!(cli.command, Command::Test { verbose: true, .. }));
    }

    // --- `--timeout` -----------------------------------------------------

    #[test]
    fn timeout_is_an_optional_number_of_seconds_offered_by_both_subcommands() {
        let cli =
            Cli::try_parse_from(["sendra", "run", "req.yaml"]).expect("`--timeout` is optional");
        assert!(matches!(cli.command, Command::Run { timeout: None, .. }));

        let cli = Cli::try_parse_from(["sendra", "run", "req.yaml", "--timeout", "5"])
            .expect("`--timeout` takes a number of seconds");
        assert!(matches!(
            cli.command,
            Command::Run {
                timeout: Some(5),
                ..
            }
        ));

        let cli = Cli::try_parse_from(["sendra", "test", "req.yaml", "--timeout", "5"])
            .expect("`--timeout` is offered by `test` too");
        assert!(matches!(
            cli.command,
            Command::Test {
                timeout: Some(5),
                ..
            }
        ));

        assert!(
            Cli::try_parse_from(["sendra", "run", "req.yaml", "--timeout", "not-a-number"])
                .is_err(),
            "a non-numeric timeout is a clap-level error"
        );
    }

    // --- `--repeat` --------------------------------------------------------

    #[test]
    fn repeat_defaults_to_one_and_is_offered_by_both_subcommands() {
        let cli =
            Cli::try_parse_from(["sendra", "run", "req.yaml"]).expect("`--repeat` is optional");
        assert!(matches!(cli.command, Command::Run { repeat: 1, .. }));

        let cli = Cli::try_parse_from(["sendra", "test", "req.yaml"])
            .expect("`--repeat` is optional on `test`");
        assert!(matches!(cli.command, Command::Test { repeat: 1, .. }));

        let cli = Cli::try_parse_from(["sendra", "run", "req.yaml", "--repeat", "3"])
            .expect("`--repeat` takes a count");
        assert!(matches!(cli.command, Command::Run { repeat: 3, .. }));

        let cli = Cli::try_parse_from(["sendra", "test", "req.yaml", "--repeat", "5"])
            .expect("`--repeat` is offered by `test` too");
        assert!(matches!(cli.command, Command::Test { repeat: 5, .. }));
    }

    #[test]
    fn repeat_zero_is_a_clear_cli_error() {
        let err = expect_cli_error(&["sendra", "run", "req.yaml", "--repeat", "0"]);
        assert_eq!(err.exit_code(), 2);
        assert!(
            err.to_string().contains("at least 1"),
            "the message should say why: {err}"
        );
    }

    #[test]
    fn a_non_numeric_repeat_is_a_clear_cli_error() {
        let err = expect_cli_error(&["sendra", "run", "req.yaml", "--repeat", "many"]);
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("positive integer"));
    }

    // --- `--insecure` --------------------------------------------------------

    #[test]
    fn insecure_defaults_to_false_and_is_offered_by_both_subcommands() {
        let cli =
            Cli::try_parse_from(["sendra", "run", "req.yaml"]).expect("`--insecure` is optional");
        assert!(matches!(
            cli.command,
            Command::Run {
                insecure: false,
                ..
            }
        ));

        let cli = Cli::try_parse_from(["sendra", "run", "req.yaml", "--insecure"])
            .expect("`--insecure` is offered by `run`");
        assert!(matches!(cli.command, Command::Run { insecure: true, .. }));

        let cli = Cli::try_parse_from(["sendra", "test", "req.yaml", "--insecure"])
            .expect("`--insecure` is offered by `test`");
        assert!(matches!(cli.command, Command::Test { insecure: true, .. }));
    }

    // --- `--proxy` -------------------------------------------------------------

    #[test]
    fn proxy_defaults_to_none_and_is_offered_by_both_subcommands() {
        let cli =
            Cli::try_parse_from(["sendra", "run", "req.yaml"]).expect("`--proxy` is optional");
        assert!(matches!(cli.command, Command::Run { proxy: None, .. }));

        let cli = Cli::try_parse_from([
            "sendra",
            "run",
            "req.yaml",
            "--proxy",
            "http://proxy.example.com:8080",
        ])
        .expect("`--proxy` takes a URL");
        assert!(matches!(
            cli.command,
            Command::Run { proxy: Some(ref url), .. } if url == "http://proxy.example.com:8080"
        ));

        let cli = Cli::try_parse_from([
            "sendra",
            "test",
            "req.yaml",
            "--proxy",
            "http://proxy.example.com:8080",
        ])
        .expect("`--proxy` is offered by `test` too");
        assert!(matches!(
            cli.command,
            Command::Test { proxy: Some(ref url), .. } if url == "http://proxy.example.com:8080"
        ));
    }

    // --- `--client-cert`/`--client-key` --------------------------------------

    #[test]
    fn client_cert_and_client_key_default_to_none_and_are_offered_by_both_subcommands() {
        let cli =
            Cli::try_parse_from(["sendra", "run", "req.yaml"]).expect("both flags are optional");
        assert!(matches!(
            cli.command,
            Command::Run {
                client_cert: None,
                client_key: None,
                ..
            }
        ));

        let cli = Cli::try_parse_from([
            "sendra",
            "run",
            "req.yaml",
            "--client-cert",
            "client.pem",
            "--client-key",
            "client-key.pem",
        ])
        .expect("`--client-cert`/`--client-key` take a path each");
        assert!(matches!(
            cli.command,
            Command::Run {
                client_cert: Some(ref cert),
                client_key: Some(ref key),
                ..
            } if cert == &PathBuf::from("client.pem") && key == &PathBuf::from("client-key.pem")
        ));

        let cli = Cli::try_parse_from([
            "sendra",
            "test",
            "req.yaml",
            "--client-cert",
            "client.pem",
            "--client-key",
            "client-key.pem",
        ])
        .expect("`--client-cert`/`--client-key` are offered by `test` too");
        assert!(matches!(
            cli.command,
            Command::Test {
                client_cert: Some(ref cert),
                client_key: Some(ref key),
                ..
            } if cert == &PathBuf::from("client.pem") && key == &PathBuf::from("client-key.pem")
        ));
    }

    #[test]
    fn client_cert_and_client_key_may_be_passed_independently() {
        // Each flag overrides only its own half of the config's client
        // certificate — see `Config::client_cert`'s doc comment — so parsing
        // must not require the other to be present.
        let cli = Cli::try_parse_from(["sendra", "run", "req.yaml", "--client-cert", "c.pem"])
            .expect("`--client-cert` alone must parse");
        assert!(matches!(
            cli.command,
            Command::Run {
                client_cert: Some(ref cert),
                client_key: None,
                ..
            } if cert == &PathBuf::from("c.pem")
        ));

        let cli = Cli::try_parse_from(["sendra", "run", "req.yaml", "--client-key", "k.pem"])
            .expect("`--client-key` alone must parse");
        assert!(matches!(
            cli.command,
            Command::Run {
                client_cert: None,
                client_key: Some(ref key),
                ..
            } if key == &PathBuf::from("k.pem")
        ));
    }

    #[test]
    fn a_proxy_url_with_credentials_parses_as_one_opaque_string() {
        // Not validated or split apart at the clap level — see `Config::proxy`
        // for why: reqwest reads the credentials straight out of the URL.
        let cli = Cli::try_parse_from([
            "sendra",
            "run",
            "req.yaml",
            "--proxy",
            "http://user:pass@proxy.example.com:8080",
        ])
        .expect("credentials embedded in the URL are not rejected here");
        assert!(matches!(
            cli.command,
            Command::Run { proxy: Some(ref url), .. }
                if url == "http://user:pass@proxy.example.com:8080"
        ));
    }
}
