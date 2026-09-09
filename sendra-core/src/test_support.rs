//! Test-only fixtures shared across `http`'s test module: three hand-rolled
//! TCP servers (below `reqwest`, not just below HTTP semantics — see each
//! one's own doc comment for why it exists rather than a mock-server crate)
//! and the tiny [`Request`] builders every one of them is exercised through.
//!
//! The three servers are shaped differently on purpose, for three different
//! things being tested — a connection counter, a server that goes silent, and
//! one that answers a routing table — and are **not** forced into one
//! implementation. They do share one piece of literally identical code (the
//! "read a request line, then drain headers until the blank line" sequence),
//! factored out as [`drain_request_headers`] since extracting it changes
//! nothing about what any of them do. Everything else — whether a server
//! loops over more than one connection, whether it serves more than one
//! request per connection, what a read failure does to the surrounding loop —
//! was already different between them in the original flat test module, and
//! is kept exactly as different here.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::request::Method;
use crate::Request;

/// Read one request line off `reader`, then drain headers up to (and
/// including) the blank line that ends them.
///
/// Returns the request line (e.g. `"GET /path HTTP/1.1\r\n"`) once the whole
/// header block has been read, or `None` the moment either read hits EOF or
/// an error — matching what every one of these servers did inline before
/// this was pulled out: a read failure during either the request line or the
/// headers is always treated as "this connection is done", never as
/// something to retry.
fn drain_request_headers(reader: &mut BufReader<TcpStream>) -> Option<String> {
    let mut request_line = String::new();
    match reader.read_line(&mut request_line) {
        Ok(0) | Err(_) => return None,
        Ok(_) => {}
    }

    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) | Err(_) => return None,
            Ok(_) if header == "\r\n" => break,
            Ok(_) => {}
        }
    }

    Some(request_line)
}

/// A GET with nothing on it but a URL — what a collection of requests
/// against one host looks like once everything else is stripped away.
pub(crate) fn get(url: &str) -> Request {
    Request {
        name: None,
        method: Method::Get,
        url: url.to_string(),
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
        retry: None,
    }
}

/// A server that counts the TCP connections it is asked to accept, so a
/// test can tell "sent twice down one connection" from "connected twice".
///
/// Deliberately hand-rolled over a blocking `TcpListener` on its own
/// thread rather than pulled in as a mock-server dependency: what is being
/// observed here is below HTTP — whether a *socket* was opened — and the
/// whole protocol these tests need is "read a request, write a response,
/// keep the connection open", which is shorter than the configuration of a
/// library that does more.
///
/// **Does not use [`drain_request_headers`]**, unlike the other two servers
/// below: unlike them, this one treats a request-line EOF and a header EOF
/// differently on purpose (or at least, that is the behaviour it has always
/// had, carried over verbatim rather than normalized away by this move) — a
/// request-line EOF only `break`s the per-connection loop, so the listener
/// keeps accepting new connections, while a header EOF `return`s and ends
/// the whole server thread. Folding both into one helper that always treats
/// EOF the same way would quietly change which of the two happens.
pub(crate) struct CountingServer {
    addr: SocketAddr,
    connections: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
}

