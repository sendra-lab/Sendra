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
/// package, plus four functions registered below, minus what is turned off
/// further down. **Sendra registers no types, no packages and no modules of
/// its own — only four plain functions**, chosen and checked one at a time
/// against a single rule:
///
/// > A function may be registered here only if it is a closed computation —
/// > it can open no file, make no network connection, spawn or signal no
/// > process, and read no process configuration (no environment variable, no
/// > argument, no working directory) — with one narrow exception for a
/// > function whose *entire, stated purpose* is to read an ambient,
/// > non-secret, already-running property of the process itself: the system
/// > clock, or its already-present CSPRNG. That exception was already made,
/// > before this feature existed, for Rhai's own standard-package
/// > `timestamp()` (see below) — it is not a new door, it is the same one
/// > applied to two more functions that need the same thing.
///
/// Checked against that rule:
///
/// - **`base64_encode`/`base64_decode`** — a pure string transform over the
///   one argument the script passed in. No different in kind from a string
///   method Rhai's standard package already exposes (`to_upper`, `split`);
///   the only reason it needs Sendra's help at all is that the codec itself
///   is not part of Rhai's standard library.
/// - **`hmac_sha256`** — a pure cryptographic computation over the two
///   arguments the script passed in (key, message). Not ambient at all: every
///   byte the function touches came from the script's own call, so this is
///   *less* of a special case than `uuid`/`now` below, not more.
/// - **`uuid`** — falls under the exception: it reads the OS's own CSPRNG
///   (via `getrandom`, not a file it opens or a path a script controls) to
///   produce a value whose entire point is to be unpredictable. It returns
///   nothing about the host, and there is no argument a script could pass to
///   turn it into a read of anything else.
/// - **`now`** — falls under the exact same exception `timestamp()` already
///   established: it reads the system clock and nothing else. See the
///   dedicated note on why this one specifically was worth deciding on
///   rather than assuming.
///
/// None of the four takes a path, a URL, a command, or any other kind of
/// "resource identifier" that could be reinterpreted to reach something the
/// function's name does not promise — which is what keeps approving these
/// four from being a precedent a later, broader function (`read(name)`, say)
/// could ride in on. The sandbox tests below (`nothing_that_touches_the_
/// filesystem_or_the_network_is_reachable` and its neighbours) name exactly
/// the boundary this rule protects — a filesystem open, a socket, a spawned
/// process, an environment read — and every one of those names still
/// resolves to nothing, unchanged, because none of the four new functions is
/// any of them; see `none_of_the_new_functions_provide_a_filesystem_network_
/// or_process_wedge` for that checked directly against the new functions
/// themselves, not just the old forbidden names.
///
/// **Why `now()` and not just `timestamp()`.** Rhai's own `timestamp()`
/// returns an opaque instant meant for measuring *elapsed* time
/// (`timestamp() - timestamp()`), the same shape as [`std::time::Instant`] —
/// it cannot be formatted, compared to an epoch, or put in a header. The
/// canonical signing use case this whole feature exists for (HMAC-based API
/// authentication) routinely needs a wall-clock value *in* the signed
/// payload and *as* a header value, which nothing already reachable from a
/// script can produce. `now()` closes exactly that gap and no more: it
/// returns milliseconds since the Unix epoch as a plain integer, not a
/// formatted/timezone-aware string, so a script that wants a particular
/// format builds it from the integer rather than Sendra shipping a date
/// formatting library into the sandbox for one function.
///
/// Rhai's standard package is arithmetic, strings, arrays, object maps,
/// ranges, bit operations, `timestamp()` and maths. It opens no files, no
/// sockets and no processes; Rhai does not expose any of those to scripts
/// unless a host registers them. `timestamp()` reads the system clock, which
/// is ambient but not a capability the process did not already have — the
/// same reasoning `now()` extends below.
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

    engine.register_fn("base64_encode", base64_encode);
    engine.register_fn("base64_decode", base64_decode);
    engine.register_fn("hmac_sha256", hmac_sha256);
    engine.register_fn("uuid", uuid_v4);
    engine.register_fn("now", now_millis);

    engine
}

/// Add one line to the buffer [`capture`] is about to drain.
fn push_output(line: String) {
    OUTPUT.with(|output| output.borrow_mut().push(line));
}

/// `base64_encode(string) -> string`. Standard alphabet, padded — the same
/// [`base64::engine::general_purpose::STANDARD`] the request-side `auth.basic`
/// encoding already uses in [`crate::Request::resolve_auth`], so a signature a
/// script builds this way and a `Basic` header Sendra builds itself are the
/// same encoding by construction rather than by coincidence.
fn base64_encode(input: &str) -> String {
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, input)
}

