//! `sendra init`: scaffold the `.sendra/` directory a new project needs.
//!
//! `.sendra/config.yaml` and `.sendra/environments/default.yaml` are the two
//! files [`sendra_core::Config`] and [`sendra_core::Environment`] already know
//! how to read; nothing before this command wrote them, so starting a new
//! project meant copying an example by hand.
//!
//! **If `.sendra/` already exists, this refuses entirely rather than filling
//! in whichever of the two files is missing.** A `.sendra/` directory that
//! exists already almost always means a project someone is already working
//! in — a config with real values, an environment with real secrets — and the
//! one thing worse than a scaffold command that does nothing is one that
//! reaches into a directory that already has content and adds files to it
//! without being asked. The finer-grained "only create what's missing" option
//! was considered and set aside: it would still write into a directory a
//! person did not tell this command to touch, for a case — half the scaffold
//! present, half missing — rare enough that a clear refusal and a one-line
//! fix (delete or rename the file in the way) costs less than the surprise of
//! a command that sometimes writes one file and sometimes two.

use std::path::{Path, PathBuf};

use crate::exit::Exit;
use crate::output::print_error_line;

const PROJECT_DIR_NAME: &str = ".sendra";
const CONFIG_FILE_NAME: &str = "config.yaml";
const ENVIRONMENTS_DIR_NAME: &str = "environments";
const DEFAULT_ENVIRONMENT_FILE_NAME: &str = "default.yaml";

/// Every field [`sendra_core::config::ConfigFile`] knows, commented out, so
/// the file parses as an empty config (a fully-commented file is YAML null,
/// which core already treats as "nothing set yet") while still showing the
/// shape each key takes.
const CONFIG_TEMPLATE: &str = "\
# Sendra project configuration.
#
# Every field is optional; a project config overrides a global one key by
# key, not file by file. See the README's \"Precedence, start to finish\"
# section for where this sits among CLI overrides, the request file, and an
# active environment.

# Headers merged into every request. A header the request itself sets wins.
# headers:
#   User-Agent: my-app
#   Accept: application/json

# Whole-request timeout, in seconds: connect, send and body read.
# timeout_seconds: 30

# Whether to follow redirects, and how many hops to allow before giving up.
# `true` follows up to 10 (the default); `false` reports a 3xx response as-is
# instead of chasing it; a number sets a custom maximum.
# follow_redirects: true
";

/// A variable or two, commented out, showing both reference syntaxes a
/// request file can use: `{{name}}` for a value this file defines, and
/// `${VAR}` inside that value for one read from the OS environment at use
/// time.
const ENVIRONMENT_TEMPLATE: &str = "\
# Sendra environment: variables `{{name}}` in a request file substitutes
# from.
#
# `${VAR}` inside a value is read from the OS environment when it is used, so
# a file naming a secret can be committed without the secret itself ever
# being in it.

# base_url: https://api.example.com
# api_key: ${API_KEY}
";

/// Run `sendra init` against the current directory.
pub(crate) fn init() -> Exit {
    let cwd = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(err) => {
            print_error_line(format!("could not read the current directory: {err}"));
            return Exit::Failure;
        }
    };

    match init_in(&cwd) {
        Ok(created) => {
            println!("Created:");
            for path in created {
                println!("  {}", path.display());
            }
            Exit::Ok
        }
        Err(InitError::AlreadyExists(existing)) => {
            print_error_line(format!(
                "`{}` already exists — refusing to overwrite it",
                existing.display()
            ));
            Exit::Failure
        }
        Err(InitError::Io(err)) => {
            print_error_line(format!("could not scaffold `.sendra/`: {err}"));
            Exit::Failure
        }
    }
}

#[derive(Debug)]
enum InitError {
    /// `.sendra/` (or something at that path) is already there.
    AlreadyExists(PathBuf),
    Io(std::io::Error),
}

impl From<std::io::Error> for InitError {
    fn from(err: std::io::Error) -> Self {
        InitError::Io(err)
    }
}

