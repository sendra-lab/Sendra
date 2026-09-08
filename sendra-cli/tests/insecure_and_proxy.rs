//! End-to-end coverage for `--insecure`/`insecure:` and `--proxy`/`proxy:` —
//! the two invocation-level settings that reach
//! [`sendra_core::build_client`] through [`Config`](sendra_core::Config),
//! same tier as `--timeout` in [`cli_overrides`](cli_overrides.rs).
//!
//! `--insecure`'s tests run a real TLS handshake against a real self-signed
//! certificate — the same fixture `sendra-core`'s own `http::tests` use,
//! rebuilt here because it is `pub(crate)` test-only code in a different
//! crate and so unreachable from an integration test that only gets to spawn
//! the compiled binary. `--proxy`'s tests record the request line a mock
//! proxy actually receives — absolute-form, target URL and all — which is
//! what tells "the request went *through* the proxy" apart from "the request
//! reached some server that happened to answer 200".

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

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
        "the run should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "the run should fail: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// --- `--insecure` / `insecure:` -------------------------------------------

/// A server that terminates a TLS handshake with a self-signed certificate
/// for `127.0.0.1`, then answers one request with a fixed `200`. See the
/// module doc comment for why this duplicates `sendra-core`'s own copy.
fn start_self_signed_tls_server() -> SocketAddr {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let certified = rcgen::generate_simple_self_signed(["127.0.0.1".to_string()])
        .expect("a self-signed certificate for 127.0.0.1 generates");
    let cert_der = certified.cert.der().clone();
    let key_der =
        rustls::pki_types::PrivateKeyDer::Pkcs8(certified.key_pair.serialize_der().into());

    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("the freshly generated cert and key are valid together");
    let server_config = Arc::new(server_config);

    let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
    let addr = listener.local_addr().expect("the listener has an address");

    std::thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        let Ok(mut conn) = rustls::ServerConnection::new(server_config) else {
            return;
        };
        let mut tls = rustls::Stream::new(&mut conn, &mut sock);

        let mut reader = BufReader::new(&mut tls);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        loop {
            let mut header = String::new();
            match reader.read_line(&mut header) {
                Ok(0) | Err(_) => return,
                Ok(_) if header == "\r\n" => break,
                Ok(_) => {}
            }
        }

        let _ = tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    });

    addr
}

#[test]
fn omitting_insecure_fails_verification_against_a_self_signed_endpoint() {
    let addr = start_self_signed_tls_server();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("method: GET\nurl: https://{addr}/\n"),
    )
    .unwrap();

    assert_failure(&sendra(dir.path(), &["run", "req.yaml"]));
}

#[test]
fn insecure_flag_allows_a_self_signed_endpoint_through() {
    let addr = start_self_signed_tls_server();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("method: GET\nurl: https://{addr}/\n"),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml", "--insecure"]));
}

#[test]
fn insecure_config_key_allows_a_self_signed_endpoint_through() {
    let addr = start_self_signed_tls_server();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::create_dir_all(dir.path().join(".sendra")).unwrap();
    std::fs::write(dir.path().join(".sendra/config.yaml"), "insecure: true\n").unwrap();
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("method: GET\nurl: https://{addr}/\n"),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));
}

#[test]
fn the_insecure_flag_prints_a_warning_but_is_not_suppressed_by_quiet() {
    let addr = start_self_signed_tls_server();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("method: GET\nurl: https://{addr}/\n"),
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml", "--insecure", "-q"]);
    assert_success(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--insecure") && stderr.to_lowercase().contains("certificate"),
        "-q must not suppress the security warning: {stderr}"
    );
}

#[test]
fn omitting_insecure_prints_no_warning() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: GET\nurl: https://example.com\n",
    )
    .unwrap();

    // `--dry-run` so this never touches the network — the point here is only
    // whether the warning prints, which is decided before any request is
    // built at all.
    let output = sendra(dir.path(), &["run", "req.yaml", "--dry-run"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("--insecure"),
        "no --insecure and no insecure: true — nothing to warn about: {stderr}"
    );
}

