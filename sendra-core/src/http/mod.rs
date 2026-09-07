//! Sending a [`crate::Request`] over the wire: [`send`] and [`send_prepared`],
//! the client that carries them ([`client`]), and what comes back
//! ([`response`]).

pub mod client;
pub mod response;

use std::time::Instant;

use crate::config::Config;
use crate::error::SendraError;
use crate::http::client::HttpClient;
use crate::http::response::Response;
use crate::request::Request;

/// Send `request` under `config` and collect the full response.
///
/// The elapsed time covers connect, send and body read — i.e. what a user
/// waits for, not just time-to-first-byte.
///
/// `config` is a parameter rather than something resolved in here, and is not
/// optional, so that a caller cannot send a request without deciding what
/// configuration applies to it. Callers with nothing to apply pass
/// [`Config::default`], which is the same defaults resolution falls back to. It
/// contributes one thing here — default headers, merged by [`Config::apply`]
/// with the request winning ties. The other thing it decides, the timeout, was
/// applied when `client` was built; see [`build_client`](client::build_client).
///
/// `client` is borrowed rather than built here so that a run sending more than
/// one request sends them all down the same connection pool. See
/// [`build_client`](client::build_client) for what that is worth and where the client should come
/// from.
///
/// This is the whole pipeline in one call, for a caller that has no reason to
/// step between the two halves. A caller that does — one running a
/// `pre_request` script, which by definition is the *last* thing to touch the
/// request — applies the config itself and calls [`send_prepared`]. That is the
/// only reason the seam exists; see there.
pub async fn send(
    request: &Request,
    client: &HttpClient,
    config: &Config,
) -> Result<Response, SendraError> {
    // Everything below works from the merged request, so a config header is
    // validated and sent exactly like one written in the file.
    send_prepared(&config.apply(request), client).await
}

