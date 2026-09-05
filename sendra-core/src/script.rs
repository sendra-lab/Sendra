//! The `pre_request` and `post_request` hooks: compiling them, running them,
//! and the closed surface a script can see.
//!
//! # What a script is
//!
//! A script is Rhai source, written inline in the request file as a YAML block
//! scalar:
//!
//! ```text
//! method: POST
//! url: https://api.example.com/orders
//! pre_request: |
//!   request.headers["X-Request-Id"] = "abc-123";
//! post_request: |
//!   if response.status != 201 {
//!     throw "expected 201, got " + response.status;
//!   }
//! ```
//!
//! [Rhai] rather than an embedded JavaScript engine because the whole point of
//! the feature is that a script needs nothing installed next to `sendra`: the
//! interpreter is linked into the binary, there is no FFI boundary, and the
//! sandbox is a property of what the [`Engine`] was built with rather than of a
//! separate runtime's flags.
//!
//! # Script source is never substituted
//!
//! **A `{{variable}}` or `${OS_VAR}` inside a script is not expanded.** It is
//! whatever those characters mean to Rhai — in practice, part of a string
//! literal. [`Environment::apply`] copies both script fields through verbatim,
//! and there is a test on exactly that.
//!
//! This is a decision, not an oversight. Substitution is textual, and the whole
//! reason it is confined to values is that a value must not be able to change
//! the structure of the document it sits in. A script *is* structure: it is
//! executable code, so the failure mode is not a malformed URL but a variable
//! whose contents get parsed as program text. A script that needs an
//! environment value reads it off the request it is handed — `request.url` and
//! `request.headers` arrive fully substituted — which is both safe and the
//! honest place for it to come from.
//!
//! # Ordering
//!
//! Fixed, and stated here because it decides what an existing file means:
//!
//! 1. Environment substitution.
//! 2. Config apply.
//! 3. `pre_request`, against the fully-substituted, config-applied request. It
//!    is the last thing to touch the request before it goes over the wire, so
//!    a header it removes stays removed — which is why the CLI applies the
//!    config itself and then calls [`send_prepared`](crate::send_prepared)
//!    rather than [`send`](crate::send), whose whole job is to apply it.
//! 4. Send.
//! 5. `post_request`, against the response.
//! 6. Assertions, against the same response, unaffected by whether a
//!    `post_request` script ran or what it decided.
//!
//! Scripts and assertions are two independent mechanisms that happen to look at
//! the same response. Neither can see the other.
//!
//! # Both scripts are compiled before the request is sent
//!
//! [`Scripts::compile`] compiles `pre_request` *and* `post_request` up front,
//! so a syntax error in a `post_request` script is found before the `POST` that
//! would have created an order — not after. A file whose script does not parse
//! is a broken file in the same way a collection with two identically-named
//! requests is a broken file, and [`Collection`](crate::Collection) already
//! makes the argument: finding that out before the first request goes over the
//! wire beats finding it out halfway through a run.
//!
//! It is compiled per request rather than for the whole file, in the same place
//! and for the same reason substitution is per request: a script that will not
//! compile is that request's problem, not its siblings'.
//!
//! [Rhai]: https://rhai.rs
//! [`Environment::apply`]: crate::Environment::apply

use std::cell::RefCell;
use std::collections::BTreeMap;

use rhai::{Dynamic, Engine, Map, Scope, AST};

use crate::{Request, Response, SendraError};

/// Which of the two hooks a script is.
///
/// Carried on the error variants so a message can name the field the user has
/// to go and fix, in the spelling they wrote it in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hook {
    PreRequest,
    PostRequest,
}

impl Hook {
    /// The YAML key this hook is written as.
    pub fn as_str(self) -> &'static str {
        match self {
            Hook::PreRequest => "pre_request",
            Hook::PostRequest => "post_request",
        }
    }
}

impl std::fmt::Display for Hook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A script that has been parsed and is ready to run.
///
/// Compiling is separated from running because the two failures are different
/// problems for a user to fix and happen at different points in the pipeline:
/// a script that does not parse is a broken file, found before anything is
/// sent, while a script that parses and then throws is a statement about this
/// particular request or response.
#[derive(Debug, Clone)]
pub struct Script {
    hook: Hook,
    ast: AST,
}

