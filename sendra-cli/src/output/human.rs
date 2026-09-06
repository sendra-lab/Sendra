//! Rendering a response, its assertions and a run's summary for a terminal.
//! Reached only through the [`Reporter`](super::Reporter), which decides
//! whether a run is rendered at all.

use std::borrow::Cow;

use owo_colors::{OwoColorize, Stream};
use sendra_core::{AssertionReport, CaptureReport, RedirectHop, Response, ScriptOutcome};

use crate::exit::Summary;

/// Print the redirect chain that led to a response, one line per hop:
///
/// ```text
/// → 301 https://example.com/old
/// → 302 https://example.com/mid
/// ```
///
/// Above the status line, in the order the hops actually happened, so a
/// redirected request reads as the chain it was rather than presenting only
/// the response it ended on. Nothing is printed for a response that was not
/// redirected — most of them — so a run that touches nothing new looks exactly
/// as it always has.
///
/// The same dimmed `→` the `→ <label>` line on stderr uses, but on stdout and
/// coloured differently: this is part of the response, not the announcement of
/// a request, and belongs in [`Reporter::responded`](super::Reporter::responded)'s
/// output rather than alongside it.
fn print_redirects(redirects: &[RedirectHop]) {
    for hop in redirects {
        println!(
            "{} {} {}",
            "→".if_supports_color(Stream::Stdout, |t| t.dimmed()),
            hop.status
                .to_string()
                .if_supports_color(Stream::Stdout, |t| t.yellow()),
            hop.location
        );
    }
}

/// The one line every response gets, whichever subcommand asked for it:
/// `200 OK  412 ms`.
///
/// Split out of [`print_response`] so that `sendra test`, which prints no
/// headers and no body, still says which response the assertions under it are
/// about — and says it in the same words and the same colours, rather than in a
/// second rendering of the same fact. Both callers get the redirect chain too,
/// since it printed here rather than in [`print_response`] alone: a chain is
/// as much a fact about what "the response the checks below it are about"
/// took to arrive as the status line is.
pub(super) fn print_status_line(response: &Response) {
    print_redirects(&response.redirects);

    let status_line = format!("{} {}", response.status, response.status_text);
    let status_line = status_line.trim_end().to_string();

    // Green for 2xx, red otherwise — a 404 is a completed run but a failed
    // request, and the colour is what says so at a glance.
    let painted = if response.is_success() {
        status_line
            .if_supports_color(Stream::Stdout, |t| t.green())
            .to_string()
    } else {
        status_line
            .if_supports_color(Stream::Stdout, |t| t.red())
            .to_string()
    };

    println!(
        "{}  {}",
        painted.if_supports_color(Stream::Stdout, |t| t.bold()),
        format!("{} ms", response.elapsed.as_millis())
            .if_supports_color(Stream::Stdout, |t| t.dimmed())
    );
}

pub(super) fn print_response(response: &Response) {
    print_status_line(response);

    for (name, value) in &response.headers {
        println!(
            "{}: {}",
            name.if_supports_color(Stream::Stdout, |t| t.cyan()),
            value
        );
    }

    if !response.body.is_empty() {
        println!();
        println!("{}", body_for_display(response));
    }
}

/// The body as [`print_response`] shows it: re-indented when the response says
/// it is JSON and the bytes agree, and the raw body in every other case.
///
/// **The decision is made from `content-type` and nowhere else.** A body
/// starting with `{` is not a promise of anything — a text/plain body, an HTML
/// error page from a proxy, a JSON-looking log line — and guessing from the
/// first byte would mean the same server reply is displayed two different ways
/// depending on what its first character happens to be. `application/json` and
/// anything with a `+json` suffix ([RFC 6839]) count; the parameters after a
/// `;` are ignored, so `application/json; charset=utf-8` is JSON like the bare
/// form is.
///
/// **A body that claims to be JSON and is not, prints raw.** A truncated
/// response, or an error page served under the wrong content-type, is exactly
/// when the body is worth looking at, so a parse failure falls back rather than
/// hiding it or failing the run. The parse failure itself is not reported: the
/// body is on screen, and whether it is valid JSON is a question the assertions
/// exist to answer.
///
/// Returns a [`Cow`] because the common case — a non-JSON body — must not copy
/// the response body to print it.
///
/// One caveat, from re-serialising through [`serde_json::Value`]: numbers are
/// carried as `f64`, so a JSON number with more precision than that holds is
/// re-printed rounded. Key order is preserved (see the `preserve_order`
/// feature in `Cargo.toml`); duplicate keys in one object are not — the last
/// wins, as it does for every other JSON reader.
///
/// [RFC 6839]: https://www.rfc-editor.org/rfc/rfc6839
pub(super) fn body_for_display(response: &Response) -> Cow<'_, str> {
    if !claims_json(&response.headers) {
        return Cow::Borrowed(&response.body);
    }

    match serde_json::from_str::<serde_json::Value>(&response.body) {
        Ok(value) => match serde_json::to_string_pretty(&value) {
            Ok(pretty) => Cow::Owned(pretty),
            Err(_) => Cow::Borrowed(&response.body),
        },
        Err(_) => Cow::Borrowed(&response.body),
    }
}

