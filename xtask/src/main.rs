//! Generates the editor-tooling JSON Schemas under `schema/` from the actual
//! Rust types in `sendra-core` (behind its `schema` feature — see the note on
//! that feature), and checks that the committed files still match.
//!
//! `cargo run -p xtask -- generate` writes `schema/*.schema.json`.
//! `cargo run -p xtask -- check` regenerates in memory and fails (non-zero
//! exit, with a diff-shaped message) if a committed file would change — the
//! anti-drift check CI runs so a schema can never silently go stale against
//! the types it claims to describe.
//!
//! There is one exception: `environment.schema.json`. Environment files parse
//! straight into a `BTreeMap<String, String>` — see
//! `sendra_core::environment` — so there is no dedicated Rust type for
//! `schemars` to derive from. That schema is a hand-written literal, checked
//! in exactly the same way as the other three so it cannot drift from *this
//! file* even though it cannot drift from a Rust type that does not exist.

use std::path::{Path, PathBuf};

use schemars::{schema_for, Schema};
use sendra_core::collection::Collection;
use sendra_core::config::ConfigFile;
use sendra_core::request::Request;

fn schema_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives directly under the workspace root")
        .join("schema")
}

/// One generated file: name under `schema/`, and the schema itself.
struct Target {
    file_name: &'static str,
    schema: Schema,
}

fn targets() -> Vec<Target> {
    vec![
        Target {
            file_name: "request.schema.json",
            schema: titled(schema_for!(Request), "Sendra request"),
        },
        Target {
            file_name: "collection.schema.json",
            schema: titled(schema_for!(Collection), "Sendra collection"),
        },
        Target {
            file_name: "config.schema.json",
            schema: titled(schema_for!(ConfigFile), "Sendra config"),
        },
        Target {
            file_name: "environment.schema.json",
            schema: environment_schema(),
        },
    ]
}

/// `schema_for!` already titles a root schema with the type's Rust name
/// (`Request`, `Collection`, `ConfigFile`); this overrides it with the name a
/// file's own author would recognise, since "ConfigFile" is an internal
/// implementation detail no `.sendra/config.yaml` author has ever seen.
fn titled(mut schema: Schema, title: &str) -> Schema {
    schema.insert("title".to_string(), title.into());
    schema
}

/// Hand-written, not `schemars`-generated: there is no `EnvironmentFile`
/// Rust type to derive from (see the module docs above). The shape is a
/// closed, one-line fact — every value is coerced to a string, matching
/// [`sendra_core::environment`]'s own parsing — so a hand-maintained schema
/// carries negligible drift risk despite not being derived.
fn environment_schema() -> Schema {
    schemars::json_schema!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Sendra environment",
        "description": "An environment file: a flat mapping of variable name to value, \
            substituted for `{{name}}` in a request. Every value is read as a string — \
            `port: 8080` defines the string \"8080\", not a number.",
        "type": "object",
        "additionalProperties": { "type": "string" }
    })
}

fn render(schema: &Schema) -> String {
    let mut text =
        serde_json::to_string_pretty(schema).expect("a generated JSON Schema always serializes");
    text.push('\n');
    text
}

fn generate() {
    let dir = schema_dir();
    std::fs::create_dir_all(&dir).expect("schema/ directory should be creatable");
    for target in targets() {
        let path = dir.join(target.file_name);
        std::fs::write(&path, render(&target.schema))
            .unwrap_or_else(|err| panic!("writing {}: {err}", path.display()));
        println!("wrote {}", path.display());
    }
}

fn check() -> bool {
    let dir = schema_dir();
    let mut drifted = Vec::new();

    for target in targets() {
        let path = dir.join(target.file_name);
        let expected = render(&target.schema);
        let actual = std::fs::read_to_string(&path).unwrap_or_default();
        if actual != expected {
            drifted.push(path);
        }
    }

    if drifted.is_empty() {
        println!("schema/*.schema.json matches the current Rust types.");
        true
    } else {
        eprintln!("schema drift detected — regenerate with `cargo run -p xtask -- generate`:");
        for path in &drifted {
            eprintln!("  {}", path.display());
        }
        false
    }
}

fn main() {
    let command = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "generate".to_string());
    match command.as_str() {
        "generate" => generate(),
        "check" => {
            if !check() {
                std::process::exit(1);
            }
        }
        other => {
            eprintln!("unknown command `{other}`; expected `generate` or `check`");
            std::process::exit(2);
        }
    }
}
