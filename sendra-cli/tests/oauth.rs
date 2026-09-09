//! End-to-end coverage for `auth: oauth` — acquiring a `client_credentials`
//! or `password` grant token from a real (mock) token endpoint and sending
//! it as `Authorization: Bearer <token>`, exactly like `auth: bearer`
//! already does.
//!
//! A hand-rolled server answers two routes: `/token` (the OAuth token
//! endpoint, counted and recorded separately) and everything else (the
//! actual API request, whose `Authorization` header is captured) — the same
//! pattern [`auth`](auth.rs) uses for a single endpoint, extended to two.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// One HTTP request as the server saw it.
#[derive(Clone)]
struct Captured {
    headers: Vec<(String, String)>,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// A server with a `/token` route (answering a fixed raw HTTP response,
/// swappable mid-test, and counted) and a catch-all `/api` route (answering
/// a fixed 200 and recording every request it receives, in order).
struct OAuthServer {
    addr: SocketAddr,
    token_hits: Arc<AtomicUsize>,
    api_requests: Arc<Mutex<Vec<Captured>>>,
}

impl OAuthServer {
    /// `token_response` is the raw HTTP response `/token` answers every hit
    /// with — a fixed body is enough for every test here: none of them need
    /// the endpoint's answer to change mid-run, since the cache-vs-reacquire
    /// behaviour is proven by counting hits, not by varying the response.
    fn start(token_response: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let addr = listener.local_addr().expect("the listener has an address");
        let token_hits = Arc::new(AtomicUsize::new(0));
        let api_requests = Arc::new(Mutex::new(Vec::new()));

        let (hits, requests) = (token_hits.clone(), api_requests.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let mut writer = stream.try_clone().expect("the socket clones");
                let mut reader = BufReader::new(stream);

                // One accepted TCP connection can carry more than one HTTP
                // request: `sendra`'s `HttpClient` is built once per run and
                // reused for every send (HTTP/1.1 keep-alive), so the OAuth
                // token request and the API request that follows it are
                // very likely to go out over the *same* pooled connection
                // once both endpoints share a host:port, as they do here.
                // Reading only one request per accepted connection (and then
                // letting `reader`/`writer` drop, which closes the socket)
                // would leave that pooled connection dangling from the
                // server's side — visible to the client as a reset the next
                // time it tried to reuse it. Loop until the client actually
                // hangs up, the same pattern `start_route_server` in
                // `sendra-core` already uses for exactly this reason.
                loop {
                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                        break;
                    }
                    let path = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_string();

                    let mut headers = Vec::new();
                    let mut content_length = 0usize;
                    loop {
                        let mut line = String::new();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) if line == "\r\n" => break,
                            Ok(_) => {
                                if let Some((name, value)) = line.trim_end().split_once(':') {
                                    let value = value.trim_start().to_string();
                                    if name.eq_ignore_ascii_case("content-length") {
                                        content_length = value.parse().unwrap_or(0);
                                    }
                                    headers.push((name.to_string(), value));
                                }
                            }
                        }
                    }
                    let mut body = vec![0u8; content_length];
                    if content_length > 0 {
                        let _ = reader.read_exact(&mut body);
                    }

                    if path.starts_with("/token") {
                        hits.fetch_add(1, Ordering::SeqCst);
                        if writer.write_all(&token_response).is_err() {
                            break;
                        }
                    } else {
                        requests.lock().unwrap().push(Captured { headers });
                        if writer
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                            .is_err()
                        {
                            break;
                        }
                    }
                    if writer.flush().is_err() {
                        break;
                    }
                }
            }
        });

        Self {
            addr,
            token_hits,
            api_requests,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn token_hits(&self) -> usize {
        self.token_hits.load(Ordering::SeqCst)
    }

    fn api_requests(&self) -> Vec<Captured> {
        self.api_requests.lock().unwrap().clone()
    }
}

fn ok_token_response(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn sendra(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sendra"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("the binary under test runs")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "the run should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "the run should fail: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn client_credentials_acquires_a_token_and_authenticates() {
    let server = OAuthServer::start(ok_token_response(
        r#"{"access_token": "cc-token", "token_type": "Bearer"}"#,
    ));
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/api\nauth:\n  oauth:\n    grant_type: client_credentials\n    token_url: {}/token\n    client_id: my-client\n    client_secret: my-secret\n",
            server.base_url(),
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    let requests = server.api_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].header("authorization"), Some("Bearer cc-token"));
}

