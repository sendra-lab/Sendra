//! Shared fixtures for `script`'s test modules.
//!
//! One copy rather than one per submodule, since every layer's tests build a
//! request the same way and run it through the same two entry points — a
//! second copy would just be a second place for these to drift apart.

use std::time::Duration;

use crate::{Request, Response, SendraError};

use super::{run_post_request, run_pre_request, Scripts};
use super::{ScriptOutcome, ScriptOutput};

pub(super) fn request(yaml: &str) -> Request {
    Request::from_yaml_str(yaml).expect("the test request should parse")
}

/// A request with a `pre_request` script and nothing else of note.
pub(super) fn with_pre_request(script: &str) -> Request {
    request(&format!(
        "method: POST\n\
         url: https://example.com/orders\n\
         headers:\n  Accept: application/json\n\
         body: '{{\"id\":1}}'\n\
         pre_request: |\n{}\n",
        indent(script)
    ))
}

pub(super) fn with_post_request(script: &str) -> Request {
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
pub(super) fn run_pre(request: &Request) -> Result<Request, SendraError> {
    // Two layers of failure flattened into one: the outer is "it did not
    // compile", the inner is "it compiled and then went wrong". These tests
    // only care which of them happened via the variant, not via the nesting.
    let (result, _output) = pre(request)?;
    result
}

/// The same, keeping the output — for the tests that are about it.
#[allow(clippy::type_complexity)]
pub(super) fn pre(
    request: &Request,
) -> Result<(Result<Request, SendraError>, ScriptOutput), SendraError> {
    let scripts = Scripts::compile(request)?;
    Ok(run_pre_request(
        scripts.pre_request().expect("the test has one"),
        request,
    ))
}

/// Compile and run a `post_request` script against `response`.
pub(super) fn run_post(request: &Request, response: &Response) -> ScriptOutcome {
    post(request, response).0
}

/// The same, keeping the output.
pub(super) fn post(request: &Request, response: &Response) -> (ScriptOutcome, ScriptOutput) {
    let scripts = Scripts::compile(request).expect("the test script should compile");
    run_post_request(scripts.post_request().expect("the test has one"), response)
}

pub(super) fn response() -> Response {
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
        redirects: Vec::new(),
    }
}