impl Script {
    /// Parse `source` as the given hook.
    pub fn compile(hook: Hook, source: &str) -> Result<Self, SendraError> {
        let ast = ENGINE
            .with(|engine| engine.compile(source))
            .map_err(|source| SendraError::ScriptParse { hook, source })?;

        Ok(Self { hook, ast })
    }

    pub fn hook(&self) -> Hook {
        self.hook
    }
}

/// A request's two scripts, both compiled.
///
/// One type rather than two `Option<Script>` at the call site so that "compile
/// everything before sending anything" is a single call that cannot be
/// half-made, and so the CLI does not have to remember which order to compile
/// them in.
#[derive(Debug, Clone, Default)]
pub struct Scripts {
    pre_request: Option<Script>,
    post_request: Option<Script>,
}

impl Scripts {
    /// Compile whichever of `request`'s two script fields are present.
    ///
    /// `pre_request` is compiled first, so a file with two broken scripts
    /// reports the one that would have run first.
    pub fn compile(request: &Request) -> Result<Self, SendraError> {
        Ok(Self {
            pre_request: request
                .pre_request
                .as_deref()
                .map(|source| Script::compile(Hook::PreRequest, source))
                .transpose()?,
            post_request: request
                .post_request
                .as_deref()
                .map(|source| Script::compile(Hook::PostRequest, source))
                .transpose()?,
        })
    }

    pub fn pre_request(&self) -> Option<&Script> {
        self.pre_request.as_ref()
    }

    pub fn post_request(&self) -> Option<&Script> {
        self.post_request.as_ref()
    }
}

/// Anything a script printed while it ran, in the order it printed it.
///
/// Rhai's `print` and `debug` write somewhere, and the only question is where.
/// Core does not answer it: it collects the lines and hands them back, and the
/// front-end decides what a line is for — stderr for the CLI, a pane for a TUI,
/// a log record for something else. That is the same arrangement as every other
/// piece of "what does the outside world do here" in this crate: `Config` and
/// `Environment` take the directory to search rather than reading the real one,
/// and the CLI's sending loop takes the function that sends rather than calling
/// the network itself.
///
/// It matters more here than it looks. `sendra-core` has no `println!` or
/// `eprintln!` anywhere, by design, because a `sendra-tui` sharing this crate
/// cannot have a library writing over its interface — and a `print` in a script
/// is exactly the kind of thing that would otherwise land in the middle of a
/// redrawn frame, or inside the single JSON document `--json` promises stdout
/// holds.
///
/// A `debug` line is already formatted with its source and position by the time
/// it lands here, because that formatting is Rhai's information to render and
/// not the front-end's to reconstruct. Which stream it goes to, and whether it
/// is coloured, is the front-end's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptOutput {
    lines: Vec<String>,
}

impl ScriptOutput {
    /// The lines, in the order the script printed them.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Whether the script printed nothing — the usual case, and the one a
    /// front-end should be able to check without allocating.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// What a `post_request` script decided about a response.
///
/// Not a `Result<(), SendraError>`, because neither outcome is an error in the
/// sense the rest of this crate uses the word: the response came back, and the
/// script is a check on it, exactly as an assertion is. A script that throws
/// has *worked* — it has reported that the response was not what the file
/// expected — and the front-end that receives this treats it the way it treats
/// a failed assertion. See the CLI's `exit` module for where that lands in a
/// summary and an exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptOutcome {
    /// The script ran to completion without throwing.
    Passed,

    /// The script threw, or hit a runtime error.
    ///
    /// The two are not told apart here; see [`failure_message`] for why, and
    /// for what the string contains in each case.
    Failed { message: String },
}

impl ScriptOutcome {
    pub fn passed(&self) -> bool {
        matches!(self, ScriptOutcome::Passed)
    }

    /// Why the script failed, or `None` if it did not.
    pub fn failure(&self) -> Option<&str> {
        match self {
            ScriptOutcome::Passed => None,
            ScriptOutcome::Failed { message } => Some(message),
        }
    }
}

