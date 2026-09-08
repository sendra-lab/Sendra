//! End-to-end coverage for `sendra import curl`.
//!
//! Two kinds of test live here. Most exercise the compiled binary directly —
//! stdin vs. the positional argument, `-o`/`--output`, the unsupported-flag
//! and invocation-note reporting on stderr, a malformed command — the way
//! every other CLI-level test file in this crate does (see
//! [`mtls`](mtls.rs)). The last two are the feature's real proof, per the
//! issue this was built against: a realistic curl command is run for real,
//! against a server that records exactly what it received, and the file
//! `sendra import curl` generates from that same command is run against a
//! second instance of the same server — proving the generated file produces
//! the same wire behavior as curl itself, not just YAML that "looks right".
//! Everything specific to *converting* a command — the flag-by-flag
//! behavior, the JSON-detection heuristic, the round trip through Sendra's
//! own parser — is unit-tested in `sendra-cli/src/import/curl.rs`, against
//! the pure converter, rather than repeated here through a spawned process.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

fn sendra(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sendra"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("the binary under test runs")
}

fn sendra_with_stdin(dir: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sendra"))
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary under test spawns");
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(stdin.as_bytes())
        .expect("writing to the child's stdin succeeds");
    child
        .wait_with_output()
        .expect("the binary under test runs to completion")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "should fail: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// --- CLI-level behavior: argument vs. stdin, -o, and error reporting -----

#[test]
fn a_curl_command_given_as_an_argument_is_converted_to_stdout() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let output = sendra(
        dir.path(),
        &[
            "import",
            "curl",
            "curl -X POST https://api.example.com/users",
        ],
    );
    assert_success(&output);

    let yaml = String::from_utf8_lossy(&output.stdout);
    assert!(yaml.contains("method: POST"), "got {yaml}");
    assert!(
        yaml.contains("url: https://api.example.com/users"),
        "got {yaml}"
    );

    // What stdout produces must itself be a valid request file.
    let request_path = dir.path().join("req.yaml");
    std::fs::write(&request_path, yaml.as_bytes()).unwrap();
    assert_success(&sendra(dir.path(), &["run", "req.yaml", "--dry-run"]));
}

#[test]
fn omitting_the_command_reads_it_from_stdin() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let output = sendra_with_stdin(
        dir.path(),
        &["import", "curl"],
        "curl https://api.example.com/ping",
    );
    assert_success(&output);
    let yaml = String::from_utf8_lossy(&output.stdout);
    assert!(
        yaml.contains("url: https://api.example.com/ping"),
        "got {yaml}"
    );
}

#[test]
fn dash_o_writes_the_generated_file_instead_of_stdout() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let output = sendra(
        dir.path(),
        &[
            "import",
            "curl",
            "curl https://api.example.com/ping",
            "-o",
            "generated.yaml",
        ],
    );
    assert_success(&output);
    assert!(
        output.stdout.is_empty(),
        "the YAML went to the file, not stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let written =
        std::fs::read_to_string(dir.path().join("generated.yaml")).expect("the file was written");
    assert!(written.contains("url: https://api.example.com/ping"));

    // The written file is itself a valid request file.
    assert_success(&sendra(dir.path(), &["run", "generated.yaml", "--dry-run"]));
}

#[test]
fn a_malformed_command_is_a_clear_error_not_a_garbage_file() {
    let dir = tempfile::tempdir().expect("a temporary directory");

    // An unterminated quote: cannot be tokenized as a shell command at all.
    let output = sendra(
        dir.path(),
        &[
            "import",
            "curl",
            "curl https://example.com -d \"unterminated",
        ],
    );
    assert_failure(&output);
    assert!(output.stdout.is_empty(), "no file on a conversion failure");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("error:"), "got {stderr}");
}

#[test]
fn a_command_with_no_url_is_a_clear_error() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let output = sendra(dir.path(), &["import", "curl", "curl -X POST"]);
    assert_failure(&output);
    assert!(output.stdout.is_empty());
}