/// `base64_decode(string) -> string`. Two ways this can fail — the text is not
/// valid base64 at all, or it decodes to bytes that are not valid UTF-8 (Rhai
/// strings are always UTF-8, so there is nowhere else for those bytes to go)
/// — and both are reported the way a `throw` is: see the `From<T:
/// AsRef<str>>` impl on `Box<EvalAltResult>` that a returned `Err(String)`
/// goes through, which produces the exact same `ErrorRuntime` variant
/// [`failure_message`] already unwraps to a bare message.
fn base64_decode(input: &str) -> Result<String, Box<rhai::EvalAltResult>> {
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, input)
        .map_err(|err| format!("invalid base64: {err}"))?;
    String::from_utf8(bytes).map_err(|_| "decoded base64 is not valid UTF-8".into())
}

/// `hmac_sha256(key, message) -> string`, hex-encoded — the canonical
/// Postman-style request-signing primitive this whole feature exists for.
///
/// `Hmac::<Sha256>::new_from_slice` returns a `Result` because the trait it
/// comes from is shared with MACs that *do* have a fixed key size, but HMAC
/// itself accepts a key of any length (longer than the block size is hashed
/// down first) — so the `Result` can never actually be `Err` here, and
/// `expect` says so rather than plumbing a `Result` a caller could never
/// meaningfully act on.
fn hmac_sha256(key: &str, message: &str) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;

    let mut mac =
        Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC accepts a key of any length");
    mac.update(message.as_bytes());
    hex_encode(&mac.finalize().into_bytes())
}