/// Whether these response headers say the body is JSON.
///
/// Header names are matched case-insensitively because HTTP header names are,
/// and so is the media type, because [RFC 9110] says media types are. A
/// repeated `content-type` — which is malformed, but happens — counts if any of
/// them says JSON.
///
/// [RFC 9110]: https://www.rfc-editor.org/rfc/rfc9110#name-media-type
fn claims_json(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .any(|(_, value)| {
            let media_type = value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            media_type == "application/json" || media_type.ends_with("+json")
        })
}

/// Print one request's assertion results under its response.
///
/// ```text
/// assertions
///   ✓ status is 200
///   ✓ header `content-type` is `application/json`
///   ✗ body contains `widget` — not found in the 429-byte body
///   ✗ `$.json.count` is 3 — got 2
///   2 passed, 2 failed
/// ```
///
/// Green for a `✓` and for the pass count, red for a `✗`, its detail and the
/// fail count.
///
/// An empty report prints nothing at all, so a request with no `assertions`
/// block produces byte-for-byte the output it did before assertions existed.
///
/// This goes to stdout, with the response rather than with the `→` label on
/// stderr: an assertion result is a statement *about the response*, produced
/// only because the file asked for it, and belongs next to the thing it
/// describes. The indented block under a dimmed `assertions` heading is what
/// keeps it apart from the raw body above it — a body can end in anything,
/// including a line that looks like a checkmark, so the separation is carried
/// by a blank line and a heading rather than by the symbols alone.
///
/// The wording of each line comes from core, so a future TUI reports the same
/// failure in the same words. Only the symbols, colour and layout are decided
/// here.
pub(super) fn print_assertions(report: &AssertionReport) {
    if report.is_empty() {
        return;
    }

    println!();
    println!(
        "{}",
        "assertions".if_supports_color(Stream::Stdout, |t| t.dimmed())
    );

    for result in report.results() {
        match &result.failure {
            None => println!(
                "  {} {}",
                "✓".if_supports_color(Stream::Stdout, |t| t.green()),
                result.expectation
            ),
            // The detail is on the same line as the expectation it belongs to:
            // "what I asked for" and "what I got" are one sentence, and reading
            // them together is the whole reason the detail exists.
            Some(detail) => println!(
                "  {} {} {} {}",
                "✗".if_supports_color(Stream::Stdout, |t| t.red()),
                result.expectation,
                "—".if_supports_color(Stream::Stdout, |t| t.dimmed()),
                detail.if_supports_color(Stream::Stdout, |t| t.red())
            ),
        }
    }

    // A count line even for a single assertion: it is the one line worth
    // grepping for, and a summary that appears only sometimes is worse to
    // script against than one that is always there.
    //
    // Each half is coloured like the symbol it counts — green for the passes,
    // red for the failures — rather than the line taking one colour from
    // whether anything failed. Colouring the whole line red made "4 passed"
    // read as bad news.
    let passed = format!("{} passed", report.passed_count())
        .if_supports_color(Stream::Stdout, |t| t.green())
        .to_string();

    if report.passed() {
        println!("  {passed}");
    } else {
        let failed = format!("{} failed", report.failed_count())
            .if_supports_color(Stream::Stdout, |t| t.red())
            .to_string();
        println!("  {passed}, {failed}");
    }
}

/// Print what a request's `post_request` script decided, under its response.
///
/// ```text
/// post_request
///   ✓ passed
/// ```
///
/// ```text
/// post_request
///   ✗ expected 201, got 500
/// ```
///
/// Same shape as [`print_assertions`] — dimmed heading, indented results, green
/// `✓` and red `✗` — because it is the same kind of statement: a check the file
/// asked for, reported next to the response it is about. It goes *above* the
/// assertions block, which is the order the two ran in.
///
/// A passing script prints a line rather than staying silent. The rule the
/// assertions block follows is "nothing declared, nothing printed", and a
/// script that ran is something declared: having asked for a check, the author
/// should be able to see it happened. Nothing at all is printed for a request
/// with no `post_request` block, so a file written before scripts existed looks
/// exactly as it did before they existed — the caller decides that by not
/// calling this.
///
/// The failure text is the script's own, straight from core: a thrown message
/// verbatim, or Rhai's full error with a line number when the script had a bug
/// rather than a complaint. Only the symbol, colour and layout are decided here.
///
/// `✓ passed` rather than repeating the script source: unlike an assertion,
/// which core can render as the phrase `status is 200`, a script has no short
/// form — the "expectation" is the whole block, it is in the file the reader
/// just ran, and echoing it back would put a program in the middle of a run's
/// output every single time it worked.
pub(super) fn print_post_request(outcome: &ScriptOutcome) {
    println!();
    println!(
        "{}",
        "post_request".if_supports_color(Stream::Stdout, |t| t.dimmed())
    );

    match outcome.failure() {
        None => println!(
            "  {} passed",
            "✓".if_supports_color(Stream::Stdout, |t| t.green())
        ),
        Some(failure) => println!(
            "  {} {}",
            "✗".if_supports_color(Stream::Stdout, |t| t.red()),
            failure.if_supports_color(Stream::Stdout, |t| t.red())
        ),
    }
}

