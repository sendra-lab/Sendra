//! [`Response`] and [`RedirectHop`]: what sending a [`crate::Request`] gets
//! back.

use std::time::Duration;

/// The result of sending a [`crate::Request`].
///
/// Headers are a `Vec` of pairs rather than a map: HTTP allows repeats
/// (`set-cookie`) and wire order is worth preserving for display.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    /// The response body, decoded from the bytes on the wire **lossily**:
    /// any byte sequence that is not valid UTF-8 is replaced with U+FFFD
    /// (`\u{fffd}`, the replacement character) rather than erroring.
    ///
    /// This is a deliberate contract, not an accident of the type. A body can
    /// legitimately be a PNG or a protobuf, and a tool whose job is to show
    /// you what came back should show you *something* rather than refuse the
    /// whole response over its encoding — the status, the headers and the
    /// elapsed time are all still true and all still worth seeing. So an
    /// invalid body is never an error.
    ///
    /// The cost is that it is **not round-trippable**: `body.as_bytes()` is
    /// not what the server sent, and the original bytes cannot be recovered
    /// from here. Everything downstream that reads this — assertions,
    /// captures, scripts, `--json` output — is reading the replaced text, so
    /// a `body_contains` against a binary payload is comparing against U+FFFD
    /// and will not match. Binary-safe bodies (keeping the raw bytes
    /// alongside, and telling the user when a substitution happened) are a
    /// later concern; today the substitution is silent.
    pub body: String,
    pub elapsed: Duration,
    /// Every redirect hop that led to this response, oldest first: empty when
    /// the request was answered directly, when [`FollowRedirects::Disabled`](crate::config::FollowRedirects::Disabled)
    /// left a 3xx response as this one, or when only one hop's worth of
    /// following happened and it landed here without an intermediate stop.
    ///
    /// Each entry is the status of the response that redirected, and the
    /// `Location` it pointed at (resolved to an absolute URL) — the same two
    /// facts a `curl -v` trace would show for that hop. This response's own
    /// status and headers are not repeated here.
    pub redirects: Vec<RedirectHop>,
}

impl Response {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// One hop of a redirect chain, recorded on the way to a [`Response`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedirectHop {
    /// The status of the response that redirected — `301`, `302`, and so on.
    pub status: u16,
    /// Where it pointed: the `Location` header, resolved against the URL that
    /// received it, as an absolute URL.
    pub location: String,
}
