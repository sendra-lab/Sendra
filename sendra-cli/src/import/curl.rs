//! The curl-command-to-`Request` converter itself: [`convert`]. Pure — no
//! I/O, no printing — so it is testable against string input/output;
//! `sendra-cli/src/import/mod.rs` is the thin stdin/file wrapper around it.
//!
//! **Scope.** curl's flag surface is enormous; this covers the flags that
//! actually show up in a command copied from a bug report or a browser's
//! "copy as curl": `-X`/`--request`, `-H`/`--header`, `-d`/`--data`/
//! `--data-raw`/`--data-binary`, `-u`/`--user`, `-F`/`--form`,
//! `-b`/`--cookie`, `-A`/`--user-agent`, and the URL itself. `--compressed`
//! is accepted and silently ignored — Sendra already negotiates compression
//! by default, so ignoring it produces identical behavior, unlike every
//! other flag this does not convert. Everything else this command
//! recognizes but cannot turn into per-request YAML (`-k`/`--insecure`,
//! `-x`/`--proxy`, `--cert`/`--key`) becomes an [`Conversion::invocation_notes`]
//! entry pointing at the `sendra run`/`sendra test` flag that matches it.
//! Anything this command does not recognize at all is collected into
//! [`Conversion::unsupported_flags`] rather than silently dropped, matching
//! this project's standing rule of reporting every failure rather than
//! guessing past it.

use std::fmt;

use sendra_core::{Auth, BasicAuth, Method, MultipartPart, Request};

/// Short curl flags whose value may be attached directly to the flag letter
/// — `-XPOST`, not just `-X POST` — which is common enough in hand-typed
/// curl commands (`-X` especially) to be worth handling. Boolean short
/// flags are deliberately not in this set: splitting a combined boolean
/// group like `-sS` the same way would misread it as `-s` with the value
/// `"S"` instead of two separate flags.
const VALUE_TAKING_SHORT_FLAGS: &[char] = &['X', 'H', 'd', 'u', 'F', 'b', 'A', 'x'];

/// The result of converting one curl command: the request it describes, and
/// an honest account of what this command could not carry over.
#[derive(Debug)]
pub(crate) struct Conversion {
    pub(crate) request: Request,
    /// Flags recognized as corresponding to a `sendra run`/`sendra test`
    /// invocation flag rather than to anything a request *file* can hold —
    /// each entry already names the equivalent command.
    pub(crate) invocation_notes: Vec<String>,
    /// Flags (or other command-line pieces) this command does not know how
    /// to convert at all, exactly as they appeared on the command line.
    pub(crate) unsupported_flags: Vec<String>,
}

#[derive(Debug)]
pub(crate) enum CurlImportError {
    /// The command line could not be split into shell words at all — an
    /// unterminated quote, most likely.
    Tokenize,
    /// No URL was found among the command's arguments.
    NoUrl,
    /// A flag that takes a value was the last token, with nothing after it.
    MissingValue(String),
    /// A `-H`/`--header` value was not `Name: value`.
    MalformedHeader(String),
    /// `-X`/`--request` named something other than one of the methods
    /// Sendra can send.
    UnsupportedMethod(String),
    /// Both `--data`-family and `-F`/`--form` were set — two different body
    /// encodings curl itself does not support sending together either, and
    /// Sendra's request format has no way to represent both at once.
    ConflictingBody,
    /// The request this command built did not pass Sendra's own validation
    /// — caught here, by round-tripping the generated YAML back through
    /// [`Request::from_yaml_str`], rather than ever handing back YAML that
    /// does not parse.
    Invalid(sendra_core::SendraError),
}

impl fmt::Display for CurlImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CurlImportError::Tokenize => {
                write!(f, "could not parse this as a shell command line (check for an unterminated quote)")
            }
            CurlImportError::NoUrl => write!(f, "no URL found in the curl command"),
            CurlImportError::MissingValue(flag) => {
                write!(f, "`{flag}` expects a value but none was given")
            }
            CurlImportError::MalformedHeader(raw) => {
                write!(f, "`-H {raw}` is not `Name: value`")
            }
            CurlImportError::UnsupportedMethod(method) => write!(
                f,
                "`-X {method}` is not a method Sendra can send (GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS)"
            ),
            CurlImportError::ConflictingBody => write!(
                f,
                "both `--data`(-raw/-binary) and `-F`/`--form` were given; a request cannot send both a plain body and a multipart one"
            ),
            CurlImportError::Invalid(err) => {
                write!(f, "the converted request is not valid: {err}")
            }
        }
    }
}