/// Run a `pre_request` script and return the request it left behind, along
/// with anything it printed.
///
/// The script sees `request` as an object map — see [`request_map`] for the
/// exact shape — mutates it in place, and this reads it back out and validates
/// it. Errors are [`SendraError`] rather than a [`ScriptOutcome`] because
/// there is no response and never will be: whatever went wrong, this request is
/// not being sent, which is the same category of failure as a missing variable
/// or a refused connection.
///
/// The [`ScriptOutput`] comes back **whether the script succeeded or not**,
/// which is why it is beside the `Result` rather than inside its `Ok`: a script
/// that printed three lines and then threw printed three lines, and those are
/// usually the three lines that explain the throw. Losing them on the error
/// path would lose them exactly when they are worth the most.
pub fn run_pre_request(
    script: &Script,
    request: &Request,
) -> (Result<Request, SendraError>, ScriptOutput) {
    debug_assert_eq!(script.hook, Hook::PreRequest);

    let mut scope = Scope::new();
    scope.push("request", request_map(request));

    let (result, output) = capture(|engine| engine.run_ast_with_scope(&mut scope, &script.ast));

    let result = result
        .map_err(|err| SendraError::ScriptFailed {
            hook: Hook::PreRequest,
            message: failure_message(&err),
        })
        .and_then(|()| {
            let left_behind = scope.get_value::<Dynamic>("request").expect(
                "`request` was pushed into the scope and a script cannot remove a variable",
            );

            request_from_dynamic(request, left_behind)
        });

    (result, output)
}

/// Run a `post_request` script against a response, returning its verdict and
/// anything it printed.
///
/// The verdict is infallible by design: every way this can go wrong is a
/// statement about the response, and the caller reports all of them the same
/// way. The response is pushed as a *constant*, so assigning to it is Rhai's
/// own error rather than a silent no-op — see [`response_map`].
pub fn run_post_request(script: &Script, response: &Response) -> (ScriptOutcome, ScriptOutput) {
    debug_assert_eq!(script.hook, Hook::PostRequest);

    let mut scope = Scope::new();
    scope.push_constant("response", response_map(response));

    let (result, output) = capture(|engine| engine.run_ast_with_scope(&mut scope, &script.ast));

    let outcome = match result {
        Ok(()) => ScriptOutcome::Passed,
        Err(err) => ScriptOutcome::Failed {
            message: failure_message(&err),
        },
    };

    (outcome, output)
}

// --- the script's view of a request and a response -----------------------

/// The `request` a `pre_request` script is handed:
///
/// ```text
/// request.method    "POST"                    read-only
/// request.url       "https://…/orders"        read/write, string
/// request.headers   #{ "Accept": "…" }        read/write, string to string
/// request.body      "…" or ()                 read/write, string or ()
/// ```
///
/// A plain object map rather than a registered custom type, which is what makes
/// `request.headers["X-Signature"] = sig;` mean what it looks like it means
/// with no getter/setter write-back subtleties to get right. The cost of a map
/// is that Rhai will happily accept `request.anything = 1`, so the closed
/// surface is enforced on the way back out instead — see
/// [`request_from_dynamic`], which rejects a key it does not know and a value
/// of the wrong type rather than dropping either on the floor.
///
/// **`name` and `assertions` are not here.** `name` is what
/// `sendra run <file> <name>` selects on, so a script-dependent label could not
/// be typed on a command line — the same reason [`Environment::apply`] leaves
/// it alone. `assertions` is the other mechanism that reads this response, and
/// the two are deliberately not integrated.
///
/// **`method` is readable and not writable.** A script may branch on it; an
/// assignment to it is an error rather than a silent no-op. Three reasons.
/// It is a closed enum in the schema, so today a bad method is caught by serde
/// at parse time with a position in the file, and letting a script set it would
/// move that check to a runtime failure against a value set of its own that
/// script authors would have to know. It is also the most load-bearing fact
/// about what a request *is*: the `→` label a run announces is `METHOD url`,
/// printed before the script runs, so a script that changed it would make that
/// line a lie about what went over the wire. And the cost of refusing is close
/// to zero — a call that needs to be both a GET and a POST is two requests, and
/// writing them as two is clearer than writing one and a branch.
///
/// [`Environment::apply`]: crate::Environment::apply
fn request_map(request: &Request) -> Dynamic {
    let mut headers = Map::new();
    for (name, value) in &request.headers {
        headers.insert(name.as_str().into(), value.clone().into());
    }

    let mut map = Map::new();
    map.insert("method".into(), request.method.as_str().into());
    map.insert("url".into(), request.url.clone().into());
    map.insert("headers".into(), Dynamic::from_map(headers));
    map.insert(
        "body".into(),
        match &request.body {
            Some(body) => body.clone().into(),
            // Rhai's unit, which reads as `if request.body == () { … }` — a
            // request with no body and a request with an empty one are
            // different things and stay different here.
            None => Dynamic::UNIT,
        },
    );

    Dynamic::from_map(map)
}

