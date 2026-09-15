//! Generates the editor-tooling JSON Schemas under `schema/` from the actual
//! Rust types in `sendra-core` (behind its `schema` feature — see the note on
//! that feature), and checks that the committed files still match.
//!
//! Also writes (and checks) an identical vendored copy under
//! `sendra-cli/schema/`: `sendra-cli/src/schema.rs` embeds these files with
//! `include_str!` at compile time, and a `cargo publish` tarball can only
//! ever contain files inside the crate's own directory, so `sendra-cli`
//! needs its own copy rather than reaching up to the workspace root. `schema/`
//! at the workspace root stays the canonical, checked-out-repo-facing
//! location (see `docs/reference/json-schema.md`) — the vendored copy is a
//! packaging-only duplicate kept identical by this file, not a second
//! source of truth.
//!
//! `cargo run -p xtask -- generate` writes both `schema/*.schema.json` and
//! `sendra-cli/schema/*.schema.json`.
//! `cargo run -p xtask -- check` regenerates in memory and fails (non-zero
//! exit, with a diff-shaped message) if either committed copy would change —
//! the anti-drift check CI runs so a schema can never silently go stale
//! against the types it claims to describe, or against its own vendored copy.

use std::path::{Path, PathBuf};

use schemars::{schema_for, Schema};
use sendra_core::collection::Collection;
use sendra_core::config::ConfigFile;
use sendra_core::environment::EnvironmentFile;
use sendra_core::request::Request;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives directly under the workspace root")
        .to_path_buf()
}

fn schema_dir() -> PathBuf {
    workspace_root().join("schema")
}

/// `sendra-cli`'s own vendored copy — see the module docs above for why this
/// duplicate exists.
fn vendored_schema_dir() -> PathBuf {
    workspace_root().join("sendra-cli").join("schema")
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
            schema: titled(schema_for!(EnvironmentFile), "Sendra environment"),
        },
    ]
}

/// `schema_for!` already titles a root schema with the type's Rust name
/// (`Request`, `Collection`, `ConfigFile`, `EnvironmentFile`); this overrides
/// it with the name a file's own author would recognise, since
/// "EnvironmentFile" is an internal implementation detail no
/// `.sendra/environments/*.yaml` author has ever seen.
fn titled(mut schema: Schema, title: &str) -> Schema {
    schema.insert("title".to_string(), title.into());
    schema
}

fn render(schema: &Schema) -> String {
    let mut text =
        serde_json::to_string_pretty(schema).expect("a generated JSON Schema always serializes");
    text.push('\n');
    text
}

fn generate() {
    for dir in [schema_dir(), vendored_schema_dir()] {
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|err| panic!("{} directory should be creatable: {err}", dir.display()));
        for target in targets() {
            let path = dir.join(target.file_name);
            std::fs::write(&path, render(&target.schema))
                .unwrap_or_else(|err| panic!("writing {}: {err}", path.display()));
            println!("wrote {}", path.display());
        }
    }
}

fn check() -> bool {
    let mut drifted = Vec::new();

    for dir in [schema_dir(), vendored_schema_dir()] {
        for target in targets() {
            let path = dir.join(target.file_name);
            let expected = render(&target.schema);
            let actual = std::fs::read_to_string(&path).unwrap_or_default();
            if actual != expected {
                drifted.push(path);
            }
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
