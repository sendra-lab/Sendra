//! The two entry points a caller actually runs: `pre_request` against a
//! request, `post_request` against a response.

use rhai::{Dynamic, Scope};

use crate::{Request, Response, SendraError};

use super::engine::{capture, failure_message};
use super::marshal::{request_from_dynamic, request_map, response_map};
use super::{Hook, Script, ScriptOutcome, ScriptOutput};

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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::Method;

    use super::super::test_support::{
        response, run_post, run_pre, with_post_request, with_pre_request,
    };

    // --- pre_request ------------------------------------------------------

    #[test]
    fn a_pre_request_script_can_add_a_header() {
        let request = with_pre_request(r#"request.headers["X-Signature"] = "abc";"#);
        let sent = run_pre(&request).expect("the script should run");

        assert_eq!(sent.header("X-Signature"), Some("abc"));
        // And leaves everything else exactly as it was.
        assert_eq!(sent.header("Accept"), Some("application/json"));
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
        assert_eq!(sent.header("Accept"), Some("text/plain"));

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
            sent.header("X-Seen"),
            Some("POST https://example.com/orders 8")
        );
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
}
