//! [`MultipartPart`] and the hand-rolled `multipart/form-data` encoder
//! [`crate::Request::resolve_body`] uses for [`crate::Request::multipart`].

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::SendraError;

/// One part of a [`crate::Request::multipart`] body: either inline text (`value`) or
/// a file (`path`), never both and never neither — enforced by
/// [`crate::Request::validate`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultipartPart {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// Read a `body_file` (or a multipart file part) relative to `base_dir`, as
/// UTF-8 text.
///
/// A non-UTF-8 file surfaces as [`SendraError::BodyFileIo`] wrapping an
/// `InvalidData` error, matching what `std::fs::read_to_string` itself
/// returns for the same failure, rather than a silent lossy conversion —
/// unlike a *response* body, which Sendra has never promised to send
/// unmodified.
pub(crate) fn read_body_file(base_dir: &Path, path: &str) -> Result<String, SendraError> {
    let full_path = base_dir.join(path);
    std::fs::read_to_string(&full_path).map_err(|source| SendraError::BodyFileIo {
        path: full_path,
        source,
    })
}

/// Encode a `multipart` body by hand, as `multipart/form-data` text, and
/// return it along with the `Content-Type` (boundary included) it implies.
///
/// Not built with `reqwest::multipart::Form`: that type holds arbitrary
/// bytes and cannot be cloned, `PartialEq`d or serialized, none of which
/// [`crate::Request`] can give up — it is `Clone`, `PartialEq`, `Serialize` and
/// `Deserialize` throughout, including in the config/substitution/scripting
/// pipeline a multipart request passes through like any other. Writing the
/// format directly keeps the whole body a `String`, consistent with
/// [`resolve_body`](crate::Request::resolve_body)'s UTF-8-text rule for
/// `body_file`.
///
/// The boundary is derived from the current time, which is unique enough
/// per-request for a boundary's actual job: a delimiter unlikely to occur
/// inside any part's own content, not a cryptographic guarantee.
pub(crate) fn encode_multipart(
    parts: &[MultipartPart],
    base_dir: &Path,
) -> Result<(String, String), SendraError> {
    let boundary = format!(
        "----sendra-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );

    let mut body = String::new();
    for part in parts {
        body.push_str("--");
        body.push_str(&boundary);
        body.push_str("\r\n");
        match (&part.value, &part.path) {
            (Some(value), None) => {
                body.push_str(&format!(
                    "Content-Disposition: form-data; name=\"{}\"\r\n\r\n",
                    part.name
                ));
                body.push_str(value);
            }
            (None, Some(path)) => {
                let filename = Path::new(path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(path);
                body.push_str(&format!(
                    "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n\r\n",
                    part.name, filename
                ));
                body.push_str(&read_body_file(base_dir, path)?);
            }
            // Ruled out by `Request::validate` before `resolve_body` is ever
            // reached; kept exhaustive rather than `unreachable!()` so a
            // future caller of `encode_multipart` that skips validation gets
            // an empty part instead of a panic.
            (Some(_), Some(_)) | (None, None) => {}
        }
        body.push_str("\r\n");
    }
    body.push_str("--");
    body.push_str(&boundary);
    body.push_str("--\r\n");

    let content_type = format!("multipart/form-data; boundary={boundary}");
    Ok((body, content_type))
}