impl std::error::Error for CurlImportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CurlImportError::Invalid(err) => Some(err),
            _ => None,
        }
    }
}

/// Split `flag=value`/`--flag=value` into its two halves, and a
/// value-taking short flag's attached form (`-XPOST`) the same way. Returns
/// the bare flag and `None` for everything else — including every boolean
/// flag, whose token is returned unchanged.
fn split_token(token: &str) -> (String, Option<String>) {
    if let Some(rest) = token.strip_prefix("--") {
        if let Some((name, value)) = rest.split_once('=') {
            return (format!("--{name}"), Some(value.to_string()));
        }
        return (token.to_string(), None);
    }

    if let Some(rest) = token.strip_prefix('-') {
        if let Some(first) = rest.chars().next() {
            if VALUE_TAKING_SHORT_FLAGS.contains(&first) && rest.len() > first.len_utf8() {
                return (
                    format!("-{first}"),
                    Some(rest[first.len_utf8()..].to_string()),
                );
            }
        }
    }

    (token.to_string(), None)
}

/// Parse one `-H`/`--header` value: `Name: value`. Mirrors `cli.rs`'s own
/// `-H` parser (same trimming rule), kept separate rather than shared
/// because that one returns a clap `Result<_, String>` for a `value_parser`
/// and this one needs a [`CurlImportError`].
fn parse_header(raw: &str) -> Result<(String, String), CurlImportError> {
    let (name, value) = raw
        .split_once(':')
        .ok_or_else(|| CurlImportError::MalformedHeader(raw.to_string()))?;
    let name = name.trim();
    if name.is_empty() {
        return Err(CurlImportError::MalformedHeader(raw.to_string()));
    }
    Ok((name.to_string(), value.trim().to_string()))
}

fn parse_method(raw: &str) -> Result<Method, CurlImportError> {
    match raw.to_ascii_uppercase().as_str() {
        "GET" => Ok(Method::Get),
        "POST" => Ok(Method::Post),
        "PUT" => Ok(Method::Put),
        "PATCH" => Ok(Method::Patch),
        "DELETE" => Ok(Method::Delete),
        "HEAD" => Ok(Method::Head),
        "OPTIONS" => Ok(Method::Options),
        _ => Err(CurlImportError::UnsupportedMethod(raw.to_string())),
    }
}

fn has_header(headers: &[(String, String)], name: &str) -> bool {
    headers
        .iter()
        .any(|(existing, _)| existing.eq_ignore_ascii_case(name))
}

/// A body only counts as JSON when it *looks* like a JSON document, not
/// merely when it happens to parse as one: bare curl data like `-d 5` or
/// `-d true` is valid JSON by [`serde_json`]'s own rules but is
/// overwhelmingly more likely to be a literal scalar body than an author
/// intending JSON, and misreading it would send `5` under `json:` — where
/// `Request::resolve_body` reserializes it — instead of verbatim.
fn looks_like_json(trimmed: &str) -> bool {
    trimmed.starts_with('{') || trimmed.starts_with('[')
}