impl CountingServer {
    /// Start on an ephemeral loopback port and serve until the test ends.
    ///
    /// The thread is left running when the test finishes; it dies with the
    /// process, which is the whole lifetime a test binary has.
    pub(crate) fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let addr = listener.local_addr().expect("the listener has an address");
        let connections = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));

        let (server_connections, server_requests) = (connections.clone(), requests.clone());
        std::thread::spawn(move || {
            // One connection at a time, which is all a sendra run ever
            // opens: requests go out in file order, one after the other.
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                server_connections.fetch_add(1, Ordering::SeqCst);

                let mut writer = stream.try_clone().expect("the socket clones");
                let mut reader = BufReader::new(stream);

                // Keep reading requests off this connection until the
                // client hangs up: a client that is reusing the connection
                // sends its next request here rather than reconnecting.
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    // Drain the headers; these requests carry no body.
                    loop {
                        let mut header = String::new();
                        match reader.read_line(&mut header) {
                            Ok(0) | Err(_) => return,
                            Ok(_) if header == "\r\n" => break,
                            Ok(_) => {}
                        }
                    }

                    // Counted before the response is written, so a client
                    // that has read its last response has necessarily been
                    // counted by the time the test looks.
                    server_requests.fetch_add(1, Ordering::SeqCst);
                    if writer
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                        .is_err()
                    {
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

    pub(crate) fn url(&self) -> String {
        format!("http://{}/", self.addr)
    }

    pub(crate) fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    pub(crate) fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

/// Where a slow server stops, relative to the response it owes.
///
/// The configured timeout is a *whole-request* one — connect, send and
/// body read — and `send_prepared` can therefore fail at either of two
/// awaits. These are those two places, so both are exercised rather than
/// assumed equivalent.
pub(crate) enum Stall {
    /// Read the request and then say nothing at all: what an overloaded
    /// server that has not started work yet looks like.
    BeforeResponding,
    /// Send the status line and a `Content-Length` promising a body, then
    /// stop without sending it: headers arrive, `bytes()` never finishes.
    MidBody,
}

/// A server that reads a request and then goes quiet for `delay`.
///
/// Hand-rolled over a blocking `TcpListener` on its own thread, like every
/// other mock server in this file. A mock-server crate would be a new
/// dependency for a behaviour that is one `thread::sleep` inside the
/// pattern already here — and what these tests need is a server that does
/// *not* obey HTTP's usual rhythm, which is the case a library built
/// around stubbing well-formed exchanges is least suited to.
///
/// The sleep is on a std thread, not a tokio task, so it blocks nothing
/// the client under test is running on.
pub(crate) fn start_stalling_server(stall: Stall, delay: Duration) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
    let addr = listener.local_addr().expect("the listener has an address");

    std::thread::spawn(move || {
        let Ok(stream) = listener.accept().map(|(s, _)| s) else {
            return;
        };
        let mut writer = stream.try_clone().expect("the socket clones");
        let mut reader = BufReader::new(stream);

        if drain_request_headers(&mut reader).is_none() {
            return;
        }

        if matches!(stall, Stall::MidBody) {
            // A body is promised and never sent, so the client is left
            // waiting inside the body read rather than inside the send.
            let _ = writer.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 11\r\n\r\n",
            );
            let _ = writer.flush();
        }

        // Long enough that a test seeing a timeout has necessarily seen
        // the client's clock rather than the server's.
        std::thread::sleep(delay);
        let _ = writer.write_all(b"too late");
    });

    addr
}

/// A server that answers a fixed table of `path -> raw HTTP response`,
/// over as many requests on one connection as the client cares to send —
/// which is what following a redirect chain to the same host looks like
/// on the wire. Unmatched paths 404, so a route the test forgot to wire up
/// fails loudly instead of hanging.
pub(crate) fn start_route_server(routes: Vec<(&'static str, Vec<u8>)>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
    let addr = listener.local_addr().expect("the listener has an address");

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let mut writer = stream.try_clone().expect("the socket clones");
            let mut reader = BufReader::new(stream);

            loop {
                let Some(request_line) = drain_request_headers(&mut reader) else {
                    return;
                };
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();

                let response = routes
                    .iter()
                    .find(|(route, _)| *route == path)
                    .map(|(_, body)| body.clone())
                    .unwrap_or_else(|| {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()
                    });

                if writer.write_all(&response).is_err() {
                    return;
                }
                let _ = writer.flush();
            }
        }
    });

    addr
}

/// A raw `301 Moved Permanently` pointing at `location`, keep-alive so the
/// client's next request in the chain arrives on the same connection.
pub(crate) fn redirect_response(status: u16, reason: &str, location: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status} {reason}\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n")
        .into_bytes()
}

/// A redirect exactly like [`redirect_response`], but carrying a
/// `Set-Cookie` on the hop itself — the cookie jar's test fixture for
/// whether a cookie set on an *intermediate* hop of a chain is picked up,
/// not just one set on the final response.
pub(crate) fn redirect_with_cookie_response(
    status: u16,
    reason: &str,
    location: &str,
    cookie: &str,
) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} {reason}\r\nLocation: {location}\r\nSet-Cookie: {cookie}\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

