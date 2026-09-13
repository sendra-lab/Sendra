//! Finds candidate collection files near the current directory for the
//! welcome screen's discovery picker (`view::welcome`) to offer instead of
//! the plain empty-state message — see `LoadState::NoPathProvided`'s own
//! doc comment for why this runs from inside `update()` rather than being
//! handed in from `main.rs`.
//!
//! This is a new, standalone concept, not a reuse of `find_environment`/
//! `find_project_config` (`sendra-core`) or `main::discover_environment_names`:
//! those all resolve one *named* file (an environment, a project config)
//! by walking upward through `Path::ancestors()` looking for a fixed
//! `.sendra/...` location. A collection file has no such reserved home — it
//! can be any `.yaml`/`.yml` file a user points `sendra-tui`/`sendra run` at
//! — so there is nothing to name and nowhere fixed to walk to; the only
//! meaningful sense of "discoverable" is "sitting right here already."

use std::path::{Path, PathBuf};

/// Every `.yaml`/`.yml` file sitting directly inside `start_dir` — a single,
/// non-recursive listing (not `start_dir`'s subdirectories, and not its
/// ancestors the way `find_environment`/`find_project_config` walk upward):
/// "discoverable from the current directory" means "already sitting here",
/// not "somewhere in a project root above it" the way an environment or
/// config file's fixed `.sendra/` location does. Sorted for a stable,
/// predictable picker order rather than whatever order the OS happens to
/// hand back from `read_dir`. Empty (never an error) when `start_dir` can't
/// be read at all — the caller's fallback is the plain welcome message, not
/// a propagated I/O error over something this minor.
pub(crate) fn discover_collections(start_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(start_dir) else {
        return Vec::new();
    };

    let mut candidates: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("yaml") | Some("yml")
            )
        })
        .collect();
    candidates.sort();
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_yaml_and_yml_files_directly_in_the_directory() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        std::fs::write(dir.path().join("one.yaml"), "").unwrap();
        std::fs::write(dir.path().join("two.yml"), "").unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();

        let found = discover_collections(dir.path());

        assert_eq!(
            found,
            vec![dir.path().join("one.yaml"), dir.path().join("two.yml")]
        );
    }

    #[test]
    fn does_not_descend_into_subdirectories() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("inner.yaml"), "").unwrap();

        assert!(discover_collections(dir.path()).is_empty());
    }

    #[test]
    fn ignores_a_dot_sendra_directory_the_same_as_any_other_subdirectory() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let sendra_dir = dir.path().join(".sendra").join("environments");
        std::fs::create_dir_all(&sendra_dir).unwrap();
        std::fs::write(sendra_dir.join("default.yaml"), "").unwrap();

        assert!(
            discover_collections(dir.path()).is_empty(),
            "an environment file nested under .sendra/ is not a collection candidate"
        );
    }

    #[test]
    fn an_empty_directory_yields_no_candidates() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        assert!(discover_collections(dir.path()).is_empty());
    }

    #[test]
    fn a_directory_that_does_not_exist_yields_no_candidates_rather_than_panicking() {
        let missing = Path::new("this/directory/does/not/exist/anywhere");
        assert!(discover_collections(missing).is_empty());
    }

    #[test]
    fn results_are_sorted() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        std::fs::write(dir.path().join("z.yaml"), "").unwrap();
        std::fs::write(dir.path().join("a.yaml"), "").unwrap();
        std::fs::write(dir.path().join("m.yml"), "").unwrap();

        let found = discover_collections(dir.path());

        assert_eq!(
            found,
            vec![
                dir.path().join("a.yaml"),
                dir.path().join("m.yml"),
                dir.path().join("z.yaml"),
            ]
        );
    }
}