/// Convert one curl command line into a Sendra [`Request`], collecting
/// everything it could not carry over rather than dropping it silently.
///
/// Pure: takes the command as a string, returns a [`Conversion`] or a
/// [`CurlImportError`]. See the module doc comment for the flags this
/// covers and why; see the doc comments on the flag-handling arms below for
/// the JSON-detection and `Content-Type`-inference decisions specifically.
pub(crate) fn convert(command: &str) -> Result<Conversion, CurlImportError> {
    // A curl command copied from a bug report or browser devtools is
    // usually spread across several lines with a trailing `\` marking each
    // line break — the same continuation a POSIX shell itself would join
    // back into one line before ever tokenizing it. `shlex` tokenizes a
    // line at a time and does not do this joining on its own, so it is done
    // here first: a `\` immediately followed by a newline (optionally
    // preceded by `\r`) is removed entirely, exactly as a shell removes it,
    // rather than left for `shlex` to turn into a stray empty token.
    let joined = command.replace("\\\r\n", " ").replace("\\\n", " ");
    let mut tokens = shlex::split(&joined).ok_or(CurlImportError::Tokenize)?;
    if tokens.first().map(String::as_str) == Some("curl") {
        tokens.remove(0);
    }

    let mut url: Option<String> = None;
    let mut method_override: Option<String> = None;
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut data_parts: Vec<String> = Vec::new();
    let mut data_used = false;
    let mut form_used = false;
    let mut multipart: Vec<MultipartPart> = Vec::new();
    let mut basic_auth: Option<(String, String)> = None;
    let mut cookie_parts: Vec<String> = Vec::new();
    let mut user_agent: Option<String> = None;
    let mut invocation_notes: Vec<String> = Vec::new();
    let mut unsupported_flags: Vec<String> = Vec::new();

    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i].clone();

        if token != "-" && !token.starts_with('-') {
            if url.is_none() {
                url = Some(token);
            } else {
                // curl accepts more than one URL and sends a request to
                // each in turn; Sendra's request file describes exactly one
                // request, so a second positional has nothing to become.
                unsupported_flags.push(format!("{token} (a second URL; only the first converts)"));
            }
            i += 1;
            continue;
        }

        let (flag, inline_value) = split_token(&token);

        let take_value = |i: &mut usize| -> Result<String, CurlImportError> {
            if let Some(value) = &inline_value {
                return Ok(value.clone());
            }
            *i += 1;
            tokens
                .get(*i)
                .cloned()
                .ok_or_else(|| CurlImportError::MissingValue(flag.clone()))
        };

        match flag.as_str() {
            "-X" | "--request" => method_override = Some(take_value(&mut i)?),
            "-H" | "--header" => headers.push(parse_header(&take_value(&mut i)?)?),
            "-d" | "--data" | "--data-raw" | "--data-binary" | "--data-ascii" => {
                let value = take_value(&mut i)?;
                data_used = true;
                match value.strip_prefix('@') {
                    Some(path) => unsupported_flags.push(format!(
                        "{flag} @{path} (reading the body from a file is not supported)"
                    )),
                    None => data_parts.push(value),
                }
            }
            "-u" | "--user" => {
                let value = take_value(&mut i)?;
                let (user, pass) = match value.split_once(':') {
                    Some((user, pass)) => (user.to_string(), pass.to_string()),
                    None => (value, String::new()),
                };
                basic_auth = Some((user, pass));
            }
            "-F" | "--form" => {
                let value = take_value(&mut i)?;
                form_used = true;
                match value.split_once('=') {
                    Some((name, part_value)) => match part_value.strip_prefix('@') {
                        Some(path) => multipart.push(MultipartPart {
                            name: name.to_string(),
                            value: None,
                            path: Some(path.to_string()),
                        }),
                        None => multipart.push(MultipartPart {
                            name: name.to_string(),
                            value: Some(part_value.to_string()),
                            path: None,
                        }),
                    },
                    None => unsupported_flags.push(format!(
                        "{flag} {value} (expected `name=value` or `name=@path`)"
                    )),
                }
            }
            "-b" | "--cookie" => {
                let value = take_value(&mut i)?;
                if value.contains('=') {
                    cookie_parts.push(value);
                } else {
                    unsupported_flags.push(format!(
                        "{flag} {value} (reading cookies from a cookie-jar file is not supported)"
                    ));
                }
            }
            "-A" | "--user-agent" => user_agent = Some(take_value(&mut i)?),
            "-k" | "--insecure" => invocation_notes.push(
                "curl used -k/--insecure; run the generated file with `sendra run --insecure` \
                 (or `sendra test --insecure`) to match."
                    .to_string(),
            ),
            "-x" | "--proxy" => {
                let value = take_value(&mut i)?;
                invocation_notes.push(format!(
                    "curl used -x/--proxy {value}; run the generated file with \
                     `sendra run --proxy {value}` to match."
                ));
            }
            "--cert" => {
                let value = take_value(&mut i)?;
                invocation_notes.push(format!(
                    "curl used --cert {value}; run the generated file with \
                     `sendra run --client-cert {value} --client-key <key>` to match."
                ));
            }
            "--key" => {
                let value = take_value(&mut i)?;
                invocation_notes.push(format!(
                    "curl used --key {value}; run the generated file with \
                     `sendra run --client-cert <cert> --client-key {value}` to match."
                ));
            }
            // A no-op: Sendra already negotiates response compression by
            // default (see the note on `Config`), so accepting and
            // ignoring this produces identical behavior — unlike every
            // other flag above and below it, which is why this is the one
            // silent case.
            "--compressed" => {}
            _ => match inline_value {
                Some(value) => unsupported_flags.push(format!("{flag} {value}")),
                None => unsupported_flags.push(flag),
            },
        }

        i += 1;
    }

    let url = url.ok_or(CurlImportError::NoUrl)?;

    if data_used && form_used {
        return Err(CurlImportError::ConflictingBody);
    }

    let method = match method_override {
        Some(raw) => parse_method(&raw)?,
        None if data_used || form_used => Method::Post,
        None => Method::Get,
    };

    // `-A`/`--user-agent` only sets the header when the command did not
    // already set one explicitly via `-H` — matching real curl, where an
    // explicit header always wins over the flag that would otherwise set a
    // default. Same precedence, same reasoning, for `-b`/`--cookie` below
    // and for `-u`/`--user` further down.
    if let Some(user_agent) = user_agent {
        if !has_header(&headers, "User-Agent") {
            headers.push(("User-Agent".to_string(), user_agent));
        }
    }

    if !cookie_parts.is_empty() && !has_header(&headers, "Cookie") {
        headers.push(("Cookie".to_string(), cookie_parts.join("; ")));
    }

    let mut body = None;
    let mut json = None;

    if !data_parts.is_empty() {
        let combined = data_parts.join("&");
        let trimmed = combined.trim();
        if looks_like_json(trimmed) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
                json = Some(value);
            }
        }

        if json.is_some() {
            // Real curl does not look at the data at all: an unadorned
            // `-d` always implies `Content-Type:
            // application/x-www-form-urlencoded`, whatever the bytes are.
            // This converter deviates from that literal behavior on
            // purpose when the data parses as JSON and no explicit
            // `Content-Type` was given — a JSON-shaped `-d` body is
            // overwhelmingly more likely to be a forgotten `-H
            // 'Content-Type: application/json'` than a deliberate form
            // body that happens to look like JSON, and `json:` reads
            // better than `body:` with a hand-set header either way. See
            // the module doc comment.
            if !has_header(&headers, "Content-Type") {
                invocation_notes.push(
                    "the request body parsed as JSON; this generated file sets `Content-Type: \
                     application/json` rather than curl's own default of \
                     `application/x-www-form-urlencoded` for `-d`. Pass an explicit `-H \
                     'Content-Type: ...'` in the curl command if you need curl's literal default."
                        .to_string(),
                );
            }
        } else {
            body = Some(combined);
            if !has_header(&headers, "Content-Type") {
                headers.push((
                    "Content-Type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                ));
            }
        }
    }

    let auth = match basic_auth {
        Some((user, pass)) if !has_header(&headers, "Authorization") => Some(Auth {
            bearer: None,
            basic: Some(BasicAuth { user, pass }),
            api_key: None,
        }),
        Some(_) => {
            // Both `-u` and an explicit `Authorization` header were given.
            // Real curl sends whichever header ends up in its internal
            // list, which is the explicit one — so this keeps only the
            // header, the same "explicit header wins" precedence as
            // `-A`/`-b` above, and says so, since silently dropping half of
            // what was typed is worth a word.
            invocation_notes.push(
                "curl used -u/--user together with an explicit `Authorization` header; keeping \
                 only the explicit header, matching curl's own precedence."
                    .to_string(),
            );
            None
        }
        None => None,
    };

    let request = Request {
        name: None,
        method,
        url,
        headers,
        query: Vec::new(),
        body,
        json,
        body_file: None,
        form: Vec::new(),
        multipart,
        auth,
        assertions: None,
        pre_request: None,
        post_request: None,
        capture: None,
        retry: None,
    };

    // The acceptance check this feature was built against: the generated
    // file must parse — and validate — through Sendra's own parser, not
    // merely look right. Round-tripping here means a bug in this converter
    // that produced e.g. a request with two body fields set is reported as
    // a conversion error rather than handed back as YAML that fails the
    // moment someone runs `sendra run` on it.
    let yaml = serde_yaml::to_string(&request).expect("a Request always serializes");
    Request::from_yaml_str(&yaml).map_err(CurlImportError::Invalid)?;

    Ok(Conversion {
        request,
        invocation_notes,
        unsupported_flags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert_ok(command: &str) -> Conversion {
        convert(command).unwrap_or_else(|err| panic!("`{command}` should convert: {err}"))
    }

    // --- the URL, and basic tokenization -----------------------------------

    #[test]
    fn a_bare_url_converts_to_a_get_request() {
        let conversion = convert_ok("curl https://api.example.com/users/1");
        assert_eq!(conversion.request.method, Method::Get);
        assert_eq!(conversion.request.url, "https://api.example.com/users/1");
        assert!(conversion.request.headers.is_empty());
        assert!(conversion.invocation_notes.is_empty());
        assert!(conversion.unsupported_flags.is_empty());
    }

    #[test]
    fn the_leading_curl_token_is_optional() {
        // A command handed over without the program name at all — e.g.
        // pasted from somewhere that already stripped it.
        let conversion = convert_ok("https://example.com");
        assert_eq!(conversion.request.url, "https://example.com");
    }

    #[test]
    fn a_quoted_argument_with_spaces_is_one_token_not_several() {
        let conversion = convert_ok(r#"curl https://example.com -H "Accept: application/json""#);
        assert_eq!(
            conversion.request.headers,
            vec![("Accept".to_string(), "application/json".to_string())]
        );
    }

    #[test]
    fn unterminated_quoting_is_a_clear_error_not_a_panic() {
        let err = convert(r#"curl https://example.com -d "unterminated"#)
            .expect_err("an unterminated quote cannot be tokenized");
        assert!(matches!(err, CurlImportError::Tokenize));
    }

    #[test]
    fn a_command_with_no_url_is_a_clear_error() {
        let err = convert("curl -X POST").expect_err("no URL means nothing to send to");
        assert!(matches!(err, CurlImportError::NoUrl));
    }

    // --- -X / --request ------------------------------------------------------

    #[test]
    fn dash_x_sets_the_method() {
        let conversion = convert_ok("curl -X PUT https://example.com");
        assert_eq!(conversion.request.method, Method::Put);

        let conversion = convert_ok("curl --request DELETE https://example.com");
        assert_eq!(conversion.request.method, Method::Delete);
    }

    #[test]
    fn dash_x_accepts_the_value_attached_to_the_flag() {
        // `-XPOST`, not `-X POST` — common enough in hand-typed commands to
        // support explicitly; see `split_token`.
        let conversion = convert_ok("curl -XPOST https://example.com");
        assert_eq!(conversion.request.method, Method::Post);
    }

    #[test]
    fn an_unrecognized_method_is_a_clear_error() {
        let err = convert("curl -X TELEPORT https://example.com")
            .expect_err("TELEPORT is not a method Sendra can send");
        assert!(matches!(err, CurlImportError::UnsupportedMethod(m) if m == "TELEPORT"));
    }

    #[test]
    fn no_dash_x_and_no_body_defaults_to_get() {
        let conversion = convert_ok("curl https://example.com");
        assert_eq!(conversion.request.method, Method::Get);
    }

    #[test]
    fn data_with_no_dash_x_implies_post_matching_real_curl() {
        let conversion = convert_ok("curl https://example.com -d 'a=1'");
        assert_eq!(conversion.request.method, Method::Post);
    }

    #[test]
    fn an_explicit_dash_x_is_not_overridden_by_data() {
        let conversion = convert_ok("curl -X PUT https://example.com -d 'a=1'");
        assert_eq!(conversion.request.method, Method::Put);
    }

    // --- -H / --header ---------------------------------------------------

    #[test]
    fn header_repeats_and_preserves_order() {
        let conversion = convert_ok(
            "curl https://example.com -H 'X-Trace-Id: abc' -H 'Accept: application/json'",
        );
        assert_eq!(
            conversion.request.headers,
            vec![
                ("X-Trace-Id".to_string(), "abc".to_string()),
                ("Accept".to_string(), "application/json".to_string()),
            ]
        );
    }

    #[test]
    fn a_malformed_header_is_a_clear_error() {
        let err = convert("curl https://example.com -H 'no-colon-here'")
            .expect_err("a header needs a colon");
        assert!(matches!(err, CurlImportError::MalformedHeader(_)));
    }

    #[test]
    fn a_header_missing_its_value_is_a_clear_error() {
        let err = convert("curl https://example.com -H").expect_err("`-H` with nothing after it");
        assert!(matches!(err, CurlImportError::MissingValue(f) if f == "-H"));
    }

    // --- -d / --data / --data-raw / --data-binary -------------------------

    #[test]
    fn plain_data_becomes_a_raw_body_with_the_curl_default_content_type() {
        let conversion = convert_ok("curl https://example.com -d 'a=1&b=2'");
        assert_eq!(conversion.request.body.as_deref(), Some("a=1&b=2"));
        assert_eq!(conversion.request.json, None);
        assert_eq!(
            conversion.request.header("Content-Type"),
            Some("application/x-www-form-urlencoded")
        );
    }

    #[test]
    fn an_explicit_content_type_is_not_overridden_by_the_default() {
        let conversion =
            convert_ok("curl https://example.com -H 'Content-Type: text/plain' -d 'hello'");
        assert_eq!(
            conversion.request.header("Content-Type"),
            Some("text/plain")
        );
    }

    #[test]
    fn repeated_data_flags_are_joined_with_ampersand_like_real_curl() {
        let conversion = convert_ok("curl https://example.com -d 'a=1' -d 'b=2'");
        assert_eq!(conversion.request.body.as_deref(), Some("a=1&b=2"));
    }

    #[test]
    fn data_raw_and_data_binary_behave_like_data() {
        let conversion = convert_ok("curl https://example.com --data-raw 'a=1'");
        assert_eq!(conversion.request.body.as_deref(), Some("a=1"));

        let conversion = convert_ok("curl https://example.com --data-binary 'a=1'");
        assert_eq!(conversion.request.body.as_deref(), Some("a=1"));
    }

    #[test]
    fn a_json_looking_body_is_emitted_as_json_not_a_raw_string() {
        let conversion = convert_ok(r#"curl https://example.com -d '{"name":"ada"}'"#);
        assert_eq!(conversion.request.body, None);
        assert_eq!(
            conversion.request.json,
            Some(serde_json::json!({"name": "ada"}))
        );
    }

    #[test]
    fn a_json_body_with_no_explicit_content_type_gets_application_json_and_a_note() {
        let conversion = convert_ok(r#"curl https://example.com -d '{"ok":true}'"#);
        assert_eq!(
            conversion.request.header("Content-Type"),
            None,
            "no header is written into the file; `json:` gets `application/json` \
             automatically when the request is resolved"
        );
        assert!(
            conversion
                .invocation_notes
                .iter()
                .any(|note| note.contains("application/json")),
            "the deliberate deviation from curl's own default must be explained: {:?}",
            conversion.invocation_notes
        );
    }

    #[test]
    fn a_json_body_with_an_explicit_content_type_keeps_it_and_prints_no_note() {
        let conversion = convert_ok(
            r#"curl https://example.com -H 'Content-Type: application/json' -d '{"ok":true}'"#,
        );
        assert_eq!(
            conversion.request.header("Content-Type"),
            Some("application/json")
        );
        assert!(
            conversion
                .invocation_notes
                .iter()
                .all(|note| !note.contains("application/json")),
            "an explicit Content-Type needs no explanation: {:?}",
            conversion.invocation_notes
        );
    }

    #[test]
    fn a_bare_json_scalar_is_not_misread_as_json() {
        // `-d 5` is valid JSON by serde_json's own rules but is far more
        // likely a literal form value than someone intending a JSON body —
        // see `looks_like_json`.
        let conversion = convert_ok("curl https://example.com -d '5'");
        assert_eq!(conversion.request.body.as_deref(), Some("5"));
        assert_eq!(conversion.request.json, None);
    }

    #[test]
    fn data_from_a_file_is_reported_as_unsupported_not_silently_wrong() {
        let conversion = convert_ok("curl https://example.com -d @payload.json");
        assert_eq!(
            conversion.request.body, None,
            "no body is fabricated from a path this converter never reads"
        );
        assert_eq!(
            conversion.request.method,
            Method::Post,
            "curl still implies POST"
        );
        assert!(conversion
            .unsupported_flags
            .iter()
            .any(|flag| flag.contains("payload.json")));
    }

    #[test]
    fn data_and_form_together_is_a_clear_conflict_error() {
        let err = convert("curl https://example.com -d 'a=1' -F 'b=2'")
            .expect_err("curl itself does not support sending both either");
        assert!(matches!(err, CurlImportError::ConflictingBody));
    }

    // --- -u / --user -------------------------------------------------------

    #[test]
    fn user_colon_pass_becomes_basic_auth() {
        let conversion = convert_ok("curl https://example.com -u ada:s3cr3t");
        let auth = conversion.request.auth.expect("auth was set");
        let basic = auth.basic.expect("basic, not bearer");
        assert_eq!(basic.user, "ada");
        assert_eq!(basic.pass, "s3cr3t");
    }

    #[test]
    fn user_with_no_colon_is_a_username_with_an_empty_password() {
        let conversion = convert_ok("curl https://example.com -u ada");
        let basic = conversion.request.auth.unwrap().basic.unwrap();
        assert_eq!(basic.user, "ada");
        assert_eq!(basic.pass, "");
    }

    #[test]
    fn user_alongside_an_explicit_authorization_header_keeps_only_the_header() {
        let conversion =
            convert_ok("curl https://example.com -u ada:s3cr3t -H 'Authorization: Bearer token'");
        assert_eq!(conversion.request.auth, None);
        assert_eq!(
            conversion.request.header("Authorization"),
            Some("Bearer token")
        );
        assert!(conversion
            .invocation_notes
            .iter()
            .any(|note| note.contains("-u/--user")));
    }

    // --- -F / --form -------------------------------------------------------

    #[test]
    fn form_name_value_becomes_a_text_multipart_part() {
        let conversion = convert_ok("curl https://example.com -F 'description=a cat'");
        assert_eq!(
            conversion.request.multipart,
            vec![MultipartPart {
                name: "description".to_string(),
                value: Some("a cat".to_string()),
                path: None,
            }]
        );
        assert_eq!(conversion.request.method, Method::Post);
    }

    #[test]
    fn form_name_at_path_becomes_a_file_multipart_part() {
        let conversion = convert_ok("curl https://example.com -F 'photo=@cat.jpg'");
        assert_eq!(
            conversion.request.multipart,
            vec![MultipartPart {
                name: "photo".to_string(),
                value: None,
                path: Some("cat.jpg".to_string()),
            }]
        );
    }

    #[test]
    fn a_malformed_form_part_is_reported_not_dropped() {
        let conversion = convert_ok("curl https://example.com -F 'no-equals-here'");
        assert!(conversion.request.multipart.is_empty());
        assert!(conversion
            .unsupported_flags
            .iter()
            .any(|flag| flag.contains("no-equals-here")));
    }

    // --- -b / --cookie -------------------------------------------------------

    #[test]
    fn cookie_name_value_becomes_a_cookie_header() {
        let conversion = convert_ok("curl https://example.com -b 'session=abc123'");
        assert_eq!(conversion.request.header("Cookie"), Some("session=abc123"));
    }

    #[test]
    fn repeated_cookie_flags_are_joined() {
        let conversion = convert_ok("curl https://example.com -b 'a=1' -b 'b=2'");
        assert_eq!(conversion.request.header("Cookie"), Some("a=1; b=2"));
    }

    #[test]
    fn a_cookie_jar_filename_is_reported_as_unsupported() {
        let conversion = convert_ok("curl https://example.com -b cookies.txt");
        assert_eq!(conversion.request.header("Cookie"), None);
        assert!(conversion
            .unsupported_flags
            .iter()
            .any(|flag| flag.contains("cookies.txt")));
    }

    // --- -A / --user-agent -------------------------------------------------

    #[test]
    fn user_agent_sets_the_header() {
        let conversion = convert_ok("curl https://example.com -A 'my-agent/1.0'");
        assert_eq!(
            conversion.request.header("User-Agent"),
            Some("my-agent/1.0")
        );
    }

    #[test]
    fn an_explicit_user_agent_header_wins_over_dash_a() {
        let conversion =
            convert_ok("curl https://example.com -A 'curl/8.0' -H 'User-Agent: custom/1.0'");
        assert_eq!(conversion.request.header("User-Agent"), Some("custom/1.0"));
    }

    // --- --compressed --------------------------------------------------------

    #[test]
    fn compressed_is_silently_accepted() {
        let conversion = convert_ok("curl https://example.com --compressed");
        assert!(
            conversion.unsupported_flags.is_empty(),
            "--compressed changes nothing Sendra does not already do by default"
        );
        assert!(conversion.invocation_notes.is_empty());
    }

    // --- invocation-level flags ---------------------------------------------

    #[test]
    fn insecure_is_reported_as_an_invocation_level_note() {
        let conversion = convert_ok("curl https://example.com -k");
        assert!(conversion.unsupported_flags.is_empty());
        assert!(conversion
            .invocation_notes
            .iter()
            .any(|note| note.contains("--insecure")));
    }

    #[test]
    fn proxy_is_reported_as_an_invocation_level_note_naming_the_url() {
        let conversion = convert_ok("curl https://example.com -x http://proxy:8080");
        assert!(conversion
            .invocation_notes
            .iter()
            .any(|note| note.contains("--proxy") && note.contains("http://proxy:8080")));
    }

    #[test]
    fn cert_and_key_are_reported_as_invocation_level_notes() {
        let conversion =
            convert_ok("curl https://example.com --cert client.pem --key client-key.pem");
        assert!(conversion
            .invocation_notes
            .iter()
            .any(|note| note.contains("--client-cert") && note.contains("client.pem")));
        assert!(conversion
            .invocation_notes
            .iter()
            .any(|note| note.contains("--client-key") && note.contains("client-key.pem")));
    }

    // --- genuinely unrecognized flags ---------------------------------------

    #[test]
    fn an_unrecognized_flag_is_collected_not_dropped() {
        let conversion = convert_ok("curl https://example.com --some-future-flag");
        assert_eq!(conversion.unsupported_flags, vec!["--some-future-flag"]);
    }

    #[test]
    fn several_unrecognized_flags_are_all_collected() {
        let conversion = convert_ok("curl https://example.com -L -s --fail");
        assert_eq!(conversion.unsupported_flags.len(), 3);
        assert!(conversion.unsupported_flags.contains(&"-L".to_string()));
        assert!(conversion.unsupported_flags.contains(&"-s".to_string()));
        assert!(conversion.unsupported_flags.contains(&"--fail".to_string()));
    }

    // --- the round trip ------------------------------------------------------

    #[test]
    fn every_converted_request_round_trips_through_sendras_own_parser() {
        for command in [
            "curl https://example.com",
            "curl -X POST https://example.com -H 'Accept: application/json' -d 'a=1&b=2'",
            r#"curl https://example.com -d '{"name":"ada","roles":["admin","user"]}'"#,
            "curl https://example.com -u ada:s3cr3t",
            "curl https://example.com -F 'a=1' -F 'photo=@cat.jpg'",
            "curl https://example.com -b 'a=1' -A 'agent/1.0' -k -x http://proxy:8080",
        ] {
            let conversion = convert_ok(command);
            let yaml = serde_yaml::to_string(&conversion.request).expect("serializes");
            Request::from_yaml_str(&yaml).unwrap_or_else(|err| {
                panic!("`{command}` produced YAML that does not parse: {err}\n{yaml}")
            });
        }
    }

    #[test]
    fn a_realistic_multi_flag_command_converts_correctly_as_one_whole() {
        // Roughly what a browser's "copy as cURL" produces for a JSON POST
        // with auth and a custom header.
        let command = r#"curl 'https://api.example.com/v1/orders' \
            -X POST \
            -H 'Accept: application/json' \
            -H 'Content-Type: application/json' \
            -H 'Authorization: Bearer abc123' \
            --data-raw '{"item":"widget","quantity":3}' \
            --compressed"#;

        let conversion = convert_ok(command);
        let request = conversion.request;

        assert_eq!(request.method, Method::Post);
        assert_eq!(request.url, "https://api.example.com/v1/orders");
        assert_eq!(request.header("Accept"), Some("application/json"));
        assert_eq!(request.header("Content-Type"), Some("application/json"));
        assert_eq!(request.header("Authorization"), Some("Bearer abc123"));
        assert_eq!(
            request.json,
            Some(serde_json::json!({"item": "widget", "quantity": 3}))
        );
        assert!(conversion.unsupported_flags.is_empty());
        assert!(conversion.invocation_notes.is_empty());
    }

    #[test]
    fn a_line_continuation_does_not_produce_a_stray_empty_argument() {
        // The `\` line-break a multi-line curl command uses must be joined
        // back into one line before tokenizing, the way a real shell would
        // — otherwise it is read as an empty positional and misread as a
        // second URL. See `convert`'s comment on this.
        let command = "curl https://example.com \\\n  -X POST \\\n  --compressed";
        let conversion = convert_ok(command);
        assert_eq!(conversion.request.url, "https://example.com");
        assert_eq!(conversion.request.method, Method::Post);
        assert!(
            conversion.unsupported_flags.is_empty(),
            "{:?}",
            conversion.unsupported_flags
        );
    }
}
