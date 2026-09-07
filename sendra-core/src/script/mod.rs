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
//! sandbox is a property of what the [`Engine`](rhai::Engine) was built with
//! rather than of a separate runtime's flags.
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

use rhai::AST;

use crate::{Request, SendraError};

mod engine;
mod marshal;
mod run;

#[cfg(test)]
mod test_support;

pub use run::{run_post_request, run_pre_request};

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
        let ast = engine::ENGINE
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
    /// The two are not told apart here; see [`failure_message`](engine::failure_message)
    /// for why, and for what the string contains in each case.
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

#[cfg(test)]
mod tests {
    use super::*;

    use test_support::{request, with_pre_request};

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
}