#[test]
fn password_grant_acquires_a_token_and_authenticates() {
    let server = OAuthServer::start(ok_token_response(r#"{"access_token": "pwd-token"}"#));
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/api\nauth:\n  oauth:\n    grant_type: password\n    token_url: {}/token\n    client_id: my-client\n    client_secret: my-secret\n    username: ada\n    password: s3cr3t\n",
            server.base_url(),
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    let requests = server.api_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer pwd-token")
    );
}

#[test]
fn a_token_is_acquired_once_and_reused_across_a_collection() {
    let server = OAuthServer::start(ok_token_response(r#"{"access_token": "shared-token"}"#));
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "requests:\n\
             {}",
            (1..=3)
                .map(|n| format!(
                    "  - name: Req{n}\n    method: GET\n    url: {}/api/{n}\n    auth:\n      oauth:\n        grant_type: client_credentials\n        token_url: {}/token\n        client_id: my-client\n        client_secret: my-secret\n",
                    server.base_url(),
                    server.base_url()
                ))
                .collect::<String>()
        ),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    assert_eq!(
        server.token_hits(),
        1,
        "three requests sharing one oauth config must acquire exactly one token"
    );
    let requests = server.api_requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(request.header("authorization"), Some("Bearer shared-token"));
    }
}

#[test]
fn a_failed_acquisition_is_a_clear_error_and_the_run_fails() {
    let server = OAuthServer::start(
        b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 20\r\n\r\n{\"error\":\"denied\"}\r\n"
            .to_vec(),
    );
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/api\nauth:\n  oauth:\n    grant_type: client_credentials\n    token_url: {}/token\n    client_id: bad\n    client_secret: bad\n",
            server.base_url(),
            server.base_url()
        ),
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml"]);
    assert_failure(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("OAuth") || stderr.contains("401"),
        "got {stderr}"
    );
    assert!(
        server.api_requests().is_empty(),
        "the request must never be sent when its token could not be acquired"
    );
}

#[test]
fn a_broken_oauth_config_shared_by_several_requests_fails_fast_after_the_first() {
    let server = OAuthServer::start(ok_token_response("not json"));
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "requests:\n\
             {}",
            (1..=3)
                .map(|n| format!(
                    "  - name: Req{n}\n    method: GET\n    url: {}/api/{n}\n    auth:\n      oauth:\n        grant_type: client_credentials\n        token_url: {}/token\n        client_id: my-client\n        client_secret: my-secret\n",
                    server.base_url(),
                    server.base_url()
                ))
                .collect::<String>()
        ),
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml"]);
    assert_failure(&output);

    assert_eq!(
        server.token_hits(),
        1,
        "a config that already failed once in this run must not be retried against the \
         token endpoint for its siblings"
    );
    assert!(server.api_requests().is_empty());
}

#[test]
fn environment_level_oauth_applies_to_a_request_with_no_auth_of_its_own() {
    let server = OAuthServer::start(ok_token_response(r#"{"access_token": "env-token"}"#));
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::create_dir_all(dir.path().join(".sendra/environments")).unwrap();
    std::fs::write(
        dir.path().join(".sendra/environments/default.yaml"),
        format!(
            "auth:\n  oauth:\n    grant_type: client_credentials\n    token_url: {}/token\n    client_id: my-client\n    client_secret: my-secret\n",
            server.base_url()
        ),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("method: GET\nurl: {}/api\n", server.base_url()),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    let requests = server.api_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer env-token")
    );
}

#[test]
fn environment_level_oauth_is_shared_across_requests_exactly_like_request_level() {
    let server = OAuthServer::start(ok_token_response(r#"{"access_token": "env-shared"}"#));
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::create_dir_all(dir.path().join(".sendra/environments")).unwrap();
    std::fs::write(
        dir.path().join(".sendra/environments/default.yaml"),
        format!(
            "auth:\n  oauth:\n    grant_type: client_credentials\n    token_url: {}/token\n    client_id: my-client\n    client_secret: my-secret\n",
            server.base_url()
        ),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "requests:\n\
             {}",
            (1..=2)
                .map(|n| format!(
                    "  - name: Req{n}\n    method: GET\n    url: {}/api/{n}\n",
                    server.base_url()
                ))
                .collect::<String>()
        ),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    assert_eq!(
        server.token_hits(),
        1,
        "an environment-level oauth default must be cached and reused exactly like a \
         request-level one"
    );
}

#[test]
fn a_request_level_oauth_config_overrides_the_environments_default_entirely() {
    let server = OAuthServer::start(ok_token_response(
        r#"{"access_token": "should-not-be-used"}"#,
    ));
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::create_dir_all(dir.path().join(".sendra/environments")).unwrap();
    std::fs::write(
        dir.path().join(".sendra/environments/default.yaml"),
        format!(
            "auth:\n  oauth:\n    grant_type: client_credentials\n    token_url: {}/token\n    client_id: env-client\n    client_secret: env-secret\n",
            server.base_url()
        ),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/api\nauth:\n  bearer: from-the-request\n",
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    assert_eq!(
        server.token_hits(),
        0,
        "a request's own auth must fully replace the environment's oauth default, not run \
         alongside it"
    );
    let requests = server.api_requests();
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer from-the-request")
    );
}

#[test]
fn setting_both_oauth_and_bearer_is_rejected() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: GET\nurl: https://example.com\nauth:\n  bearer: x\n  oauth:\n    grant_type: client_credentials\n    token_url: https://example.com/token\n    client_id: id\n    client_secret: secret\n",
    )
    .unwrap();

    assert_failure(&sendra(dir.path(), &["run", "req.yaml"]));
}

#[test]
fn oauth_password_grant_missing_username_is_rejected() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: GET\nurl: https://example.com\nauth:\n  oauth:\n    grant_type: password\n    token_url: https://example.com/token\n    client_id: id\n    client_secret: secret\n    password: pw\n",
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml"]);
    assert_failure(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("username"), "got {stderr}");
}

#[test]
fn repeat_shares_one_acquired_token_across_every_pass() {
    let server = OAuthServer::start(ok_token_response(r#"{"access_token": "repeat-token"}"#));
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/api\nauth:\n  oauth:\n    grant_type: client_credentials\n    token_url: {}/token\n    client_id: my-client\n    client_secret: my-secret\n",
            server.base_url(),
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml", "--repeat", "3"]));

    assert_eq!(
        server.token_hits(),
        1,
        "--repeat is still one invocation: the token cache must persist across every pass, \
         not reset per pass"
    );
    assert_eq!(server.api_requests().len(), 3);
}