// --- `--proxy` / `proxy:` --------------------------------------------------

/// A server that records the request line of the one connection it accepts,
/// then answers `200` — a stand-in for an HTTP proxy. See the module doc
/// comment for why the absolute-form request line is what proves proxying
/// actually happened.
fn start_proxy_recording_server() -> (SocketAddr, Arc<Mutex<Option<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
    let addr = listener.local_addr().expect("the listener has an address");
    let seen = Arc::new(Mutex::new(None));

    let stored = seen.clone();
    std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let mut writer = stream.try_clone().expect("the socket clones");
        let mut reader = BufReader::new(stream);

        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        loop {
            let mut header = String::new();
            match reader.read_line(&mut header) {
                Ok(0) | Err(_) => return,
                Ok(_) if header == "\r\n" => break,
                Ok(_) => {}
            }
        }

        *stored.lock().unwrap() = Some(request_line.trim_end().to_string());
        let _ = writer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        let _ = writer.flush();
    });

    (addr, seen)
}

#[test]
fn proxy_flag_routes_the_request_through_the_proxy() {
    let (proxy_addr, seen) = start_proxy_recording_server();
    let dir = tempfile::tempdir().expect("a temporary directory");
    // A host nothing in this test binds or listens on: if this reached the
    // target directly rather than through the proxy, the run would fail to
    // connect instead of succeeding.
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: GET\nurl: http://example-target.invalid/widgets\n",
    )
    .unwrap();

    assert_success(&sendra(
        dir.path(),
        &[
            "run",
            "req.yaml",
            "--proxy",
            &format!("http://{proxy_addr}"),
        ],
    ));

    let request_line = seen
        .lock()
        .unwrap()
        .take()
        .expect("the proxy should have seen exactly one request");
    assert_eq!(
        request_line, "GET http://example-target.invalid/widgets HTTP/1.1",
        "the proxy did not see an absolute-form request line: {request_line:?}"
    );
}

#[test]
fn proxy_config_key_routes_the_request_through_the_proxy() {
    let (proxy_addr, seen) = start_proxy_recording_server();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::create_dir_all(dir.path().join(".sendra")).unwrap();
    std::fs::write(
        dir.path().join(".sendra/config.yaml"),
        format!("proxy: http://{proxy_addr}\n"),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: GET\nurl: http://example-target.invalid/widgets\n",
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));
    assert!(seen.lock().unwrap().is_some(), "the proxy saw nothing");
}

#[test]
fn proxy_flag_overrides_a_different_proxy_in_config() {
    // The config points at a proxy that does not exist; the flag points at
    // the real one. If the override did not win, this would fail to
    // connect rather than succeed.
    let (proxy_addr, seen) = start_proxy_recording_server();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::create_dir_all(dir.path().join(".sendra")).unwrap();
    std::fs::write(
        dir.path().join(".sendra/config.yaml"),
        "proxy: http://127.0.0.1:1\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: GET\nurl: http://example-target.invalid/widgets\n",
    )
    .unwrap();

    assert_success(&sendra(
        dir.path(),
        &[
            "run",
            "req.yaml",
            "--proxy",
            &format!("http://{proxy_addr}"),
        ],
    ));
    assert!(
        seen.lock().unwrap().is_some(),
        "the request must have gone through the --proxy override, not the config value"
    );
}

#[test]
fn a_malformed_proxy_url_is_a_reported_client_error() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: GET\nurl: https://example.com\n",
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml", "--proxy", "not a url"]);
    assert_failure(&output);
}

#[test]
fn omitting_proxy_reaches_the_target_directly() {
    // The negative control: with no proxy configured anywhere, a request
    // still reaches its actual target rather than going nowhere.
    let (server_addr, seen) = start_proxy_recording_server();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("method: GET\nurl: http://{server_addr}/direct\n"),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    let request_line = seen
        .lock()
        .unwrap()
        .take()
        .expect("the server saw a request");
    // Origin-form, not absolute-form: a direct request, not one routed
    // through a proxy.
    assert_eq!(request_line, "GET /direct HTTP/1.1");
}