#[test]
fn insecure_is_reported_as_an_invocation_level_note_and_still_succeeds() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let output = sendra(
        dir.path(),
        &["import", "curl", "curl -k https://api.example.com/"],
    );
    assert_success(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--insecure") && stderr.contains("sendra run"),
        "the note should point at the equivalent invocation flag: {stderr}"
    );
    // Not treated as a dropped/unsupported flag — it has a real answer.
    assert!(!stderr.contains("not converted"), "got {stderr}");
}

#[test]
fn a_genuinely_unrecognized_flag_is_reported_and_still_succeeds() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let output = sendra(
        dir.path(),
        &[
            "import",
            "curl",
            "curl --some-flag-sendra-does-not-know https://api.example.com/",
        ],
    );
    assert_success(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not converted") && stderr.contains("--some-flag-sendra-does-not-know"),
        "got {stderr}"
    );
}

#[test]
fn compressed_produces_no_note_at_all() {
    // Silent, on purpose: Sendra already negotiates compression by default,
    // so there is nothing to warn about — see the module doc comment on
    // `sendra-cli/src/import/curl.rs`.
    let dir = tempfile::tempdir().expect("a temporary directory");
    let output = sendra(
        dir.path(),
        &[
            "import",
            "curl",
            "curl --compressed https://api.example.com/",
        ],
    );
    assert_success(&output);
    assert!(
        output.stderr.is_empty(),
        "got {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// --- the real proof: wire-behavior equivalence with real curl ------------

/// One HTTP request as a server actually received it: the request line and
/// every header, verbatim (names as sent, not lower-cased — curl and Sendra
/// both send headers with the case they were given), plus the raw body.
struct Captured {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// A server that accepts exactly one connection, records the request it
/// received, and answers with a fixed `200`. Same shape as
/// `structured_body.rs`'s `CapturingServer`, plus the request line (method
/// and path), which this file's wire-equivalence tests need and that one
/// does not.
struct CapturingServer {
    addr: SocketAddr,
    captured: Arc<Mutex<Option<Captured>>>,
}

impl CapturingServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let addr = listener.local_addr().expect("the listener has an address");
        let captured = Arc::new(Mutex::new(None));

        let stored = captured.clone();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut writer = stream.try_clone().expect("the socket clones");
            let mut reader = BufReader::new(stream);

            let mut request_line = String::new();
            reader
                .read_line(&mut request_line)
                .expect("the request line reads");
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or_default().to_string();
            let path = parts.next().unwrap_or_default().to_string();

            let mut headers = Vec::new();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("a header line reads");
                if line == "\r\n" {
                    break;
                }
                let (name, value) = line
                    .trim_end()
                    .split_once(':')
                    .expect("a well-formed header line");
                let value = value.trim_start().to_string();
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.parse().unwrap_or(0);
                }
                headers.push((name.to_string(), value));
            }

            let mut body = vec![0u8; content_length];
            if content_length > 0 {
                reader.read_exact(&mut body).expect("the body reads");
            }

            *stored.lock().unwrap() = Some(Captured {
                method,
                path,
                headers,
                body,
            });

            writer
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .expect("the response writes");
            let _ = writer.flush();
        });

        Self { addr, captured }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn captured(&self) -> Captured {
        self.captured
            .lock()
            .unwrap()
            .take()
            .expect("the server should have received exactly one request")
    }
}

fn run_real_curl(args: &[String]) -> Output {
    Command::new("curl")
        .args(args)
        .output()
        .expect("a real `curl` binary is available on PATH")
}

/// Assert `sent` (either curl's or the generated file's request) carries
/// `expected` for every header in `must_match` — the headers this test
/// actually controls, rather than every header either client happens to add
/// on its own (`User-Agent: curl/8.x`, `Host`, curl's own default `Accept:
/// */*`, none of which Sendra is trying to reproduce byte-for-byte).
fn assert_headers_match(curl: &Captured, sendra: &Captured, must_match: &[&str]) {
    for name in must_match {
        assert_eq!(
            curl.header(name),
            sendra.header(name),
            "header `{name}` differs: curl={:?} sendra={:?}",
            curl.header(name),
            sendra.header(name)
        );
    }
}