/// Lowercase hex, two characters per byte — the conventional rendering for an
/// HMAC digest (and the one nearly every API expecting a signature header
/// wants), rather than base64 or the raw bytes Rhai has no byte-array type to
/// hold anyway.
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `uuid() -> string`, a random (v4) UUID. See [`build_engine`]'s doc comment
/// for why reading the OS's CSPRNG fits the same exception `now()`'s system
/// clock read does.
fn uuid_v4() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// `now() -> int`, milliseconds since the Unix epoch. See [`build_engine`]'s
/// doc comment for why this exists alongside Rhai's own `timestamp()` rather
/// than instead of registering nothing.
fn now_millis() -> i64 {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the system clock is set after the Unix epoch")
        .as_millis();
    // `as`, not `try_from`: overflowing `i64` milliseconds since 1970 needs a
    // clock set past the year 292,278,994 — not a real failure mode worth an
    // error path for.
    millis as i64
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

    // --- the four registered functions ---------------------------------------

    #[test]
    fn base64_encode_and_decode_round_trip_a_known_pair() {
        let request = with_pre_request(
            r#"if base64_encode("hello") != "aGVsbG8=" { throw "encode mismatch"; }
if base64_decode("aGVsbG8=") != "hello" { throw "decode mismatch"; }
request.headers["X-Encoded"] = base64_encode(request.body);"#,
        );

        let resolved = run_pre(&request).expect("both known-vector checks pass");
        assert_eq!(resolved.header("X-Encoded"), Some("eyJpZCI6MX0="));
    }

    #[test]
    fn base64_decode_of_invalid_input_throws_rather_than_panics() {
        let request = with_pre_request(r#"base64_decode("not valid base64 !!!");"#);
        let err = run_pre(&request).expect_err("invalid base64 must not silently decode");

        match err {
            SendraError::ScriptFailed { message, .. } => {
                assert!(message.contains("invalid base64"), "got {message:?}");
            }
            other => panic!("expected ScriptFailed, got {other:?}"),
        }
    }

    #[test]
    fn base64_decode_of_bytes_that_are_not_utf8_throws() {
        // `//4=` decodes to the two bytes 0xFF 0xFE, which is not valid UTF-8 —
        // and a Rhai string can only ever hold valid UTF-8.
        let request = with_pre_request(r#"base64_decode("//4=");"#);
        let err = run_pre(&request).expect_err("non-UTF-8 decoded bytes must not become a string");

        match err {
            SendraError::ScriptFailed { message, .. } => {
                assert!(message.contains("not valid UTF-8"), "got {message:?}");
            }
            other => panic!("expected ScriptFailed, got {other:?}"),
        }
    }

    #[test]
    fn hmac_sha256_matches_a_known_test_vector() {
        // The standard "the quick brown fox" HMAC-SHA256 vector, hex-encoded —
        // reproducible by hand or with any other HMAC implementation.
        let request = with_pre_request(
            r#"let signature = hmac_sha256("key", "The quick brown fox jumps over the lazy dog");
if signature != "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8" {
  throw "signature mismatch: " + signature;
}"#,
        );

        run_pre(&request).expect("the known vector must match");
    }

    #[test]
    fn hmac_sha256_is_the_canonical_use_case_signing_a_header() {
        // The actual scenario this feature exists for: sign the request body
        // and attach the signature as a header, entirely from within the
        // script, using only functions this feature added.
        let request = with_pre_request(
            r#"request.headers["X-Signature"] = hmac_sha256("shared-secret", request.body);"#,
        );

        let resolved = run_pre(&request).expect("signing must not fail");
        let signature = resolved.header("X-Signature").expect("the header was set");
        // 32 bytes, hex-encoded.
        assert_eq!(signature.len(), 64);
        assert!(signature.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn uuid_produces_a_well_formed_random_v4_uuid_each_call() {
        let request = with_pre_request(
            r#"let a = uuid();
let b = uuid();
if a == b { throw "two calls produced the same uuid"; }
request.headers["X-A"] = a;
request.headers["X-B"] = b;"#,
        );

        let resolved = run_pre(&request).expect("uuid() must not fail");
        for header in ["X-A", "X-B"] {
            let value = resolved.header(header).unwrap();
            assert_eq!(value.len(), 36, "got {value:?}");
            // Version 4, variant bits set — the fixed positions every v4 UUID has.
            assert_eq!(value.as_bytes()[14], b'4', "not a v4 uuid: {value:?}");
            assert!(
                matches!(value.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
                "not a valid v4 variant nibble: {value:?}"
            );
        }
    }

    #[test]
    fn now_returns_a_plausible_current_unix_millisecond_timestamp() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let request = with_pre_request(r#"request.headers["X-Now"] = now().to_string();"#);
        let resolved = run_pre(&request).expect("now() must not fail");

        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let reported: i64 = resolved.header("X-Now").unwrap().parse().unwrap();
        assert!(
            (before..=after).contains(&reported),
            "now() = {reported}, expected between {before} and {after}"
        );
    }

    #[test]
    fn none_of_the_new_functions_provide_a_filesystem_network_or_process_wedge() {
        // The check the issue asked for directly: try to misuse each new
        // function as if its string argument were a resource identifier
        // rather than plain data, and confirm the result is exactly the pure
        // computation over the literal argument bytes — nothing more.
        //
        // `base64_decode` given a path: `/etc/passwd` is not valid base64, so
        // if this ever produced `Ok`, that would mean the argument was
        // resolved as *something other than the base64 text it plainly is* —
        // there is no legitimate reading under which this should succeed.
        let request = with_pre_request(r#"base64_decode("/etc/passwd");"#);
        assert!(
            run_pre(&request).is_err(),
            "a path is not valid base64 — this must fail exactly like any other invalid input"
        );

        // `hmac_sha256`/`base64_encode` given path-like strings: computed
        // independently here via the same crates but a *separate* call than
        // the one registered into the engine, to prove the registered
        // function is a pass-through over the literal bytes it was handed —
        // not a lookup of a file, a URL, or anything else at that path.
        use hmac::{Hmac, KeyInit, Mac};
        use sha2::Sha256;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"/etc/passwd").unwrap();
        mac.update(b"/etc/shadow");
        let expected_signature = super::hex_encode(&mac.finalize().into_bytes());
        let expected_encoded =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "/etc/shadow");

        let request = with_pre_request(
            r#"request.headers["Sig"] = hmac_sha256("/etc/passwd", "/etc/shadow");
request.headers["Enc"] = base64_encode("/etc/shadow");"#,
        );
        let resolved = run_pre(&request).expect("both are ordinary string/byte computations");
        assert_eq!(resolved.header("Sig"), Some(expected_signature.as_str()));
        assert_eq!(resolved.header("Enc"), Some(expected_encoded.as_str()));

        // And none of the other forbidden names from the sandbox test above
        // became reachable as a side effect of registering these four —
        // still exactly the same two outcomes as before: a parse error, or a
        // "function not found" runtime error.
        for attempt in [
            r#"open_file("/etc/passwd");"#,
            r#"read_file("/etc/passwd");"#,
            r#"http_get("https://example.com");"#,
            r#"env("HOME");"#,
        ] {
            let request = with_pre_request(attempt);
            let err =
                run_pre(&request).expect_err(&format!("`{attempt}` must still resolve to nothing"));
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