/// Send a request that is already exactly what should go over the wire.
///
/// Identical to [`send`] except that [`Config::apply`] is the caller's job and
/// has already happened. There is no `&Config` here at all: the only thing this
/// half ever read from it was the timeout, and that now lives in the `client`
/// it is handed.
///
/// It exists because of `pre_request`. The ordering the scripting feature is
/// built on puts the script strictly after the config and strictly before the
/// wire, and a script's most obvious use — *removing* a header the config
/// injected — only works if nothing re-merges the config afterwards. So the
/// seam has to be somewhere, and here it is named, and says in its own
/// signature that configuration is not its problem because it has already been
/// handled.
///
/// Prefer [`send`] unless there is something to do in between.
pub async fn send_prepared(
    request: &Request,
    client: &HttpClient,
) -> Result<Response, SendraError> {
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in &request.headers {
        let header_name = reqwest::header::HeaderName::try_from(name.as_str()).map_err(|e| {
            SendraError::InvalidHeader {
                name: name.clone(),
                reason: e.to_string(),
            }
        })?;
        let header_value = reqwest::header::HeaderValue::try_from(value.as_str()).map_err(|e| {
            SendraError::InvalidHeader {
                name: name.clone(),
                reason: e.to_string(),
            }
        })?;
        // `append`, not `insert`: `insert` replaces any existing value under
        // that name, which would silently drop every occurrence but the last
        // of a header this crate now allows to repeat.
        headers.append(header_name, header_value);
    }

    // Every failure below comes back as a `reqwest::Error`, and exactly one
    // kind of it is worth its own variant: the timeout, because it is the
    // only one Sendra itself caused. See `SendraError::Timeout`.
    let send_err = |source: reqwest::Error| {
        if source.is_timeout() {
            SendraError::Timeout {
                url: request.url.clone(),
                timeout: client.timeout,
                source,
            }
        } else {
            SendraError::Network {
                url: request.url.clone(),
                source,
            }
        }
    };

    let mut builder = client
        .inner
        .request(request.method.into(), &request.url)
        .headers(headers);
    if let Some(body) = &request.body {
        builder = builder.body(body.clone());
    }

    // Cleared here rather than trusted to already be empty — see
    // `RedirectLog`. This assumes `send_prepared` calls through one
    // `HttpClient` never overlap; a concurrent send through the same client
    // would race on this log and misattribute hops between requests. See
    // `RedirectLog`'s doc comment before changing that.
    client.redirects.lock().unwrap().clear();

    let started = Instant::now();
    let response = builder.send().await.map_err(send_err)?;
    let redirects = std::mem::take(&mut *client.redirects.lock().unwrap());

    let status = response.status();
    let header_pairs = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value
                    .to_str()
                    .unwrap_or("<non-utf8 header value>")
                    .to_owned(),
            )
        })
        .collect();
    let bytes = response.bytes().await.map_err(send_err)?;
    let elapsed = started.elapsed();

    Ok(Response {
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or("").to_owned(),
        headers: header_pairs,
        // Lossy by contract, and explicitly so: `.bytes()` then
        // `from_utf8_lossy`, rather than reqwest's `.text()`, which reaches
        // the same result by a route that reads like an accident. See the
        // note on `Response::body`.
        body: String::from_utf8_lossy(&bytes).into_owned(),
        elapsed,
        redirects,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::client::build_client;
    use crate::http::response::RedirectHop;
    use crate::test_support::{
        get, ok_bytes, ok_response, redirect_response, start_route_server, start_stalling_server,
        CountingServer, Stall,
    };
    use crate::{config, Method, SendraError};
    use std::collections::BTreeMap;
    use std::time::Duration;

    #[tokio::test]
    async fn invalid_header_name_is_reported_before_any_network_call() {
        let request = Request {
            name: None,
            method: Method::Get,
            // Port 1 on localhost: if we ever got as far as connecting, this
            // would surface as a Network error instead, which the assert catches.
            url: "http://127.0.0.1:1/".to_string(),
            headers: vec![("bad header".to_string(), "x".to_string())],
            query: Vec::new(),
            body: None,
            json: None,
            body_file: None,
            form: Vec::new(),
            multipart: Vec::new(),
            auth: None,
            assertions: None,
            pre_request: None,
            post_request: None,
            capture: None,
        };
        let config = Config::default();
        let client = build_client(&config).expect("a client builds");
        let err = send(&request, &client, &config)
            .await
            .expect_err("invalid header must error");
        assert!(
            matches!(err, SendraError::InvalidHeader { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn an_invalid_header_from_the_config_is_reported_the_same_way() {
        // A config default is merged in before validation, so a bad header name
        // in `.sendra/config.yaml` fails as loudly as one in a request file
        // rather than being dropped on the way to the wire.
        let request = Request {
            name: None,
            method: Method::Get,
            url: "http://127.0.0.1:1/".to_string(),
            headers: Vec::new(),
            query: Vec::new(),
            body: None,
            json: None,
            body_file: None,
            form: Vec::new(),
            multipart: Vec::new(),
            auth: None,
            assertions: None,
            pre_request: None,
            post_request: None,
            capture: None,
        };
        let config = Config {
            headers: BTreeMap::from([("bad header".to_string(), "x".to_string())]),
            ..Config::default()
        };
        let client = build_client(&config).expect("a client builds");
        let err = send(&request, &client, &config)
            .await
            .expect_err("invalid header must error");
        assert!(
            matches!(err, SendraError::InvalidHeader { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn one_client_sends_every_request_down_one_connection() {
        // The point of `build_client` being per-run rather than per-request,
        // stated as an observation a server can make: three requests, one
        // handshake.
        let server = CountingServer::start();
        let config = Config::default();
        let client = build_client(&config).expect("a client builds");

        for _ in 0..3 {
            let response = send(&get(&server.url()), &client, &config)
                .await
                .expect("the mock server answers");
            assert_eq!(response.status, 200);
        }

        assert_eq!(server.requests(), 3, "all three requests were served");
        assert_eq!(
            server.connections(),
            1,
            "three requests through one client must reuse one connection"
        );
    }

    #[tokio::test]
    async fn a_client_per_request_opens_a_connection_per_request() {
        // The counterpart, and the reason the test above is worth anything: it
        // is what the code did before the client was hoisted out of
        // `send_prepared`, and it is what the counter looks like when a client
        // is *not* reused. Without this, a server that closed connections on
        // its own would make the assertion above pass for the wrong reason.
        let server = CountingServer::start();
        let config = Config::default();

        for _ in 0..3 {
            let client = build_client(&config).expect("a client builds");
            let response = send(&get(&server.url()), &client, &config)
                .await
                .expect("the mock server answers");
            assert_eq!(response.status, 200);
        }

        assert_eq!(server.requests(), 3, "all three requests were served");
        assert_eq!(
            server.connections(),
            3,
            "a fresh client per request cannot reuse anything"
        );
    }

    #[tokio::test]
    async fn a_gzip_encoded_response_is_decompressed_before_reaching_response_body() {
        // Many APIs compress their response regardless of what the client
        // negotiated; without the "gzip" feature enabled on the client, this
        // response body would be handed to `Response.body` as raw compressed
        // bytes rather than the JSON text they hold.
        use std::io::Write;

        let body = b"{\"hello\":\"world\"}";
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(body).expect("gzip encodes into memory");
        let compressed = encoder.finish().expect("gzip stream finalises");

        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let addr = listener.local_addr().expect("the listener has an address");
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};

            if let Ok(stream) = listener.accept().map(|(s, _)| s) {
                let mut writer = stream.try_clone().expect("the socket clones");
                let mut reader = BufReader::new(stream);

                let mut line = String::new();
                reader.read_line(&mut line).expect("a request line arrives");
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).expect("headers keep coming");
                    if header == "\r\n" {
                        break;
                    }
                }

                writer
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
                            compressed.len()
                        )
                        .as_bytes(),
                    )
                    .expect("status line and headers write");
                writer
                    .write_all(&compressed)
                    .expect("the compressed body writes");
                writer.flush().expect("the response flushes");
            }
        });

        let config = Config::default();
        let client = build_client(&config).expect("a client builds");
        let response = send(&get(&format!("http://{addr}/")), &client, &config)
            .await
            .expect("the mock server answers");

        assert_eq!(response.status, 200);
        assert_eq!(
            response.body, "{\"hello\":\"world\"}",
            "the body must be the decompressed text, not the raw gzip bytes"
        );
    }

    #[tokio::test]
    async fn a_repeated_header_actually_goes_out_twice_on_the_wire() {
        // Confirms the bytes a real server receives, not just that
        // `Request.headers` holds two entries: `send_prepared` has to use
        // `HeaderMap::append` rather than `insert`, or the second value would
        // silently replace the first before anything hits a socket.
        use std::io::{BufRead, BufReader, Write};

        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let addr = listener.local_addr().expect("the listener has an address");
        let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let seen_in_thread = seen.clone();
        std::thread::spawn(move || {
            if let Ok(stream) = listener.accept().map(|(s, _)| s) {
                let mut writer = stream.try_clone().expect("the socket clones");
                let mut reader = BufReader::new(stream);

                let mut line = String::new();
                reader.read_line(&mut line).expect("a request line arrives");
                loop {
                    let mut header = String::new();
                    match reader.read_line(&mut header) {
                        Ok(0) | Err(_) => return,
                        Ok(_) if header == "\r\n" => break,
                        Ok(_) => seen_in_thread
                            .lock()
                            .unwrap()
                            .push(header.trim_end().to_string()),
                    }
                }

                writer
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .expect("status line and headers write");
                writer.flush().expect("the response flushes");
            }
        });

        let request = Request {
            headers: vec![
                ("X-Forwarded-For".to_string(), "1.2.3.4".to_string()),
                ("X-Forwarded-For".to_string(), "5.6.7.8".to_string()),
            ],
            ..get(&format!("http://{addr}/"))
        };
        let config = Config::default();
        let client = build_client(&config).expect("a client builds");
        let response = send(&request, &client, &config)
            .await
            .expect("the mock server answers");
        assert_eq!(response.status, 200);

        let lines = seen.lock().unwrap().clone();
        let matching: Vec<&String> = lines
            .iter()
            .filter(|line| line.to_ascii_lowercase().starts_with("x-forwarded-for:"))
            .collect();
        assert_eq!(
            matching.len(),
            2,
            "both values should have gone out as two separate header lines, got {lines:?}"
        );
        assert!(matching.iter().any(|l| l.contains("1.2.3.4")));
        assert!(matching.iter().any(|l| l.contains("5.6.7.8")));
    }

    // --- timeouts ----------------------------------------------------------

    /// Comfortably longer than any timeout these tests configure: the server
    /// is still holding the connection when the assertions run.
    const STALL: Duration = Duration::from_secs(30);

    #[tokio::test]
    async fn a_server_slower_than_the_timeout_fails_with_a_timeout_error() {
        // The timeout has only ever been checked as a resolved `Config` value.
        // This is it applied: a server that never answers, and a client that
        // stops waiting on its own.
        let addr = start_stalling_server(Stall::BeforeResponding, STALL);
        let config = Config {
            timeout: Duration::from_millis(300),
            ..Config::default()
        };
        let client = build_client(&config).expect("a client builds");
        let url = format!("http://{addr}/");

        let started = Instant::now();
        let err = send(&get(&url), &client, &config)
            .await
            .expect_err("a server that never answers must not hang the run");
        let waited = started.elapsed();

        match &err {
            SendraError::Timeout {
                url: got, timeout, ..
            } => {
                assert_eq!(got, &url);
                assert_eq!(
                    *timeout,
                    Duration::from_millis(300),
                    "the error must name the limit that was actually applied"
                );
            }
            other => panic!("expected a timeout, got {other:?}"),
        }

        // The message a user sees, rather than only the variant a front-end
        // matches on: "failed" alone would not tell them a setting caused it.
        assert_eq!(
            err.to_string(),
            format!("request to `{url}` timed out after 0.3s")
        );

        // The clock that fired was the client's, not the server's: the server
        // is still asleep, and has another twenty-nine-odd seconds to go.
        assert!(
            waited < STALL / 2,
            "gave up after {waited:?}, which is not the configured 300ms"
        );
    }

    #[tokio::test]
    async fn the_timeout_covers_the_body_read_not_just_the_response_headers() {
        // The config calls this a whole-request timeout, so a server that
        // sends its headers promptly and then stalls forever mid-body has to
        // be caught too — a different await in `send_prepared`, and one that
        // would quietly return `Network` if only the first were classified.
        let addr = start_stalling_server(Stall::MidBody, STALL);
        let config = Config {
            timeout: Duration::from_millis(300),
            ..Config::default()
        };
        let client = build_client(&config).expect("a client builds");

        let started = Instant::now();
        let err = send(&get(&format!("http://{addr}/")), &client, &config)
            .await
            .expect_err("a body that never arrives must time out like a response that never does");
        let waited = started.elapsed();

        assert!(
            matches!(err, SendraError::Timeout { .. }),
            "a stall after the headers is still a timeout, got {err:?}"
        );
        assert!(waited < STALL / 2, "gave up after {waited:?}");
    }

    #[tokio::test]
    async fn a_timeout_from_a_config_file_is_the_one_that_is_enforced() {
        // The half config-resolution tests cannot reach: that the number
        // written in `.sendra/config.yaml` is the number the socket obeys.
        // Resolved from a real file on disk, exactly as a run would, then put
        // against a server that never answers.
        let temp = tempfile::tempdir().expect("a temp dir");
        let project_dir = temp.path().join(".sendra");
        std::fs::create_dir_all(&project_dir).expect("the project dir is created");
        std::fs::write(project_dir.join("config.yaml"), "timeout_seconds: 1\n")
            .expect("the config file writes");

        let config = Config::resolve_from(temp.path(), None).expect("the config resolves");
        assert_eq!(config.timeout, Duration::from_secs(1), "the file was read");

        let addr = start_stalling_server(Stall::BeforeResponding, STALL);
        let client = build_client(&config).expect("a client builds");

        let started = Instant::now();
        let err = send(&get(&format!("http://{addr}/")), &client, &config)
            .await
            .expect_err("the configured second must run out");
        let waited = started.elapsed();

        match err {
            SendraError::Timeout { timeout, .. } => assert_eq!(timeout, Duration::from_secs(1)),
            other => panic!("expected a timeout, got {other:?}"),
        }
        assert!(
            waited >= Duration::from_millis(900),
            "gave up after {waited:?}, sooner than the second the file asked for"
        );
        assert!(waited < STALL / 2, "gave up after {waited:?}");
    }

    #[tokio::test]
    async fn a_connection_failure_is_still_a_network_error_not_a_timeout() {
        // The counterpart that makes the variant above worth having: if every
        // failed send came back as `Timeout`, the split would say nothing. A
        // port with nothing behind it refuses immediately, so this is a
        // connection failure and cannot be a slow one.
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let addr = listener.local_addr().expect("the listener has an address");
        drop(listener);

        let config = Config {
            timeout: Duration::from_secs(30),
            ..Config::default()
        };
        let client = build_client(&config).expect("a client builds");
        let err = send(&get(&format!("http://{addr}/")), &client, &config)
            .await
            .expect_err("nothing is listening on that port");

        assert!(
            matches!(err, SendraError::Network { .. }),
            "a refused connection is a fact about the network, not about the timeout, got {err:?}"
        );
    }

    // --- non-UTF-8 response bodies -----------------------------------------

    #[tokio::test]
    async fn invalid_utf8_in_a_body_is_replaced_rather_than_erroring() {
        // `Response.body` is a `String`, so bytes that are not UTF-8 have to
        // go somewhere. They are replaced, and this pins exactly what with:
        // U+FFFD per invalid sequence, the surrounding text untouched, and no
        // error — see the contract on `Response::body`.
        //
        // 0xFF and 0xFE cannot begin a UTF-8 sequence at all, and 0xE2 0x28 is
        // a truncated three-byte sequence: the shape a body cut off at the
        // wrong boundary actually has.
        let body = b"ok \xff\xfe then \xe2\x28 end";
        let addr = start_route_server(vec![("/", ok_bytes("text/plain", body))]);

        let config = Config::default();
        let client = build_client(&config).expect("a client builds");
        let response = send(&get(&format!("http://{addr}/")), &client, &config)
            .await
            .expect("an undecodable body is not a failed request");

        assert_eq!(response.status, 200, "the response itself is fine");
        assert_eq!(
            response.body, "ok \u{fffd}\u{fffd} then \u{fffd}( end",
            "each invalid sequence becomes one replacement character, and the \
             valid text around it survives unchanged"
        );
    }

    #[tokio::test]
    async fn a_wholly_binary_body_comes_back_as_a_response_not_an_error() {
        // The everyday case: an endpoint that answers with an image. Status,
        // headers and elapsed time are all still true and worth showing, so
        // the response comes back rather than the request failing over its
        // body's encoding.
        //
        // A PNG signature, whose second byte (0x50, 'P') is deliberately
        // printable — proof the substitution is per invalid sequence and not a
        // blanket rewrite of the whole body.
        let body: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let addr = start_route_server(vec![("/", ok_bytes("image/png", body))]);

        let config = Config::default();
        let client = build_client(&config).expect("a client builds");
        let response = send(&get(&format!("http://{addr}/")), &client, &config)
            .await
            .expect("a binary body is not a failed request");

        assert_eq!(response.status, 200);
        assert_eq!(
            response
                .headers
                .iter()
                .find(|(name, _)| name == "content-type")
                .map(|(_, value)| value.as_str()),
            Some("image/png"),
            "everything but the body is unaffected"
        );
        assert_eq!(response.body, "\u{fffd}PNG\r\n\u{1a}\n");

        // Stated as a test rather than only as a doc comment, because it is
        // the part that bites: what comes back is not what was sent, and no
        // caller can recover the original bytes from here.
        assert_ne!(
            response.body.as_bytes(),
            body,
            "the conversion is lossy, and `Response.body` is not round-trippable"
        );
    }

    // --- redirect handling -------------------------------------------------

    #[tokio::test]
    async fn a_redirect_is_followed_and_the_chain_is_captured_on_the_final_response() {
        let addr = start_route_server(vec![
            (
                "/start",
                redirect_response(301, "Moved Permanently", "/next"),
            ),
            ("/next", redirect_response(302, "Found", "/end")),
            ("/end", ok_response("done")),
        ]);

        let config = Config::default();
        let client = build_client(&config).expect("a client builds");
        let response = send(&get(&format!("http://{addr}/start")), &client, &config)
            .await
            .expect("the chain resolves");

        // The final response is what Sendra reports as *the* response...
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "done");

        // ...and the chain that got there is captured alongside it, oldest
        // hop first, each carrying the status that redirected and the
        // location it pointed at, resolved to an absolute URL.
        assert_eq!(
            response.redirects,
            vec![
                RedirectHop {
                    status: 301,
                    location: format!("http://{addr}/next"),
                },
                RedirectHop {
                    status: 302,
                    location: format!("http://{addr}/end"),
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_request_with_no_redirect_reports_an_empty_chain() {
        // The overwhelmingly common case: nothing about an ordinary response
        // should look any different from before this feature existed.
        let addr = start_route_server(vec![("/", ok_response("hello"))]);

        let config = Config::default();
        let client = build_client(&config).expect("a client builds");
        let response = send(&get(&format!("http://{addr}/")), &client, &config)
            .await
            .expect("a plain response");

        assert_eq!(response.status, 200);
        assert!(response.redirects.is_empty());
    }

    #[tokio::test]
    async fn disabling_redirects_reports_the_3xx_response_itself_not_an_error() {
        let addr = start_route_server(vec![
            (
                "/start",
                redirect_response(301, "Moved Permanently", "/end"),
            ),
            ("/end", ok_response("done")),
        ]);

        let config = Config {
            redirects: config::FollowRedirects::Disabled,
            ..Config::default()
        };
        let client = build_client(&config).expect("a client builds");
        let response = send(&get(&format!("http://{addr}/start")), &client, &config)
            .await
            .expect("a 3xx is a normal, inspectable response");

        // The redirect itself is what came back — status, Location header and
        // all — not the response at the far end of it.
        assert_eq!(response.status, 301);
        assert_eq!(
            response
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("location"))
                .map(|(_, value)| value.as_str()),
            Some("/end")
        );
        // No chain: this response is not the result of following anything.
        assert!(response.redirects.is_empty());
    }

    #[tokio::test]
    async fn a_chain_longer_than_the_configured_maximum_is_an_error() {
        // Three hops to reach `/end`; a maximum of one allows the first and
        // must refuse the second.
        let addr = start_route_server(vec![
            ("/start", redirect_response(301, "Moved Permanently", "/a")),
            ("/a", redirect_response(302, "Found", "/b")),
            ("/b", redirect_response(303, "See Other", "/end")),
            ("/end", ok_response("done")),
        ]);

        let config = Config {
            redirects: config::FollowRedirects::Follow(1),
            ..Config::default()
        };
        let client = build_client(&config).expect("a client builds");
        let err = send(&get(&format!("http://{addr}/start")), &client, &config)
            .await
            .expect_err("a chain past the configured maximum must not resolve to a response");

        match err {
            SendraError::Network { source, .. } => {
                let message = source.to_string();
                assert!(
                    message.contains("redirect") || std::error::Error::source(&source).is_some(),
                    "expected a redirect-shaped error, got {message}"
                );
            }
            other => panic!("expected Network, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_custom_maximum_higher_than_the_chain_still_resolves() {
        // The other side of the same setting: a maximum generous enough for
        // the chain still reaches the end and still reports every hop.
        let addr = start_route_server(vec![
            ("/start", redirect_response(301, "Moved Permanently", "/a")),
            ("/a", redirect_response(302, "Found", "/end")),
            ("/end", ok_response("done")),
        ]);

        let config = Config {
            redirects: config::FollowRedirects::Follow(5),
            ..Config::default()
        };
        let client = build_client(&config).expect("a client builds");
        let response = send(&get(&format!("http://{addr}/start")), &client, &config)
            .await
            .expect("two hops is well within a maximum of five");

        assert_eq!(response.status, 200);
        assert_eq!(response.redirects.len(), 2);
    }

    #[tokio::test]
    async fn each_request_through_a_reused_client_reports_only_its_own_chain() {
        // The client — and its redirect log — is built once per run and
        // reused by every request; a chain from an earlier request must not
        // bleed into a later one that had none of its own.
        let addr = start_route_server(vec![
            (
                "/redirected",
                redirect_response(301, "Moved Permanently", "/plain"),
            ),
            ("/plain", ok_response("done")),
        ]);

        let config = Config::default();
        let client = build_client(&config).expect("a client builds");

        let redirected = send(&get(&format!("http://{addr}/redirected")), &client, &config)
            .await
            .expect("the redirect resolves");
        assert_eq!(redirected.redirects.len(), 1);

        let plain = send(&get(&format!("http://{addr}/plain")), &client, &config)
            .await
            .expect("a direct hit on the same client");
        assert!(
            plain.redirects.is_empty(),
            "the previous request's chain must not leak into this one"
        );
    }
}