#[test]
fn a_json_post_with_auth_and_a_custom_header_produces_the_same_request_as_curl() {
    let curl_server = CapturingServer::start();
    let sendra_server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");

    // The exact same flags, aimed at two different servers so each side can
    // be captured independently — see the module doc comment.
    let flags = [
        "-X".to_string(),
        "POST".to_string(),
        "-H".to_string(),
        "X-Trace-Id: abc-123".to_string(),
        "-u".to_string(),
        "ada:s3cr3t".to_string(),
        "--data-raw".to_string(),
        r#"{"item":"widget","quantity":3}"#.to_string(),
    ];

    let mut curl_args = flags.clone().to_vec();
    curl_args.push(format!("{}/orders", curl_server.base_url()));
    assert_success(&run_real_curl(&curl_args));

    let mut curl_command = vec!["curl".to_string()];
    curl_command.extend(flags.clone());
    curl_command.push(format!("{}/orders", sendra_server.base_url()));
    let curl_command = curl_command
        .iter()
        .map(|arg| shlex_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");

    let import_output = sendra(
        dir.path(),
        &["import", "curl", &curl_command, "-o", "req.yaml"],
    );
    assert_success(&import_output);

    // The JSON heuristic fires with no explicit Content-Type in this
    // command, so this is exactly the documented, deliberate deviation from
    // curl's own literal default (`application/x-www-form-urlencoded`) —
    // see `curl.rs`'s module doc comment. Confirmed here rather than
    // asserted against, since it means this proof's own Content-Type
    // headers are expected to differ, not a bug.
    let import_stderr = String::from_utf8_lossy(&import_output.stderr);
    assert!(
        import_stderr.contains("application/json"),
        "got {import_stderr}"
    );

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    let from_curl = curl_server.captured();
    let from_sendra = sendra_server.captured();

    assert_eq!(from_curl.method, "POST");
    assert_eq!(from_sendra.method, "POST");
    assert_eq!(from_curl.path, "/orders");
    assert_eq!(from_sendra.path, "/orders");
    assert_eq!(from_curl.body, from_sendra.body);
    assert_headers_match(&from_curl, &from_sendra, &["X-Trace-Id", "Authorization"]);
}

#[test]
fn a_plain_form_post_gets_curls_own_default_content_type() {
    let curl_server = CapturingServer::start();
    let sendra_server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");

    let curl_args = vec![
        "-d".to_string(),
        "name=ada&role=admin".to_string(),
        format!("{}/signup", curl_server.base_url()),
    ];
    assert_success(&run_real_curl(&curl_args));

    let curl_command = format!(
        "curl -d 'name=ada&role=admin' {}/signup",
        sendra_server.base_url()
    );
    assert_success(&sendra(
        dir.path(),
        &["import", "curl", &curl_command, "-o", "req.yaml"],
    ));
    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    let from_curl = curl_server.captured();
    let from_sendra = sendra_server.captured();

    assert_eq!(from_curl.method, "POST", "curl implies POST for -d");
    assert_eq!(from_sendra.method, "POST");
    assert_eq!(from_curl.body, from_sendra.body);
    // Not JSON-shaped, so this is the plain-body path: the generated file
    // must carry curl's own literal default Content-Type, unlike the
    // JSON-detected case above.
    assert_eq!(
        from_curl.header("Content-Type"),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(
        from_curl.header("Content-Type"),
        from_sendra.header("Content-Type")
    );
}

/// Minimal single-quote shell-quoting for building a curl command string to
/// hand to `sendra import curl` — good enough for the argument values this
/// test file actually produces (a JSON body, a header value, credentials),
/// none of which contain a single quote themselves.
fn shlex_quote(arg: &str) -> String {
    if arg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_.:/@".contains(c))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}