/// Print what a request's `capture` block produced:
///
/// ```text
/// capture
///   ✓ auth_token from `$.token`
///   ✗ user_id from `$.user.id` — matched nothing in the response body
/// ```
///
/// Below the assertions rather than above them, which is the order the two ran
/// in and also the order they are about: the checks concern this response, the
/// capture concerns the requests still to come.
///
/// **The captured value is not printed.** Every other block here shows what it
/// compared, so the omission is deliberate: a capture exists to carry a token,
/// an id or a session cookie, and putting those on the terminal by default
/// would be a decision the file's author never made — `sendra test` in CI
/// prints no response body, and a captured bearer token appearing in a build
/// log because someone added `capture:` is not a trade to make on their behalf.
/// Nothing is actually hidden: under `sendra run` the body it came from is
/// printed in full a few lines above, and `--json` carries the values because
/// it already carries that same body verbatim.
///
/// The path is shown instead, because it is the half a reader needs when the
/// capture went wrong, and the failure text is core's own — only the symbol,
/// colour and layout are decided here.
///
/// Nothing at all is printed for a request with no `capture` block, so a file
/// written before this feature existed looks exactly as it did before it
/// existed.
pub(super) fn print_capture(report: &CaptureReport) {
    if report.is_empty() {
        return;
    }

    println!();
    println!(
        "{}",
        "capture".if_supports_color(Stream::Stdout, |t| t.dimmed())
    );

    for result in report.results() {
        let from = format!("{} from `{}`", result.variable, result.path);
        match result.failure() {
            None => println!(
                "  {} {}",
                "✓".if_supports_color(Stream::Stdout, |t| t.green()),
                from
            ),
            Some(failure) => println!(
                "  {} {} {} {}",
                "✗".if_supports_color(Stream::Stdout, |t| t.red()),
                from,
                "—".if_supports_color(Stream::Stdout, |t| t.dimmed()),
                failure
                    .to_string()
                    .if_supports_color(Stream::Stdout, |t| t.red())
            ),
        }
    }
}

/// The `sendra test` counterpart to [`print_assertions`] for a request that
/// declared none:
///
/// ```text
/// no assertions
/// ```
///
/// In the same position an `assertions` heading would occupy, dimmed and with
/// no `✓`/`✗` under it, because it is the absence of results rather than a
/// result. It exists so the summary's `without assertions` count has something
/// to point at: without it, the only way to find which request was uncovered
/// would be to notice which one printed nothing.
///
/// `sendra run` does not print it. A request with no assertions has always
/// produced byte-for-byte the output it produced before assertions existed, and
/// that stays true.
///
/// A request with a `post_request` script does not print it either, whatever
/// its `assertions` block says: it *was* checked, the block above says how it
/// went, and the summary counts it as a pass or a failure rather than as one of
/// the uncovered.
pub(super) fn print_no_assertions() {
    println!();
    println!(
        "{}",
        "no assertions".if_supports_color(Stream::Stdout, |t| t.dimmed())
    );
}

