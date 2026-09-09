//! `sendra schema`: materialize the JSON Schema files for editor tooling.
//!
//! `schema/*.schema.json` (request/collection/config/environment — see the
//! README's "JSON Schema / editor support" section) are generated from
//! `sendra-core`'s types by `xtask` and committed to the source repository.
//! That is enough for someone who has cloned the repo, but the actual
//! audience for editor tooling is anyone who has *installed* `sendra` —
//! most of whom never touch a checkout. `include_str!` bakes the four files
//! into this binary at compile time, so they are always available from the
//! one thing an installed-binary user actually has: the binary itself. No
//! network access, no docs site, no repository required.
//!
//! Because it is `include_str!` of the exact same committed files `xtask`
//! writes — not a hand-copied duplicate — there is no separate "embedded
//! copy" that could drift from `schema/*.schema.json`: it is the same bytes,
//! read once at compile time instead of at generation time. The regression
//! test at the bottom of this file guards the one way that could stop being
//! true (someone later replacing an `include_str!` with a literal or a
//! different path), by comparing the compiled-in bytes against the file on
//! disk independently, at test time.

use std::path::{Path, PathBuf};

use crate::exit::Exit;
use crate::output::print_error_line;

const SCHEMA_DIR_NAME: &str = "schema";

struct SchemaFile {
    name: &'static str,
    content: &'static str,
}

const SCHEMA_FILES: &[SchemaFile] = &[
    SchemaFile {
        name: "request.schema.json",
        content: include_str!("../../schema/request.schema.json"),
    },
    SchemaFile {
        name: "collection.schema.json",
        content: include_str!("../../schema/collection.schema.json"),
    },
    SchemaFile {
        name: "config.schema.json",
        content: include_str!("../../schema/config.schema.json"),
    },
    SchemaFile {
        name: "environment.schema.json",
        content: include_str!("../../schema/environment.schema.json"),
    },
];

/// Run `sendra schema`, writing into `output` (the current directory if not
/// given).
pub(crate) fn schema(output: Option<PathBuf>) -> Exit {
    let root = match output {
        Some(dir) => dir,
        None => match std::env::current_dir() {
            Ok(dir) => dir,
            Err(err) => {
                print_error_line(format!("could not read the current directory: {err}"));
                return Exit::Failure;
            }
        },
    };

    match schema_in(&root) {
        Ok(written) => {
            println!("Wrote:");
            for path in written {
                println!("  {}", path.display());
            }
            Exit::Ok
        }
        Err(err) => {
            print_error_line(format!("could not write schema files: {err}"));
            Exit::Failure
        }
    }
}

/// The write itself, against an arbitrary root rather than the real current
/// directory, so it is testable against a temporary tree.
///
/// Unlike `sendra init`, this **overwrites** rather than refusing when the
/// target already exists: these are generated, derived files with nothing of
/// the author's in them, so there is nothing to protect by refusing — only a
/// reason to make `sendra schema` after a `sendra` upgrade a normal,
/// unremarkable way to pick up a newer schema. `schema/` mirrors the
/// repository's own layout so the same `yaml.schemas` paths in the README
/// work whether the four files came from a checkout or from this command.
fn schema_in(root: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let schema_dir = root.join(SCHEMA_DIR_NAME);
    std::fs::create_dir_all(&schema_dir)?;

    let mut written = Vec::with_capacity(SCHEMA_FILES.len());
    for file in SCHEMA_FILES {
        std::fs::write(schema_dir.join(file.name), file.content)?;
        written.push(PathBuf::from(SCHEMA_DIR_NAME).join(file.name));
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_all_four_schema_files_into_an_empty_directory() {
        let temp = tempfile::tempdir().unwrap();

        let written = schema_in(temp.path()).expect("an empty directory has nothing in the way");

        assert_eq!(
            written,
            vec![
                PathBuf::from("schema/request.schema.json"),
                PathBuf::from("schema/collection.schema.json"),
                PathBuf::from("schema/config.schema.json"),
                PathBuf::from("schema/environment.schema.json"),
            ]
        );
        for path in &written {
            assert!(temp.path().join(path).is_file());
        }
    }

    #[test]
    fn every_embedded_file_is_valid_json() {
        for file in SCHEMA_FILES {
            serde_json::from_str::<serde_json::Value>(file.content)
                .unwrap_or_else(|err| panic!("{} is not valid JSON: {err}", file.name));
        }
    }

    /// Guards the thing that would otherwise let the embedded copy drift
    /// silently from `schema/*.schema.json`: this reads each file from disk
    /// independently, at test time, rather than through the same
    /// `include_str!` the embedded constant already went through — so a
    /// future change that hardcodes a literal, or points `include_str!` at
    /// the wrong path, shows up here instead of shipping unnoticed.
    #[test]
    fn every_embedded_schema_matches_the_committed_file_on_disk() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        for file in SCHEMA_FILES {
            let path = repo_root.join("schema").join(file.name);
            let on_disk = std::fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
            assert_eq!(
                file.content,
                on_disk,
                "the binary's embedded copy of {} has drifted from {}",
                file.name,
                path.display()
            );
        }
    }

    #[test]
    fn running_it_twice_overwrites_rather_than_refusing() {
        // The deliberate difference from `sendra init`: nothing here is
        // author-owned, so a second run (after a `sendra` upgrade, say)
        // should just pick up whatever is currently embedded.
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("schema/request.schema.json");

        schema_in(temp.path()).unwrap();
        std::fs::write(&target, "not a schema").unwrap();

        schema_in(temp.path()).expect("a second run must not refuse");

        assert_ne!(std::fs::read_to_string(&target).unwrap(), "not a schema");
    }

    #[test]
    fn writes_under_a_given_output_directory_not_only_the_current_one() {
        let temp = tempfile::tempdir().unwrap();
        let target_dir = temp.path().join("some/nested/project");
        std::fs::create_dir_all(&target_dir).unwrap();

        let written = schema_in(&target_dir).unwrap();

        for path in written {
            assert!(target_dir.join(path).is_file());
        }
    }
}
