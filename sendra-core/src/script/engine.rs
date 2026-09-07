//! The engine itself: the sandbox it is built with, and the stdout capture
//! that stands in for `println!`, which this crate never calls.

use std::cell::RefCell;

use rhai::Engine;

use super::ScriptOutput;

/// How many Rhai operations one script may execute before it is stopped.
///
/// Present for the reason the config always supplies an HTTP timeout: without
/// one, a mistake hangs the process with no output and Ctrl-C as the only way
/// out, while with one it gets a named error saying what happened. This is not
/// a defence against a hostile script — scripts come out of the user's own
/// request files — it is a backstop for a `while true` nobody meant to write.
///
/// Ten million is generous in the direction that matters and cheap in the other.
/// A hook that walks a megabyte-sized response body a character at a time costs
/// a few million operations, so a real script does not come close; a runaway one
/// stops in about two seconds rather than never. The number is a wall, not a
/// budget: if a legitimate script ever hits it, raise it, because a request hook
/// needing more than this is doing something worth looking at either way.
const MAX_OPERATIONS: u64 = 10_000_000;

thread_local! {
    /// One engine per thread, built once.
    ///
    /// Safe to share across every script in a run because it holds no state
    /// that a script can reach: it is configured at construction and never
    /// mutated afterwards, and each run gets a fresh [`Scope`](rhai::Scope).
    /// Per-thread rather than a `static` because a Rhai [`Engine`] is
    /// deliberately not `Sync` unless built with its `sync` feature, which
    /// costs every value an atomic refcount for a binary that runs on one
    /// thread.
    pub(super) static ENGINE: Engine = build_engine();

    /// Where the engine's `print` and `debug` handlers put their lines, until
    /// [`capture`] takes them.
    ///
    /// A scratch buffer, not state: [`capture`] empties it before a run and
    /// takes everything out of it afterwards, so nothing is ever carried from
    /// one script to the next and nothing outside this file can observe it.
    ///
    /// It exists because Rhai's `on_print` takes a `'static` callback, so the
    /// handler cannot borrow a sink the caller passed in. Somewhere has to hold
    /// the lines between the engine producing them and this module handing them
    /// over, and a per-thread buffer beside the per-thread engine is the
    /// smallest thing that does — the alternative, rebuilding the engine for
    /// every script so its handlers could own an `Rc` to a fresh buffer, throws
    /// away the caching for no gain the caller can see.
    static OUTPUT: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Run `body` against the shared engine, collecting whatever it printed.
///
/// The one place a script's output is gathered, so that neither entry point can
/// forget to drain the buffer and leak lines into the next script's output.
/// Reentrancy is not a concern: a script cannot call back into this module,
/// because nothing from this module is registered into the engine.
pub(super) fn capture<T>(body: impl FnOnce(&Engine) -> T) -> (T, ScriptOutput) {
    OUTPUT.with(|output| output.borrow_mut().clear());

    let result = ENGINE.with(body);

    let lines = OUTPUT.with(|output| std::mem::take(&mut *output.borrow_mut()));

    (result, ScriptOutput { lines })
}

/// Build the one engine every script runs in.
///
/// # What a script can reach
///
/// Everything a script can do comes from Rhai's own language and standard
/// package, minus what is turned off below. **Sendra registers no functions,
/// no types, no packages and no modules of its own** — the entire Sendra-shaped
/// surface is two variables in a scope (`request`, `response`), both plain
/// object maps of strings, integers and arrays. There is nothing to audit for
/// filesystem or network access because there is nothing registered.
///
/// Rhai's standard package is arithmetic, strings, arrays, object maps,
/// ranges, bit operations, `timestamp()` and maths. It opens no files, no
/// sockets and no processes; Rhai does not expose any of those to scripts
/// unless a host registers them, which is exactly what this function does not
/// do. `timestamp()` reads the system clock, which is ambient but not a
/// capability the process did not already have.
///
/// # What is turned off
///
/// - **`import`, and the module system with it**, at compile time, via the
///   `no_module` feature in `Cargo.toml`. This matters more than it looks:
///   Rhai's *default* module resolver is `FileModuleResolver`, which reads
///   `.rhai` files off disk, so `import "…"` is the one filesystem path a
///   stock `Engine::new()` does have. Turning the feature off removes the
///   syntax and the resolver both, rather than relying on remembering to
///   replace the resolver at runtime.
/// - **`eval`**, which evaluates a string as script. Not a filesystem or
///   network capability, but it defeats the property that a script's syntax is
///   checked before the request is sent, and there is no use for it here: no
///   script source is templated, so there is nothing to build a program out of
///   at runtime.
///
/// # Where `print` goes
///
/// Into a [`ScriptOutput`], handed back to whoever called the script, and
/// nowhere else. Rhai's default handler writes to stdout; this crate must not
/// write anywhere at all, so both handlers append to [`OUTPUT`] and [`capture`]
/// carries the lines out with the result.
///
/// Discarding the lines was the alternative and is worse — a silently ignored
/// input is the thing this codebase refuses everywhere else — and writing them
/// somewhere chosen here is worse still: stdout would land inside the single
/// JSON document `--json` promises, and even stderr is a decision that belongs
/// to whatever is drawing the screen. `sendra-cli` prints them to stderr,
/// beside the `→` labels; a `sendra-tui` will put them somewhere a redrawn
/// frame does not wipe out.
///
/// A `debug` line is formatted here, with its source and position, because that
/// is Rhai's information to render rather than something a front-end should
/// have to reconstruct from parts.
fn build_engine() -> Engine {
    let mut engine = Engine::new();

    engine.on_print(|text| push_output(text.to_string()));
    engine.on_debug(|text, source, position| {
        push_output(match source {
            Some(source) => format!("{source} @ {position:?}: {text}"),
            None => format!("{position:?}: {text}"),
        })
    });

    engine.disable_symbol("eval");
    engine.set_max_operations(MAX_OPERATIONS);

    engine
}

/// Add one line to the buffer [`capture`] is about to drain.
fn push_output(line: String) {
    OUTPUT.with(|output| output.borrow_mut().push(line));
}

/// The message a failed script run reports.
///
/// A `throw "expected 201"` is reported as `expected 201` and nothing else:
/// that is a sentence the script author wrote to be read, and wrapping it in
/// `Runtime error: … (line 2, position 5)` would bury it. Every *other* runtime
/// failure — a method that does not exist on a string, an index off the end of
/// the headers array — keeps Rhai's full message, position included, because
/// there the position is the whole point: it is a bug in the script, and the
/// reader needs the line.
///
/// This is the only place the two are told apart, and it is told apart for
/// *wording*, not for categorisation. A thrown expectation and a script bug are
/// both `Failed`, and both count the same in a summary; see the note on
/// [`ScriptOutcome`](super::ScriptOutcome) and the CLI's `exit` module for why
/// one reliable split (compile versus run) is preferred to a second, guessable
/// one.
pub(super) fn failure_message(err: &rhai::EvalAltResult) -> String {
    match err {
        rhai::EvalAltResult::ErrorRuntime(value, _) => value.to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{
        post, pre, response, run_pre, with_post_request, with_pre_request,
    };
    use crate::{ScriptOutcome, Scripts, SendraError};

    // --- the sandbox --------------------------------------------------------

    #[test]
    fn a_script_cannot_import_a_module() {
        // The one filesystem path a stock Rhai engine has. Removed at compile
        // time by the `no_module` feature, so this is a *parse* error: the
        // syntax does not exist, rather than existing and being refused.
        let request = with_pre_request(r#"import "os" as os;"#);
        let err = Scripts::compile(&request).expect_err("`import` must not exist");

        assert!(matches!(err, SendraError::ScriptParse { .. }), "{err:?}");
    }

    #[test]
    fn a_script_cannot_eval_a_string() {
        let request = with_pre_request(r#"eval("1 + 1");"#);
        let err = Scripts::compile(&request).expect_err("`eval` is disabled");

        assert!(matches!(err, SendraError::ScriptParse { .. }), "{err:?}");
    }

    #[test]
    fn nothing_that_touches_the_filesystem_or_the_network_is_reachable() {
        // Sendra registers nothing into the engine, so the check that matters
        // is that the names one would reach for resolve to nothing at all.
        //
        // Two things count as "nothing", and both are accepted here: a
        // `ScriptParse` error, which is Rhai refusing the name outright (some
        // of these are words it reserves), and a `ScriptFailed` naming a
        // function it could not find, which is what an unregistered function
        // looks like from inside a script. What must never happen is `Ok`.
        for attempt in [
            r#"open_file("/etc/passwd");"#,
            r#"read_file("/etc/passwd");"#,
            r#"http_get("https://example.com");"#,
            r#"fetch("https://example.com");"#,
            r#"system("ls");"#,
            r#"exec("ls");"#,
            r#"spawn("ls");"#,
            r#"env("HOME");"#,
            r#"read_dir(".");"#,
        ] {
            let request = with_pre_request(attempt);
            let err =
                run_pre(&request).expect_err(&format!("`{attempt}` must not resolve to anything"));

            match &err {
                SendraError::ScriptParse { .. } => {}
                SendraError::ScriptFailed { message, .. } => assert!(
                    message.contains("not found") || message.contains("Function"),
                    "`{attempt}` failed with {message}, which does not read as \"no such function\""
                ),
                other => panic!("`{attempt}` produced {other:?}"),
            }
        }
    }

    #[test]
    fn a_runaway_script_is_stopped_rather_than_hanging_the_process() {
        let request = with_pre_request("let n = 0; while true { n += 1; }");
        let err = run_pre(&request).expect_err("an infinite loop must be stopped");

        assert!(matches!(err, SendraError::ScriptFailed { .. }), "{err:?}");
    }

    // --- what a script prints ------------------------------------------------
    //
    // `sendra-core` writes to no stream at all, so `print` and `debug` come
    // back as data and the front-end decides where they go. These pin the
    // collecting; where the CLI puts them is `Reporter::script_output`.

    #[test]
    fn a_script_that_prints_nothing_produces_no_output() {
        // The usual case, and the one a front-end should be able to skip
        // without allocating.
        let request = with_pre_request(r#"request.headers["X"] = "y";"#);
        let (result, output) = pre(&request).unwrap();

        assert!(result.is_ok());
        assert!(output.is_empty());
        assert_eq!(output.lines(), &[] as &[String]);
    }

    #[test]
    fn print_is_collected_one_line_at_a_time_in_order() {
        let request = with_pre_request(
            "print(\"first\");\nprint(\"second \" + 40 + 2);\nprint(request.method);",
        );
        let (result, output) = pre(&request).unwrap();

        assert!(result.is_ok());
        assert_eq!(output.lines(), ["first", "second 402", "POST"]);
    }

    #[test]
    fn debug_carries_its_position_the_way_rhai_renders_it() {
        // The formatting is Rhai's information, so it is applied where the
        // engine's handler is set rather than left for a front-end to
        // reconstruct from parts it was handed separately. `1:1` is Rhai's
        // `Position` as line:column, and the value is debug-formatted, which is
        // why the string arrives quoted — this is byte-for-byte what the old
        // `eprintln!("{position:?}: {text}")` produced.
        let request = with_pre_request(r#"debug("looking");"#);
        let (_, output) = pre(&request).unwrap();

        assert_eq!(output.lines(), [r#"1:1: "looking""#]);
    }

    #[test]
    fn output_comes_back_even_when_the_script_throws() {
        // The reason `ScriptOutput` sits beside the `Result` rather than inside
        // its `Ok`: the lines printed before a throw are usually the ones that
        // explain it, and the error path is exactly when they are worth most.
        let request = with_pre_request("print(\"about to give up\");\nthrow \"no key\";");
        let (result, output) = pre(&request).unwrap();

        assert!(result.is_err(), "the script threw");
        assert_eq!(output.lines(), ["about to give up"]);
    }

    #[test]
    fn a_post_request_script_reports_what_it_printed_alongside_its_verdict() {
        let request = with_post_request(
            "print(\"status was \" + response.status);\nthrow \"not good enough\";",
        );
        let (outcome, output) = post(&request, &response());

        assert_eq!(
            outcome,
            ScriptOutcome::Failed {
                message: "not good enough".to_string()
            }
        );
        assert_eq!(output.lines(), ["status was 201"]);
    }

    #[test]
    fn one_script_never_sees_another_script_s_output() {
        // The buffer behind `capture` is scratch space, not state: it is
        // emptied going in and drained coming out, so a run cannot inherit
        // lines from the run before it.
        let noisy = with_pre_request(r#"print("from the first");"#);
        let (_, first) = pre(&noisy).unwrap();
        assert_eq!(first.lines(), ["from the first"]);

        let quiet = with_pre_request(r#"request.url = request.url;"#);
        let (_, second) = pre(&quiet).unwrap();
        assert!(
            second.is_empty(),
            "the second script printed nothing, but got {:?}",
            second.lines()
        );

        // And a third that prints starts from empty rather than appending.
        let noisy_again = with_pre_request(r#"print("from the third");"#);
        let (_, third) = pre(&noisy_again).unwrap();
        assert_eq!(third.lines(), ["from the third"]);
    }
}
