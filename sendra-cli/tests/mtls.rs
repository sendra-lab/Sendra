//! End-to-end coverage for `--client-cert`/`--client-key` and the
//! `client_cert:` config key — mutual TLS, same tier as
//! `--insecure`/`--proxy` in [`insecure_and_proxy`](insecure_and_proxy.rs).
//!
//! Runs a real TLS handshake against a real server that demands a client
//! certificate signed by a locally generated CA — the same fixture
//! `sendra-core`'s own `http::tests` use, rebuilt here because it is
//! `pub(crate)` test-only code in a different crate and so unreachable from
//! an integration test that only gets to spawn the compiled binary. The
//! server's own certificate is self-signed, exactly like
//! `insecure_and_proxy`'s fixture, so every test here also passes
//! `--insecure`/`insecure: true` — this file is about the *client*
//! certificate, not the server's.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

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

/// A server that terminates a TLS handshake demanding a client certificate
/// signed by a locally generated CA, then answers one request with a fixed
/// `200`. See the module doc comment for why this duplicates `sendra-core`'s
/// own copy, and that copy's doc comment for the full reasoning behind the
/// shape of the fixture.
///
/// Returns the server's address and the one client certificate/key pair
/// (PEM) it will accept.
fn start_mutual_tls_server() -> (SocketAddr, String, String) {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let server_certified = rcgen::generate_simple_self_signed(["127.0.0.1".to_string()])
        .expect("a self-signed certificate for 127.0.0.1 generates");
    let server_cert_der = server_certified.cert.der().clone();
    let server_key_der =
        rustls::pki_types::PrivateKeyDer::Pkcs8(server_certified.key_pair.serialize_der().into());

    let mut ca_params =
        rcgen::CertificateParams::new(Vec::new()).expect("no subject alt names cannot fail");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .key_usages
        .push(rcgen::KeyUsagePurpose::KeyCertSign);
    ca_params
        .key_usages
        .push(rcgen::KeyUsagePurpose::DigitalSignature);
    ca_params.key_usages.push(rcgen::KeyUsagePurpose::CrlSign);
    let ca_key = rcgen::KeyPair::generate().expect("key generation does not fail");
    let ca_cert = ca_params
        .self_signed(&ca_key)
        .expect("a self-signed CA certificate generates");

    let mut client_params =
        rcgen::CertificateParams::new(Vec::new()).expect("no subject alt names cannot fail");
    client_params
        .key_usages
        .push(rcgen::KeyUsagePurpose::DigitalSignature);
    client_params
        .extended_key_usages
        .push(rcgen::ExtendedKeyUsagePurpose::ClientAuth);
    let client_key = rcgen::KeyPair::generate().expect("key generation does not fail");
    let client_cert = client_params
        .signed_by(&client_key, &ca_cert, &ca_key)
        .expect("the client certificate is signed by the CA");
    let client_cert_pem = client_cert.pem();
    let client_key_pem = client_key.serialize_pem();

    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(ca_cert.der().clone())
        .expect("the CA certificate is well-formed DER");
    let client_verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .expect("a verifier with one trusted root builds");

    let server_config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(vec![server_cert_der], server_key_der)
        .expect("the freshly generated server cert and key are valid together");
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

    (addr, client_cert_pem, client_key_pem)
}

/// A minimal request file against `addr`, requiring `--insecure`/
/// `insecure: true` on every invocation in this file — see the module doc
/// comment.
fn write_request(dir: &Path, addr: SocketAddr) {
    std::fs::write(
        dir.join("req.yaml"),
        format!("method: GET\nurl: https://{addr}/\n"),
    )
    .unwrap();
}

#[test]
fn a_request_with_no_client_certificate_is_rejected_by_the_server() {
    let (addr, _cert, _key) = start_mutual_tls_server();
    let dir = tempfile::tempdir().expect("a temporary directory");
    write_request(dir.path(), addr);

    assert_failure(&sendra(dir.path(), &["run", "req.yaml", "--insecure"]));
}

#[test]
fn client_cert_flags_authenticate_successfully() {
    let (addr, cert, key) = start_mutual_tls_server();
    let dir = tempfile::tempdir().expect("a temporary directory");
    write_request(dir.path(), addr);
    let cert_path = dir.path().join("client.pem");
    let key_path = dir.path().join("client-key.pem");
    std::fs::write(&cert_path, cert).unwrap();
    std::fs::write(&key_path, key).unwrap();

    assert_success(&sendra(
        dir.path(),
        &[
            "run",
            "req.yaml",
            "--insecure",
            "--client-cert",
            cert_path.to_str().unwrap(),
            "--client-key",
            key_path.to_str().unwrap(),
        ],
    ));
}