/// The scaffold itself, against an arbitrary root rather than the real
/// current directory, so it is testable against a temporary tree.
///
/// Returns the paths written, relative to `root`, in the order they were
/// created — config first, then the environment file, matching the order
/// they are printed in.
fn init_in(root: &Path) -> Result<Vec<PathBuf>, InitError> {
    let project_dir = root.join(PROJECT_DIR_NAME);
    if project_dir.exists() {
        return Err(InitError::AlreadyExists(PathBuf::from(PROJECT_DIR_NAME)));
    }

    let environments_dir = project_dir.join(ENVIRONMENTS_DIR_NAME);
    std::fs::create_dir_all(&environments_dir)?;

    let config_path = project_dir.join(CONFIG_FILE_NAME);
    std::fs::write(&config_path, CONFIG_TEMPLATE)?;

    let environment_path = environments_dir.join(DEFAULT_ENVIRONMENT_FILE_NAME);
    std::fs::write(&environment_path, ENVIRONMENT_TEMPLATE)?;

    Ok(vec![
        PathBuf::from(PROJECT_DIR_NAME).join(CONFIG_FILE_NAME),
        PathBuf::from(PROJECT_DIR_NAME)
            .join(ENVIRONMENTS_DIR_NAME)
            .join(DEFAULT_ENVIRONMENT_FILE_NAME),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    use sendra_core::config::ConfigFile;
    use sendra_core::Environment;

    #[test]
    fn init_creates_both_files_in_an_empty_directory() {
        let temp = tempfile::tempdir().unwrap();

        let created = init_in(temp.path()).expect("an empty directory has nothing in the way");

        assert_eq!(
            created,
            vec![
                PathBuf::from(".sendra/config.yaml"),
                PathBuf::from(".sendra/environments/default.yaml"),
            ]
        );
        assert!(temp.path().join(".sendra/config.yaml").is_file());
        assert!(temp
            .path()
            .join(".sendra/environments/default.yaml")
            .is_file());
    }

    #[test]
    fn the_generated_config_parses_as_an_empty_config() {
        // Every field commented out is YAML null, which `ConfigFile` already
        // treats as "nothing set yet" rather than an error.
        let config =
            ConfigFile::from_yaml_str(CONFIG_TEMPLATE).expect("the template must be valid YAML");
        assert_eq!(config, ConfigFile::default());
    }

    #[test]
    fn the_generated_environment_parses_as_an_empty_environment() {
        let environment = Environment::from_yaml_str(ENVIRONMENT_TEMPLATE)
            .expect("the template must be valid YAML");
        assert!(environment.is_empty());
    }

    #[test]
    fn uncommenting_the_config_template_produces_the_values_it_shows() {
        let uncommented = "\
headers:
  User-Agent: my-app
  Accept: application/json
timeout_seconds: 30
follow_redirects: true
";
        let config =
            ConfigFile::from_yaml_str(uncommented).expect("the shown shape must actually parse");
        assert_eq!(
            config.headers.get("User-Agent").map(String::as_str),
            Some("my-app")
        );
        assert_eq!(config.timeout_seconds, Some(30));
    }

    #[test]
    fn uncommenting_the_environment_template_produces_the_values_it_shows() {
        let uncommented = "\
base_url: https://api.example.com
api_key: ${API_KEY}
";
        let environment =
            Environment::from_yaml_str(uncommented).expect("the shown shape must actually parse");
        assert_eq!(
            environment.variables.get("base_url").map(String::as_str),
            Some("https://api.example.com")
        );
        assert_eq!(
            environment.variables.get("api_key").map(String::as_str),
            Some("${API_KEY}")
        );
    }

    #[test]
    fn a_second_init_refuses_rather_than_overwriting() {
        let temp = tempfile::tempdir().unwrap();
        init_in(temp.path()).expect("the first call succeeds");

        // Modify the config so an overwrite, if it happened, would be
        // detectable.
        let config_path = temp.path().join(".sendra/config.yaml");
        std::fs::write(&config_path, "timeout_seconds: 99\n").unwrap();

        let err = init_in(temp.path()).expect_err("`.sendra/` already exists");
        assert!(matches!(err, InitError::AlreadyExists(_)));

        // Untouched by the refused second call.
        let contents = std::fs::read_to_string(&config_path).unwrap();
        assert_eq!(contents, "timeout_seconds: 99\n");
    }

    #[test]
    fn init_refuses_when_sendra_dir_exists_even_with_only_one_file_present() {
        // The coarser-grained rule stated on the module doc comment: even a
        // partially-populated `.sendra/` (here, a directory with neither file
        // in it yet) is refused rather than being filled in.
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".sendra")).unwrap();

        let err = init_in(temp.path()).expect_err("`.sendra/` exists, even though it is empty");
        assert!(matches!(err, InitError::AlreadyExists(_)));
        assert!(
            !temp.path().join(".sendra/config.yaml").exists(),
            "nothing should have been written"
        );
    }
}