pub(crate) fn ok_response(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A raw `200`, like [`ok_response`], that also sets `cookie` via
/// `Set-Cookie` — the cookie jar's test fixture for a plain (non-redirect)
/// response that a login-style request would receive.
pub(crate) fn set_cookie_response(cookie: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nSet-Cookie: {cookie}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A raw `200` whose body is exactly `body`, byte for byte.
///
/// Separate from [`ok_response`] because that one takes a `&str` and so
/// cannot express a body that is not text — which is the entire subject
/// of the two non-UTF-8-body tests in `http`.
pub(crate) fn ok_bytes(content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

/// A server that terminates a TLS handshake with a self-signed certificate
/// for `127.0.0.1`, then answers one request with a fixed `200` —
/// `--insecure`'s test fixture: a client verifying certificates against a
/// real CA has nothing to trust here, exactly the case `--insecure` exists
/// for.
///
/// Sync `rustls`, not `tokio-rustls`: this file's servers all run on a plain
/// blocking thread, and `rustls::Stream` wraps a blocking `Read + Write` the
/// same way every plain-HTTP server above wraps a raw `TcpStream`, so the
/// handshake and the request/response exchange after it go through ordinary
/// `Read`/`Write` calls rather than a second async runtime just for this one
/// server.
pub(crate) fn start_self_signed_tls_server() -> SocketAddr {
    // rustls requires exactly one process-wide default `CryptoProvider`.
    // reqwest's own client — built in this same test binary, via the
    // `rustls-tls` feature — installs one too; whichever gets there first
    // wins, and an `Err` here only means it already has. Both resolve to the
    // `ring` backend either way — see the note on this dev-dependency in the
    // workspace `Cargo.toml`.
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

        // The same "request line, then headers to the blank line" read this
        // file's plain-HTTP servers use, inlined rather than shared through
        // `drain_request_headers`: that helper is typed to a bare
        // `BufReader<TcpStream>`, and generalising it over `Read` for the
        // sake of one more caller would be a bigger change than this test
        // fixture needs.
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

/// A server that terminates a TLS handshake demanding a client certificate
/// signed by a locally generated CA, then answers one request with a fixed
/// `200` — mutual TLS's test fixture. A connection presenting no client
/// certificate, or one not signed by that CA, fails during the handshake and
/// never reaches the request/response exchange below; a client presenting
/// the returned certificate/key pair (fed to
/// `reqwest::ClientBuilder::identity`, via
/// [`crate::build_client`]'s `client_cert`/`client_key`) authenticates and
/// gets the `200`.
///
/// The server's own certificate is self-signed for `127.0.0.1`, exactly like
/// [`start_self_signed_tls_server`] — a client under test still needs
/// `--insecure`/`insecure: true` to accept *that*, which is deliberate: it
/// keeps this fixture testing exactly one thing (does the server demand and
/// verify a client certificate) rather than two, and lets an
/// `insecure` + client-certificate combination be exercised against the one
/// server both features already have a fixture for.
///
/// Returns the server's address and the client certificate/key it will
/// accept, as PEM strings — written to files by the caller (this function
/// does not know whether the test wants them under a config directory or a
/// bare temp directory) rather than as paths itself.
pub(crate) fn start_mutual_tls_server() -> (SocketAddr, String, String) {
    // See the identical line in `start_self_signed_tls_server`.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // The server's own identity — self-signed, unrelated to the CA below,
    // because this fixture is about the *client* certificate the server
    // demands, not about the server's own.
    let server_certified = rcgen::generate_simple_self_signed(["127.0.0.1".to_string()])
        .expect("a self-signed certificate for 127.0.0.1 generates");
    let server_cert_der = server_certified.cert.der().clone();
    let server_key_der =
        rustls::pki_types::PrivateKeyDer::Pkcs8(server_certified.key_pair.serialize_der().into());

    // A CA that exists only to sign the one client certificate this server
    // will accept — not presented to the client itself, so it never needs a
    // subject alt name of its own.
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

    // The one client certificate this server will accept, signed by the CA
    // above rather than self-signed — the whole point of the fixture.
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
        // A connection with no client certificate, or one the verifier above
        // does not trust, fails right here — `ServerConnection::new` succeeds
        // (it just builds local state), but driving the handshake through
        // the first read below is where rustls actually rejects it, so the
        // read returns `Err`/`0` and this thread quietly stops, exactly as it
        // does for a plain connection drop.
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

/// A server like [`start_route_server`] — the same fixed `path -> raw HTTP
/// response` table, served over as many requests on one connection as the
/// client sends — that additionally records the `Cookie` header of every
/// request it receives, in the order they arrive (`None` when a request
/// carried no `Cookie` header at all). The cookie jar's test fixture: a
/// client jar sending cookies back is observed here as a header value
/// actually seen on the wire, not inferred from the response it got.
pub(crate) fn start_cookie_server(
    routes: Vec<(&'static str, Vec<u8>)>,
) -> (SocketAddr, Arc<std::sync::Mutex<Vec<Option<String>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
    let addr = listener.local_addr().expect("the listener has an address");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));

    let stored = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let mut writer = stream.try_clone().expect("the socket clones");
            let mut reader = BufReader::new(stream);

            loop {
                let mut request_line = String::new();
                match reader.read_line(&mut request_line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();

                let mut cookie = None;
                loop {
                    let mut header = String::new();
                    match reader.read_line(&mut header) {
                        Ok(0) | Err(_) => return,
                        Ok(_) if header == "\r\n" => break,
                        Ok(_) => {
                            if let Some((name, value)) = header.split_once(':') {
                                if name.trim().eq_ignore_ascii_case("cookie") {
                                    cookie = Some(value.trim().to_string());
                                }
                            }
                        }
                    }
                }
                stored.lock().unwrap().push(cookie);

                let response = routes
                    .iter()
                    .find(|(route, _)| *route == path)
                    .map(|(_, body)| body.clone())
                    .unwrap_or_else(|| {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()
                    });

                if writer.write_all(&response).is_err() {
                    return;
                }
                let _ = writer.flush();
            }
        }
    });

    (addr, seen)
}

/// A server that records the request line of the one connection it accepts,
/// then answers `200` — a stand-in for an HTTP proxy.
///
/// It does not actually forward anything anywhere: a client configured to
/// proxy through this address sends its request *to* this server with the
/// target's full URL on the request line (`GET http://target/path HTTP/1.1`,
/// "absolute-form", per RFC 7230 §5.3.2) rather than the plain path a direct
/// request would use (`GET /path HTTP/1.1`, "origin-form"). Recording that
/// line and asserting on its shape is what proves a request actually went
/// *through* this address as a proxy, rather than merely reaching some
/// server that happened to answer 200 — the distinction
/// [`build_client`](crate::build_client)'s `--proxy` tests are for.
pub(crate) fn start_proxy_recording_server() -> (SocketAddr, Arc<std::sync::Mutex<Option<String>>>)
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
    let addr = listener.local_addr().expect("the listener has an address");
    let seen = Arc::new(std::sync::Mutex::new(None));

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