/// The `response` a `post_request` script is handed:
///
/// ```text
/// response.status       201
/// response.status_text  "Created"
/// response.headers      [ #{ name: "content-type", value: "application/json" }, … ]
/// response.body         "…"
/// response.elapsed_ms   12
/// ```
///
/// **Read-only, and enforced rather than hoped for**: the caller pushes this
/// with [`Scope::push_constant`], so `response.status = 200` is Rhai's
/// "cannot modify a constant" error. Handing over a mutable copy whose
/// mutations were then discarded would be a silently ignored input, which is
/// the one thing Sendra refuses to have anywhere in its schema.
///
/// `headers` is a list of `#{name, value}` rather than a map keyed by name,
/// because HTTP lets a header repeat (`set-cookie`) and wire order is worth
/// keeping — the same shape, for the same reason, that `--json` reports. Lookup
/// is a one-liner with Rhai's array methods:
///
/// ```text
/// let ct = response.headers.find(|h| h.name == "content-type");
/// if ct == () || !ct.value.contains("json") { throw "expected a JSON response"; }
/// ```
///
/// Header names arrive exactly as the server sent them, which for HTTP/2 means
/// lower-case and for HTTP/1.1 means whatever casing was on the wire; compare
/// with `to_lower()` if it matters.
fn response_map(response: &Response) -> Dynamic {
    let headers: rhai::Array = response
        .headers
        .iter()
        .map(|(name, value)| {
            let mut header = Map::new();
            header.insert("name".into(), name.clone().into());
            header.insert("value".into(), value.clone().into());
            Dynamic::from_map(header)
        })
        .collect();

    let mut map = Map::new();
    map.insert("status".into(), (response.status as i64).into());
    map.insert("status_text".into(), response.status_text.clone().into());
    map.insert("headers".into(), Dynamic::from_array(headers));
    map.insert("body".into(), response.body.clone().into());
    map.insert(
        "elapsed_ms".into(),
        (response.elapsed.as_millis() as i64).into(),
    );

    Dynamic::from_map(map)
}