/// Print the counts `sendra test` ends a run with.
///
/// ```text
/// summary
///   5 requests: 2 passed, 1 failed, 1 without assertions, 1 no response
/// ```
///
/// The total and the passes are always shown; the other three appear only when
/// they are not zero, so a clean run reads `3 requests: 3 passed` and nothing
/// competes with it. That follows [`print_assertions`], which prints
/// `4 passed` and adds `, 2 failed` only when there is something to add — one
/// rule, applied at both levels, rather than a per-request line that hides its
/// zeroes under a summary line that spells them out.
///
/// Each count is coloured like what it counts: green for passes, red for the
/// two that fail the run, dimmed for the one that does not. `without
/// assertions` being dimmed rather than yellow is deliberate — it is not a
/// warning, it is a fact about what the file asked for.
///
/// Under the same dimmed heading style as the per-request `assertions` blocks,
/// and for the same reason: a response body can end in anything, and the
/// heading plus the blank line is what keeps the summary from reading as the
/// tail of whatever printed above it.
pub(super) fn print_summary(summary: &Summary) {
    println!();
    println!(
        "{}",
        "summary".if_supports_color(Stream::Stdout, |t| t.dimmed())
    );

    let mut counts = vec![format!("{} passed", summary.passed)
        .if_supports_color(Stream::Stdout, |t| t.green())
        .to_string()];

    if summary.failed > 0 {
        counts.push(
            format!("{} failed", summary.failed)
                .if_supports_color(Stream::Stdout, |t| t.red())
                .to_string(),
        );
    }
    if summary.without_assertions > 0 {
        counts.push(
            format!("{} without assertions", summary.without_assertions)
                .if_supports_color(Stream::Stdout, |t| t.dimmed())
                .to_string(),
        );
    }
    if summary.no_response > 0 {
        counts.push(
            format!("{} no response", summary.no_response)
                .if_supports_color(Stream::Stdout, |t| t.red())
                .to_string(),
        );
    }

    println!(
        "  {} {}: {}",
        summary.total,
        if summary.total == 1 {
            "request"
        } else {
            "requests"
        },
        counts.join(", ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::response_with;

    // --- Pretty-printing a JSON body ------------------------------------

    #[test]
    fn a_json_body_is_indented_for_the_terminal() {
        let response = response_with("application/json", r#"{"id":1,"tags":["a","b"]}"#);

        assert_eq!(
            body_for_display(&response),
            "{\n  \"id\": 1,\n  \"tags\": [\n    \"a\",\n    \"b\"\n  ]\n}"
        );
    }

    #[test]
    fn the_servers_key_order_survives_pretty_printing() {
        // Sorting the keys would make the output a rearrangement of the
        // response rather than the response — see the `preserve_order` feature
        // in Cargo.toml. These are in an order no sort would produce.
        let response = response_with("application/json", r#"{"zeta":1,"alpha":2,"mid":3}"#);

        assert_eq!(
            body_for_display(&response),
            "{\n  \"zeta\": 1,\n  \"alpha\": 2,\n  \"mid\": 3\n}"
        );
    }

    #[test]
    fn a_charset_parameter_does_not_stop_a_body_being_json() {
        let response = response_with("application/json; charset=utf-8", r#"{"a":1}"#);
        assert_eq!(body_for_display(&response), "{\n  \"a\": 1\n}");
    }

    #[test]
    fn a_plus_json_suffix_counts_as_json() {
        // RFC 6839: `application/problem+json`, `application/vnd.api+json` and
        // every other structured suffix are JSON, and a tool that only knew
        // `application/json` would print an error document raw.
        let response = response_with("application/problem+json", r#"{"title":"Nope"}"#);
        assert_eq!(body_for_display(&response), "{\n  \"title\": \"Nope\"\n}");

        // Upper case, because a media type is not case-sensitive.
        let response = response_with("APPLICATION/VND.API+JSON", r#"{"data":null}"#);
        assert_eq!(body_for_display(&response), "{\n  \"data\": null\n}");
    }

    #[test]
    fn a_body_that_claims_to_be_json_and_is_not_prints_raw() {
        // A truncated response under the right content-type: exactly when the
        // body is worth seeing, so it is printed as it arrived rather than
        // swallowed — and, above all, not a panic.
        let truncated = r#"{"id": 1, "name": "widg"#;
        let response = response_with("application/json", truncated);

        assert_eq!(body_for_display(&response), truncated);
    }

    #[test]
    fn an_empty_body_under_a_json_content_type_prints_raw() {
        // A `204` with `content-type: application/json` and nothing after it.
        // `print_response` does not reach this for an empty body, but the
        // fallback must hold on its own.
        let response = response_with("application/json", "");
        assert_eq!(body_for_display(&response), "");
    }

    #[test]
    fn a_body_that_is_not_declared_json_is_untouched() {
        // Even when it would parse. The content-type is the only input to this
        // decision; a text/plain body that happens to be JSON is text.
        let response = response_with("text/plain", r#"{"looks":"json"}"#);
        assert_eq!(body_for_display(&response), r#"{"looks":"json"}"#);

        let html = "<html><body>Not found</body></html>";
        let response = response_with("text/html; charset=utf-8", html);
        assert_eq!(body_for_display(&response), html);

        // A near miss that is not JSON: `application/json-seq` neither equals
        // `application/json` nor carries a `+json` suffix.
        let response = response_with("application/json-seq", r#"{"a":1}"#);
        assert_eq!(body_for_display(&response), r#"{"a":1}"#);
    }

    #[test]
    fn a_response_with_no_content_type_at_all_is_untouched() {
        let response = Response {
            headers: Vec::new(),
            ..response_with("text/plain", r#"{"a":1}"#)
        };
        assert_eq!(body_for_display(&response), r#"{"a":1}"#);
    }
}
