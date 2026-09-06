//! That a collection run opens one connection, not one per request.
//!
//! The unit tests in `sendra-core` prove that its `send` reuses whatever client
//! it is handed; this proves the binary actually hands it the same one. It is
//! the whole path — `prepare` builds the client, `run_requests` sends every
//! request through it — observed from the only place that can tell the
//! difference: a server counting the TCP connections it was asked to accept.
//!
//! `sendra run` and `sendra test` are both exercised, because they are two
//! different call sites of the same loop and only one of them being wired up
//! is exactly the bug this guards against.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tempfile::TempDir;

/// Three requests against one host: the ordinary case a connection pool is
/// for, and the case that used to pay three TLS handshakes.
const THREE_REQUESTS: &str = "\
requests:
  - name: First
    method: GET
    url: '{{base}}/first'
    assertions:
      status: 200
  - name: Second
    method: GET
    url: '{{base}}/second'
    assertions:
      status: 200
  - name: Third
    method: GET
    url: '{{base}}/third'
    assertions:
      status: 200
";

/// A server that counts connections and requests separately.
///
/// Hand-rolled over a blocking listener rather than pulled in as a mock-server
/// dependency: the thing being observed is below HTTP — whether a socket was
/// opened at all — and "read a request, answer it, keep the connection open"
/// is the entire protocol these tests need.
struct CountingServer {
    addr: SocketAddr,
    connections: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
}

impl CountingServer {
    /// Start on an ephemeral loopback port. The serving thread runs until the
    /// test binary exits, which is as long as any test needs it.
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let addr = listener.local_addr().expect("the listener has an address");
        let connections = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));

        let (served_connections, served_requests) = (connections.clone(), requests.clone());
        std::thread::spawn(move || {
            // One connection at a time: a sendra run sends its requests in file
            // order, one after the other, so nothing is waiting behind this.
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                served_connections.fetch_add(1, Ordering::SeqCst);

                let mut writer = stream.try_clone().expect("the socket clones");
                let mut reader = BufReader::new(stream);

                // Keep answering on this connection until the client hangs up.
                // A client reusing the pool sends its next request here; one
                // that rebuilt itself shows up back at `incoming` instead.
                loop {
                    let mut request_line = String::new();
                    match reader.read_line(&mut request_line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let mut headers_done = false;
                    while !headers_done {
                        let mut header = String::new();
                        match reader.read_line(&mut header) {
                            Ok(0) | Err(_) => return,
                            Ok(_) => headers_done = header == "\r\n",
                        }
                    }

                    // Counted before the response goes out, so by the time the
                    // client has read its last response this number is final.
                    served_requests.fetch_add(1, Ordering::SeqCst);
                    if writer.write_all(RESPONSE).is_err() {
                        break;
                    }
                    let _ = writer.flush();
                }
            }
        });

        Self {
            addr,
            connections,
            requests,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

/// A minimal keep-alive response: HTTP/1.1 with a content length, which is
/// what lets the client know where the body ends and reuse the connection.
const RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nok";

/// A project directory holding the collection and an environment pointing at
/// `server`, and nothing else — in particular no config left over from the
/// repository these tests were launched from.
fn project(server: &CountingServer) -> TempDir {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(dir.path().join("req.yaml"), THREE_REQUESTS).expect("the collection is written");
    std::fs::create_dir_all(dir.path().join(".sendra/environments"))
        .expect("the environment directory is created");
    std::fs::write(
        dir.path().join(".sendra/environments/default.yaml"),
        format!("base: {}\n", server.base_url()),
    )
    .expect("the environment is written");
    dir
}

fn sendra(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sendra"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("the binary under test runs")
}

#[test]
fn run_sends_a_whole_collection_down_one_connection() {
    let server = CountingServer::start();
    let dir = project(&server);

    let output = sendra(dir.path(), &["run", "req.yaml"]);
    assert!(
        output.status.success(),
        "the run should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(server.requests(), 3, "all three requests were sent");
    assert_eq!(
        server.connections(),
        1,
        "one client per run means one connection for the collection"
    );
}

#[test]
fn test_sends_a_whole_collection_down_one_connection() {
    // `test` is the other call site of the same loop; a client threaded into
    // `run` and not into `test` would pass the test above and still handshake
    // per request here.
    let server = CountingServer::start();
    let dir = project(&server);

    let output = sendra(dir.path(), &["test", "req.yaml"]);
    assert!(
        output.status.success(),
        "the assertions should hold: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(server.requests(), 3, "all three requests were sent");
    assert_eq!(
        server.connections(),
        1,
        "one client per run means one connection for the collection"
    );
}