/// Read the script's `request` back into a [`Request`], refusing anything that
/// is not the shape [`request_map`] handed it.
///
/// Every rejection here is a message naming the field, because the alternative
/// — dropping an unknown key, or coercing a value — is a script that appears to
/// have worked and did not. `deny_unknown_fields` on the YAML schema makes
/// exactly this promise about the file; a script is the same document by
/// another route and gets the same promise.
///
/// **Values are not coerced.** `request.headers["X-Count"] = 5` is an error,
/// not the header `5`. A header value is a string on the wire, `.to_string()`
/// is one call, and silently stringifying leaves what a Rhai float renders as
/// up to Rhai rather than up to whoever has to read the request later.
fn request_from_dynamic(original: &Request, value: Dynamic) -> Result<Request, SendraError> {
    let invalid = |reason: String| SendraError::ScriptRequest { reason };

    let map = value.try_cast::<Map>().ok_or_else(|| {
        invalid(
            "`request` was replaced with something that is not an object map; \
             modify its fields rather than assigning over it"
                .to_string(),
        )
    })?;

    for key in map.keys() {
        if !matches!(key.as_str(), "method" | "url" | "headers" | "body") {
            return Err(invalid(format!(
                "`request.{key}` is not a field a request has (method, url, headers, body)"
            )));
        }
    }

    // Read-only, and an assignment to it is an error rather than a shrug. See
    // `request_map` for the reasoning.
    match map.get("method") {
        Some(method)
            if method.clone().try_cast::<String>().as_deref() == Some(original.method.as_str()) => {
        }
        Some(method) => {
            return Err(invalid(format!(
                "`request.method` is read-only: it was `{}` and the script set it to `{}`",
                original.method,
                method.to_string().trim()
            )))
        }
        None => {
            return Err(invalid(
                "`request.method` is read-only and was removed by the script".to_string(),
            ))
        }
    }

    let url = string_field(&map, "url")?.ok_or_else(|| {
        invalid("`request.url` was removed by the script; a request needs one".to_string())
    })?;

    let headers_value = map.get("headers").cloned().ok_or_else(|| {
        invalid(
            "`request.headers` was removed by the script; assign an empty map (`#{}`) \
             to send no headers"
                .to_string(),
        )
    })?;
    let headers_map = headers_value.try_cast::<Map>().ok_or_else(|| {
        invalid("`request.headers` must be an object map of header name to string".to_string())
    })?;

    let mut headers = BTreeMap::new();
    for (name, value) in headers_map {
        let value = value.try_cast::<String>().ok_or_else(|| {
            invalid(format!(
                "`request.headers[\"{name}\"]` must be a string; call `.to_string()` on it"
            ))
        })?;
        headers.insert(name.to_string(), value);
    }

    Ok(Request {
        // Not the script's to change: see `request_map`.
        name: original.name.clone(),
        method: original.method,
        url,
        headers,
        body: string_field(&map, "body")?,
        // The other mechanism that reads this response, carried through
        // untouched. Scripts and assertions do not see each other.
        assertions: original.assertions.clone(),
        // A script cannot rewrite the script. Both fields are carried through
        // so the returned value is still a faithful `Request`, but nothing
        // downstream reads them again: both hooks were compiled before this one
        // ran, from the source in the file.
        pre_request: original.pre_request.clone(),
        post_request: original.post_request.clone(),
        // Not exposed to the script and so not the script's to change. Letting
        // a `pre_request` hook rewrite what the response will be read for is a
        // feature in its own right — as is letting a script stash a value for
        // later requests — and neither is this one.
        capture: original.capture.clone(),
    })
}

/// A `String`-or-`()` field of the script's `request` map.
///
/// `Ok(None)` for a field set to `()` *or* removed outright — the two mean the
/// same thing for `body` ("no body"), and `url` treats `None` as its own error
/// because a request without one cannot be sent.
fn string_field(map: &Map, key: &str) -> Result<Option<String>, SendraError> {
    match map.get(key) {
        None => Ok(None),
        Some(value) if value.is_unit() => Ok(None),
        Some(value) => {
            value
                .clone()
                .try_cast::<String>()
                .map(Some)
                .ok_or_else(|| SendraError::ScriptRequest {
                    reason: format!(
                        "`request.{key}` must be a string, or `()` for none; it is {}",
                        describe(value)
                    ),
                })
        }
    }
}

/// A script value as an error message should name it: `a string`, `an i64`.
fn describe(value: &Dynamic) -> String {
    let type_name = value.type_name();
    let article = if type_name.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    };
    format!("{article} {type_name}")
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
/// [`ScriptOutcome`] and the CLI's `exit` module for why one reliable split
/// (compile versus run) is preferred to a second, guessable one.
fn failure_message(err: &rhai::EvalAltResult) -> String {
    match err {
        rhai::EvalAltResult::ErrorRuntime(value, _) => value.to_string(),
        other => other.to_string(),
    }
}

