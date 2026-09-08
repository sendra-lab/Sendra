//! Rhai ↔ domain marshalling: the script's view of a request and a response,
//! and reading a script's edits back into a [`Request`].

use std::collections::{BTreeMap, BTreeSet};

use rhai::{Dynamic, Map};

use crate::{Request, Response, SendraError};

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
/// **A script sees a map, even though `Request.headers` is a `Vec` that
/// allows a name to repeat.** This is a deliberate simplicity-over-completeness
/// call: map semantics are what makes an assignment or a `.remove()` mean the
/// obvious thing with no getter/setter write-back subtleties, and giving that
/// up in favour of a list-of-pairs API (as [`response_map`] uses, since a
/// response is read-only and never needs `["name"] = value` sugar) would make
/// every script that touches headers pay for the repeated-header case in
/// syntax, when in practice a script adding one already-uncommon header a
/// second time is rarer still.
///
/// A header name the file repeated therefore arrives here collapsed to its
/// **last** value, since the map has one entry per key. That collapse is
/// confined to what the script actually writes: the map is read back as a
/// *diff* against the view it was handed, so a name the script left alone
/// keeps every one of its original entries, in their original positions. See
/// [`merge_script_headers`], which does that fold. What remains is the one
/// irreducible cost of a map:
///
/// - A script that **writes** to a repeated name leaves one value there, the
///   one it wrote. `request.headers["Set-Cookie"] = "a";` on a request whose
///   file sent two `Set-Cookie` headers sends one afterwards, and
///   `request.headers["Set-Cookie"] = "a"; request.headers["Set-Cookie"] =
///   "b";` is one write of `"b"`, not two headers. A script cannot add a
///   second occurrence of a name; a genuinely repeated header belongs in the
///   YAML file, whose shape [`Request::headers`](crate::Request::headers)
///   documents.
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
pub(super) fn request_map(request: &Request) -> Dynamic {
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
/// with [`Scope::push_constant`](rhai::Scope::push_constant), so
/// `response.status = 200` is Rhai's "cannot modify a constant" error. Handing
/// over a mutable copy whose mutations were then discarded would be a silently
/// ignored input, which is the one thing Sendra refuses to have anywhere in
/// its schema.
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
pub(super) fn response_map(response: &Response) -> Dynamic {
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
pub(super) fn request_from_dynamic(
    original: &Request,
    value: Dynamic,
) -> Result<Request, SendraError> {
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

    let mut after = BTreeMap::new();
    for (name, value) in headers_map {
        let value = value.try_cast::<String>().ok_or_else(|| {
            invalid(format!(
                "`request.headers[\"{name}\"]` must be a string; call `.to_string()` on it"
            ))
        })?;
        after.insert(name.to_string(), value);
    }
    let headers = merge_script_headers(&original.headers, &after);

    Ok(Request {
        // Not the script's to change: see `request_map`.
        name: original.name.clone(),
        method: original.method,
        url,
        headers,
        // Not exposed to the script and carried through untouched — by the
        // time a `pre_request` script runs, `Request::resolve_query` has
        // already merged whatever `query` held into `url` above, so this is
        // already empty regardless of whether the request file used `query`
        // at all.
        query: original.query.clone(),
        body: string_field(&map, "body")?,
        // Not exposed to the script (see the allow-list above) and carried
        // through untouched — by the time a `pre_request` script runs,
        // `Request::resolve_body` has already turned whichever of these was
        // set into the plain `body` string above, so all four are already
        // `None`/empty here regardless of which field the request file used.
        // A script that wants to change the body changes `request.body`,
        // exactly as it always has.
        json: original.json.clone(),
        body_file: original.body_file.clone(),
        form: original.form.clone(),
        multipart: original.multipart.clone(),
        // Not exposed to the script and carried through untouched — by the
        // time a `pre_request` script runs, `Request::resolve_auth` has
        // already resolved `auth` into the `Authorization` header above and
        // cleared this field, so it is already `None` here regardless of
        // whether the request file used `auth` at all. A script that wants
        // to change authentication changes `request.headers["Authorization"]`,
        // exactly as it always has.
        auth: original.auth.clone(),
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
        // Not exposed to the script, and not the script's to change: how
        // many times a *send* is retried is a fact about the pipeline
        // running the script, not about the request the script is shaping.
        retry: original.retry,
    })
}

/// Fold the map a script left behind back into the request's ordered headers,
/// keeping every repeat the script did not actually touch.
///
/// The problem this solves: a script is handed a *map* of headers (see
/// [`request_map`]), and a map cannot hold two entries under one name, so a
/// header the file repeated arrives at the script already collapsed to its
/// last value. Writing that map straight back out would mean **any** script
/// dropped **every** repeated header in the request, including names it never
/// mentioned — a script that rewrites `X-Signature` has no business changing
/// what happens to three `Set-Cookie` headers beside it.
///
/// So the map is treated as a diff rather than as the new truth. `before` is
/// the collapsed view the script was handed, recomputed here from `original`
/// (which is exactly what [`request_map`] built, since it is a pure function
/// of the request), and each name in the map the script left behind falls into
/// one of three cases:
///
/// - **Unchanged** — the value is what the script was handed. The script never
///   wrote to this name, so its original entries are restored verbatim: every
///   repeat, each at the position it held in the file.
/// - **Changed, or new** — the script wrote here, so its single value is what
///   goes out, at the position the name's first occurrence held (or appended,
///   for a name the request did not have). This is the one place repetition is
///   lost, and it is lost only for a name the script genuinely wrote to, which
///   is the narrowest the cost can be while a script still sees a map at all.
/// - **Absent** — the script removed the name, so every occurrence of it goes.
///
/// Names are matched exactly, not case-insensitively: the script's map is
/// keyed by the string the file used, so `request.headers["accept"]` on a file
/// that wrote `Accept` adds a second header rather than editing the first.
/// That is what it did before this function existed, and changing it is a
/// separate decision about the scripting surface.
fn merge_script_headers(
    original: &[(String, String)],
    after: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    // The collapsed view the script was handed: last value wins, exactly as
    // `request_map` builds it.
    let mut before: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, value) in original {
        before.insert(name.as_str(), value.as_str());
    }

    let mut merged = Vec::with_capacity(original.len());
    let mut written: BTreeSet<&str> = BTreeSet::new();
    for (name, value) in original {
        let Some(script_value) = after.get(name.as_str()) else {
            // Removed by the script, and it stays removed — every occurrence.
            continue;
        };

        if before.get(name.as_str()).copied() == Some(script_value.as_str()) {
            // Untouched: this exact entry, where it always was.
            merged.push((name.clone(), value.clone()));
        } else if written.insert(name.as_str()) {
            // Written to: the script's one value, at the first position this
            // name held. Later occurrences of it are dropped, because the
            // script's map could not have described them.
            merged.push((name.clone(), script_value.clone()));
        }
    }

    // Names the script added. Appended in the map's own (alphabetical) order,
    // after everything the file itself asked for.
    for (name, value) in after {
        if !before.contains_key(name.as_str()) {
            merged.push((name.clone(), value.clone()));
        }
    }

    merged
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

#[cfg(test)]
mod tests {
    use super::super::test_support::{request, run_pre, with_pre_request};
    use super::*;

    // --- header round-tripping ---------------------------------------------

    #[test]
    fn a_repeated_header_the_script_never_touched_survives_intact() {
        // The regression that matters: a script writing to one header must not
        // disturb a repeated header beside it. The map the script is handed
        // cannot hold both values, so the fold back out has to treat that map
        // as a diff rather than as the new truth.
        let request = request(
            "method: GET\n\
             url: https://example.com\n\
             headers:\n  \
               Accept: application/json\n  \
               X-Forwarded-For:\n    - 1.2.3.4\n    - 5.6.7.8\n  \
               X-Trailing: last\n\
             pre_request: |\n  request.headers[\"X-Signature\"] = \"abc\";\n",
        );
        let sent = run_pre(&request).expect("the script should run");

        assert_eq!(
            sent.headers,
            vec![
                ("Accept".to_string(), "application/json".to_string()),
                ("X-Forwarded-For".to_string(), "1.2.3.4".to_string()),
                ("X-Forwarded-For".to_string(), "5.6.7.8".to_string()),
                ("X-Trailing".to_string(), "last".to_string()),
                ("X-Signature".to_string(), "abc".to_string()),
            ],
            "an untouched repeated header keeps both values, in file order, \
             and the script's own header is appended"
        );
    }

    #[test]
    fn a_repeated_header_collapses_only_when_the_script_writes_to_that_name() {
        // The narrow, legitimate cost: writing to a name is a write of one
        // value, because the script's map could not have described two. It
        // applies to this name and no other — `X-Other` beside it is
        // untouched and keeps both of its values.
        let request = request(
            "method: GET\n\
             url: https://example.com\n\
             headers:\n  \
               X-Forwarded-For:\n    - 1.2.3.4\n    - 5.6.7.8\n  \
               X-Other:\n    - a\n    - b\n\
             pre_request: |\n  request.headers[\"X-Forwarded-For\"] = \"9.9.9.9\";\n",
        );
        let sent = run_pre(&request).expect("the script should run");

        assert_eq!(
            sent.headers,
            vec![
                ("X-Forwarded-For".to_string(), "9.9.9.9".to_string()),
                ("X-Other".to_string(), "a".to_string()),
                ("X-Other".to_string(), "b".to_string()),
            ],
            "the written name collapses to the script's value, at the position \
             it held; the name beside it is untouched"
        );
    }

    #[test]
    fn removing_a_repeated_header_from_a_script_removes_every_occurrence() {
        let request = request(
            "method: GET\n\
             url: https://example.com\n\
             headers:\n  \
               X-Forwarded-For:\n    - 1.2.3.4\n    - 5.6.7.8\n  \
               Accept: application/json\n\
             pre_request: |\n  request.headers.remove(\"X-Forwarded-For\");\n",
        );
        let sent = run_pre(&request).expect("the script should run");

        assert_eq!(
            sent.headers,
            vec![("Accept".to_string(), "application/json".to_string())],
            "a removed name goes entirely, not just its last occurrence"
        );
    }

    #[test]
    fn a_script_that_writes_back_the_value_it_was_handed_changes_nothing() {
        // Assigning the same value is indistinguishable from not writing at
        // all — the diff is by value, and there is nothing else a map could
        // tell us. The repeated header survives, which is the useful reading:
        // the wire is unchanged because the request is unchanged.
        let request = request(
            "method: GET\n\
             url: https://example.com\n\
             headers:\n  X-Tag:\n    - one\n    - two\n\
             pre_request: |\n  request.headers[\"X-Tag\"] = request.headers[\"X-Tag\"];\n",
        );
        let sent = run_pre(&request).expect("the script should run");

        assert_eq!(
            sent.headers,
            vec![
                ("X-Tag".to_string(), "one".to_string()),
                ("X-Tag".to_string(), "two".to_string()),
            ]
        );
    }

    #[test]
    fn a_pre_request_script_writing_a_name_twice_writes_one_header() {
        // The same cost seen from the other side: two assignments to one name
        // are two writes to one map entry, so the second is what goes out. A
        // script cannot manufacture a repeated header, whether or not the file
        // already had one under that name — that shape belongs in the YAML.
        let request = with_pre_request(
            "request.headers[\"Set-Cookie\"] = \"a\";\n\
             request.headers[\"Set-Cookie\"] = \"b\";",
        );
        let sent = run_pre(&request).expect("the script should run");

        let cookies: Vec<&str> = sent
            .headers
            .iter()
            .filter(|(name, _)| name == "Set-Cookie")
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(cookies, vec!["b"], "a script cannot repeat a header name");
    }

    // --- the closed surface -------------------------------------------------

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

    // --- the request carried back out ---------------------------------------

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