#[test]
fn client_cert_config_key_authenticates_successfully() {
    let (addr, cert, key) = start_mutual_tls_server();
    let dir = tempfile::tempdir().expect("a temporary directory");
    write_request(dir.path(), addr);
    std::fs::create_dir_all(dir.path().join(".sendra")).unwrap();
    // Written *inside* `.sendra/`, with `cert`/`key` given as bare filenames:
    // proves config-relative paths resolve against `.sendra/`'s own
    // directory, not the process's cwd (which is `dir.path()` here, not
    // `.sendra/`) and not the project root.
    std::fs::write(dir.path().join(".sendra/client.pem"), cert).unwrap();
    std::fs::write(dir.path().join(".sendra/client-key.pem"), key).unwrap();
    std::fs::write(
        dir.path().join(".sendra/config.yaml"),
        "insecure: true\nclient_cert:\n  cert: ./client.pem\n  key: ./client-key.pem\n",
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));
}

#[test]
fn client_cert_flags_override_a_different_pair_in_config() {
    // The config points at cert/key files that do not exist; the flags point
    // at the real pair. If the override did not win, this would fail with a
    // missing-file error rather than succeed.
    let (addr, cert, key) = start_mutual_tls_server();
    let dir = tempfile::tempdir().expect("a temporary directory");
    write_request(dir.path(), addr);
    std::fs::create_dir_all(dir.path().join(".sendra")).unwrap();
    std::fs::write(
        dir.path().join(".sendra/config.yaml"),
        "insecure: true\nclient_cert:\n  cert: ./nope.pem\n  key: ./nope-key.pem\n",
    )
    .unwrap();
    let cert_path = dir.path().join("real-client.pem");
    let key_path = dir.path().join("real-client-key.pem");
    std::fs::write(&cert_path, cert).unwrap();
    std::fs::write(&key_path, key).unwrap();

    assert_success(&sendra(
        dir.path(),
        &[
            "run",
            "req.yaml",
            "--client-cert",
            cert_path.to_str().unwrap(),
            "--client-key",
            key_path.to_str().unwrap(),
        ],
    ));
}

#[test]
fn a_client_cert_path_on_the_command_line_resolves_relative_to_the_cwd_not_the_config_file() {
    // The cert/key files sit beside the process's cwd (`dir.path()`), not
    // beside `.sendra/config.yaml` — proving `--client-cert`/`--client-key`
    // resolve relative to the current working directory, unlike the
    // config-file form.
    let (addr, cert, key) = start_mutual_tls_server();
    let dir = tempfile::tempdir().expect("a temporary directory");
    write_request(dir.path(), addr);
    std::fs::write(dir.path().join("client.pem"), cert).unwrap();
    std::fs::write(dir.path().join("client-key.pem"), key).unwrap();

    assert_success(&sendra(
        dir.path(),
        &[
            "run",
            "req.yaml",
            "--insecure",
            "--client-cert",
            "./client.pem",
            "--client-key",
            "./client-key.pem",
        ],
    ));
}

#[test]
fn a_missing_client_cert_file_is_a_reported_error() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: GET\nurl: https://example.com\n",
    )
    .unwrap();

    let output = sendra(
        dir.path(),
        &[
            "run",
            "req.yaml",
            "--client-cert",
            "./nope.pem",
            "--client-key",
            "./nope-key.pem",
        ],
    );
    assert_failure(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nope.pem"),
        "the error should name the missing file: {stderr}"
    );
}

#[test]
fn a_client_cert_with_no_matching_key_is_a_reported_error() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: GET\nurl: https://example.com\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("client.pem"), "irrelevant").unwrap();

    let output = sendra(
        dir.path(),
        &["run", "req.yaml", "--client-cert", "./client.pem"],
    );
    assert_failure(&output);
}

#[test]
fn omitting_client_cert_is_unaffected_and_still_works_against_a_plain_server() {
    // The negative control: no `--client-cert`/`--client-key` anywhere, and
    // `--dry-run` so this never touches the network at all — the point is
    // only that resolving the config and building the client does not fail
    // when the feature is not used.
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: GET\nurl: https://example.com\n",
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml", "--dry-run"]));
}