// --- the engine ----------------------------------------------------------

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
    /// mutated afterwards, and each run gets a fresh [`Scope`]. Per-thread
    /// rather than a `static` because a Rhai [`Engine`] is deliberately not
    /// `Sync` unless built with its `sync` feature, which costs every value an
    /// atomic refcount for a binary that runs on one thread.
    static ENGINE: Engine = build_engine();

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
fn capture<T>(body: impl FnOnce(&Engine) -> T) -> (T, ScriptOutput) {
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use crate::Method;

    fn request(yaml: &str) -> Request {
        Request::from_yaml_str(yaml).expect("the test request should parse")
    }

    /// A request with a `pre_request` script and nothing else of note.
    fn with_pre_request(script: &str) -> Request {
        request(&format!(
            "method: POST\n\
             url: https://example.com/orders\n\
             headers:\n  Accept: application/json\n\
             body: '{{\"id\":1}}'\n\
             pre_request: |\n{}\n",
            indent(script)
        ))
    }

    fn with_post_request(script: &str) -> Request {
        request(&format!(
            "method: GET\nurl: https://example.com\npost_request: |\n{}\n",
            indent(script)
        ))
    }

    fn indent(script: &str) -> String {
        script
            .lines()
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Compile and run a `pre_request` script against `request`, for the tests
    /// that are about the verdict rather than about what it printed.
    fn run_pre(request: &Request) -> Result<Request, SendraError> {
        // Two layers of failure flattened into one: the outer is "it did not
        // compile", the inner is "it compiled and then went wrong". These tests
        // only care which of them happened via the variant, not via the nesting.
        let (result, _output) = pre(request)?;
        result
    }

    /// The same, keeping the output — for the tests that are about it.
    #[allow(clippy::type_complexity)]
    fn pre(request: &Request) -> Result<(Result<Request, SendraError>, ScriptOutput), SendraError> {
        let scripts = Scripts::compile(request)?;
        Ok(run_pre_request(
            scripts.pre_request().expect("the test has one"),
            request,
        ))
    }

    /// Compile and run a `post_request` script against `response`.
    fn run_post(request: &Request, response: &Response) -> ScriptOutcome {
        post(request, response).0
    }

    /// The same, keeping the output.
    fn post(request: &Request, response: &Response) -> (ScriptOutcome, ScriptOutput) {
        let scripts = Scripts::compile(request).expect("the test script should compile");
        run_post_request(scripts.post_request().expect("the test has one"), response)
    }

    fn response() -> Response {
        Response {
            status: 201,
            status_text: "Created".to_string(),
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("set-cookie".to_string(), "a=1".to_string()),
                ("set-cookie".to_string(), "b=2".to_string()),
            ],
            body: r#"{"id":7,"name":"ada"}"#.to_string(),
            elapsed: Duration::from_millis(12),
        }
    }

    // --- pre_request ------------------------------------------------------

    #[test]
    fn a_pre_request_script_can_add_a_header() {
        let request = with_pre_request(r#"request.headers["X-Signature"] = "abc";"#);
        let sent = run_pre(&request).expect("the script should run");

        assert_eq!(
            sent.headers.get("X-Signature").map(String::as_str),
            Some("abc")
        );
        // And leaves everything else exactly as it was.
        assert_eq!(
            sent.headers.get("Accept").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(sent.url, "https://example.com/orders");
        assert_eq!(sent.method, Method::Post);
        assert_eq!(sent.body.as_deref(), Some(r#"{"id":1}"#));
    }

    #[test]
    fn a_pre_request_script_can_modify_and_remove_a_header() {
        let request = with_pre_request(
            "request.headers[\"Accept\"] = \"text/plain\";\nrequest.headers.remove(\"Nope\");",
        );
        let sent = run_pre(&request).expect("the script should run");
        assert_eq!(
            sent.headers.get("Accept").map(String::as_str),
            Some("text/plain")
        );

        // Removing one that is there really removes it — the script is the last
        // thing to touch the request, so nothing puts it back.
        let request = with_pre_request(r#"request.headers.remove("Accept");"#);
        let sent = run_pre(&request).expect("the script should run");
        assert!(sent.headers.is_empty(), "{:?}", sent.headers);
    }

    #[test]
    fn a_pre_request_script_can_rewrite_the_url_and_the_body() {
        let request =
            with_pre_request("request.url = request.url + \"?dry_run=1\";\nrequest.body = \"{}\";");
        let sent = run_pre(&request).expect("the script should run");

        assert_eq!(sent.url, "https://example.com/orders?dry_run=1");
        assert_eq!(sent.body.as_deref(), Some("{}"));
    }

    #[test]
    fn a_pre_request_script_can_clear_the_body() {
        // `()` is "no body", which is a different thing from an empty one.
        let request = with_pre_request("request.body = ();");
        assert_eq!(run_pre(&request).expect("the script should run").body, None);

        let request = with_pre_request(r#"request.body = "";"#);
        assert_eq!(
            run_pre(&request)
                .expect("the script should run")
                .body
                .as_deref(),
            Some("")
        );
    }

    #[test]
    fn a_pre_request_script_can_read_the_request_it_was_given() {
        // Every field is readable, including the method it may not write.
        let request = with_pre_request(
            r#"request.headers["X-Seen"] = request.method + " " + request.url + " " + request.body.len();"#,
        );
        let sent = run_pre(&request).expect("the script should run");

        assert_eq!(
            sent.headers.get("X-Seen").map(String::as_str),
            Some("POST https://example.com/orders 8")
        );
    }

    #[test]
    fn a_pre_request_script_cannot_change_the_method() {
        let request = with_pre_request(r#"request.method = "GET";"#);
        let err = run_pre(&request).expect_err("assigning to the method is an error");

        assert!(
            matches!(&err, SendraError::ScriptRequest { reason } if reason.contains("read-only")),
            "{err:?}"
        );
        // Named in the message, both what it was and what the script tried.
        let message = err.to_string();
        assert!(
            message.contains("POST") && message.contains("GET"),
            "{message}"
        );
    }

    #[test]
    fn a_pre_request_script_cannot_invent_a_field() {
        let request = with_pre_request("request.timeout = 5;");
        let err = run_pre(&request).expect_err("an unknown field is an error");

        assert!(
            matches!(&err, SendraError::ScriptRequest { reason } if reason.contains("request.timeout")),
            "{err:?}"
        );
    }

    #[test]
    fn a_pre_request_script_cannot_set_a_header_to_a_non_string() {
        let request = with_pre_request(r#"request.headers["X-Count"] = 5;"#);
        let err = run_pre(&request).expect_err("a non-string header value is an error");

        // The message says what to do about it rather than only what is wrong.
        assert!(err.to_string().contains("to_string()"), "{err}");
    }

    #[test]
    fn a_pre_request_script_cannot_replace_the_request_wholesale() {
        let request = with_pre_request("request = 42;");
        let err = run_pre(&request).expect_err("replacing `request` is an error");
        assert!(matches!(err, SendraError::ScriptRequest { .. }), "{err:?}");
    }

    #[test]
    fn a_pre_request_script_that_throws_is_a_typed_error_not_a_panic() {
        let request = with_pre_request(r#"throw "no signing key";"#);
        let err = run_pre(&request).expect_err("a throw stops the request");

        assert!(
            matches!(
                &err,
                SendraError::ScriptFailed {
                    hook: Hook::PreRequest,
                    message
                } if message == "no signing key"
            ),
            "{err:?}"
        );
    }

    // --- post_request -----------------------------------------------------

    #[test]
    fn a_post_request_script_can_read_the_response_and_pass() {
        let request = with_post_request(
            "if response.status != 201 { throw \"expected 201\"; }\n\
             if !response.body.contains(\"ada\") { throw \"expected ada\"; }\n\
             if response.status_text != \"Created\" { throw \"expected Created\"; }\n\
             if response.elapsed_ms < 0 { throw \"time ran backwards\"; }",
        );

        assert_eq!(run_post(&request, &response()), ScriptOutcome::Passed);
    }

    #[test]
    fn a_post_request_script_sees_every_header_including_repeats() {
        // The list shape earns its keep here: a map keyed by name would have
        // dropped one of the two `set-cookie`s without a word.
        let request = with_post_request(
            "let cookies = response.headers.filter(|h| h.name == \"set-cookie\");\n\
             if cookies.len() != 2 { throw \"expected 2 set-cookie headers, got \" + cookies.len(); }\n\
             let ct = response.headers.find(|h| h.name == \"content-type\");\n\
             if ct == () || !ct.value.contains(\"json\") { throw \"expected JSON\"; }",
        );

        assert_eq!(run_post(&request, &response()), ScriptOutcome::Passed);
    }

    #[test]
    fn a_post_request_script_can_fail_explicitly() {
        let request = with_post_request(
            r#"if response.status != 200 { throw "expected 200, got " + response.status; }"#,
        );

        assert_eq!(
            run_post(&request, &response()),
            ScriptOutcome::Failed {
                message: "expected 200, got 201".to_string()
            }
        );
    }

    #[test]
    fn a_thrown_message_is_reported_verbatim() {
        // Not wrapped in "Runtime error: … (line 1, position 1)": the sentence
        // the author wrote is the thing to read.
        let request = with_post_request(r#"throw "the order id was missing";"#);
        let outcome = run_post(&request, &response());

        assert_eq!(outcome.failure(), Some("the order id was missing"));
        assert!(!outcome.passed());
    }

    #[test]
    fn a_bug_in_a_post_request_script_keeps_its_position() {
        // The other half of the wording rule: this is a mistake in the script,
        // not a statement about the response, so the line number is the point.
        let request = with_post_request("response.body.no_such_method();");
        let outcome = run_post(&request, &response());

        let message = outcome.failure().expect("a bug is still a failure");
        assert!(message.contains("no_such_method"), "{message}");
        assert!(message.contains("line"), "{message}");
    }

    #[test]
    fn a_post_request_script_cannot_modify_the_response() {
        // Pushed as a constant, so this is Rhai's own error rather than a
        // mutation that goes nowhere.
        let request = with_post_request("response.status = 200;");
        let outcome = run_post(&request, &response());

        assert!(!outcome.passed(), "assigning to the response must fail");
        let message = outcome.failure().unwrap();
        assert!(message.to_lowercase().contains("constant"), "{message}");
    }

    // --- compiling --------------------------------------------------------

    #[test]
    fn a_syntax_error_is_a_compile_error_not_a_runtime_one() {
        let request = with_pre_request("request.url = ;");
        let err = Scripts::compile(&request).expect_err("broken syntax should not compile");

        assert!(
            matches!(
                err,
                SendraError::ScriptParse {
                    hook: Hook::PreRequest,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn a_post_request_syntax_error_is_found_before_the_request_is_sent() {
        // The reason both hooks are compiled together: the `POST` that would
        // have created an order never happens because the *check* on it does
        // not parse.
        let request = request(
            "method: POST\nurl: https://example.com/orders\npost_request: |\n  if response.status { \n",
        );
        let err = Scripts::compile(&request).expect_err("broken syntax should not compile");

        assert!(
            matches!(
                err,
                SendraError::ScriptParse {
                    hook: Hook::PostRequest,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn a_request_with_no_scripts_compiles_to_nothing() {
        let request = request("method: GET\nurl: https://example.com\n");
        let scripts = Scripts::compile(&request).expect("nothing to compile");

        assert!(scripts.pre_request().is_none());
        assert!(scripts.post_request().is_none());
    }

    #[test]
    fn the_first_broken_script_is_the_one_reported() {
        // Both broken: the one that would have run first is the one to fix
        // first.
        let request = request(
            "method: GET\nurl: https://example.com\npre_request: |\n  ) (\npost_request: |\n  ) (\n",
        );
        let err = Scripts::compile(&request).expect_err("neither script compiles");

        assert!(
            matches!(
                err,
                SendraError::ScriptParse {
                    hook: Hook::PreRequest,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    // --- what a script prints ---------------------------------------------
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

    // --- the sandbox ------------------------------------------------------

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

    // --- the request carried back out -------------------------------------

    #[test]
    fn a_script_cannot_reach_the_assertions_or_the_scripts() {
        // The two mechanisms do not see each other, and a script cannot rewrite
        // the script. Neither field is in the map, so touching either is an
        // unknown field.
        for attempt in ["request.assertions = #{};", r#"request.pre_request = "";"#] {
            let request = with_pre_request(attempt);
            assert!(
                matches!(run_pre(&request), Err(SendraError::ScriptRequest { .. })),
                "`{attempt}` should have been refused"
            );
        }

        // And they survive the round trip untouched.
        let request = request(
            "method: GET\nurl: https://example.com\nassertions:\n  status: 200\npre_request: |\n  request.url = \"https://example.com/x\";\n",
        );
        let sent = run_pre(&request).expect("the script should run");

        assert_eq!(sent.assertions, request.assertions);
        assert_eq!(sent.pre_request, request.pre_request);
        assert_eq!(sent.name, request.name);
    }

    #[test]
    fn a_script_that_does_nothing_changes_nothing() {
        // The no-op guarantee at the level of the whole request: an empty
        // script must round-trip a request exactly.
        let request = request(
            "name: Create\n\
             method: POST\n\
             url: https://example.com/orders\n\
             headers:\n  Accept: application/json\n  X-Api-Key: secret\n\
             body: '{\"id\":1}'\n\
             assertions:\n  status: 201\n\
             pre_request: |\n  // nothing at all\n",
        );

        let sent = run_pre(&request).expect("an empty script should run");

        assert_eq!(sent, request);
    }
}
