//! Core model for Sendra: request/response types, YAML loading, HTTP execution.
//!
//! This crate is deliberately free of CLI concerns (argument parsing, terminal
//! colouring, exit codes). A future `sendra-tui` crate will depend on it
//! directly, so everything here returns typed [`SendraError`] values that a
//! front-end can match on rather than pre-formatted strings.

pub mod assertions;
pub mod capture;
pub mod collection;
pub mod config;
pub mod environment;
mod error;
pub mod http;
pub mod request;
pub mod script;
#[cfg(test)]
mod test_support;

pub use assertions::{AssertionKind, AssertionReport, AssertionResult, Assertions, NotAssertions};
pub use capture::{CaptureFailure, CaptureReport, CaptureResult, CaptureSource, Captures};
pub use collection::{Collection, Document};
pub use config::Config;
pub use environment::Environment;
pub use error::SendraError;
pub use http::client::{build_client, HttpClient};
pub use http::response::{RedirectHop, Response};
pub use http::{send, send_prepared};
pub use request::auth::{ApiKeyAuth, ApiKeyLocation, Auth, BasicAuth};
pub use request::multipart::MultipartPart;
pub use request::{Method, Request, RetryConfig};
pub use script::{Hook, Script, ScriptOutcome, ScriptOutput, Scripts};
