//! End-to-end coverage for `-v`/`--verbose`.
//!
//! Exercised through the real binary, like `dry_run.rs`/`output_mode.rs`/
//! `quiet.rs`, since the point of this flag is the actual text that lands on
//! stderr and how it lines up with real files on disk — `find_project_config`
//! and `global_config_path` are already unit-tested in `sendra-core`, but
//! whether `-v` reports what they *actually* found for a given invocation is
//! only checkable by running the invocation.
//!
//! `XDG_CONFIG_HOME` is set explicitly on every `Command` here (never left to
//! the host environment) so "global config: none found" vs. "global config:
//! <path>" is deterministic regardless of what is actually installed on the
//! machine running these tests — see `global_config_path`'s own doc comment
//! in `sendra-core`: it is honoured first, on every platform, when absolute.

use std::path::Path;
use std::process::{Command, Output};

fn sendra(dir: &Path, xdg_config_home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sendra"))
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", xdg_config_home)
        .args(args)
        .output()
        .expect("the binary under test runs")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "the run should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_usage_error(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(2),
        "a rejected flag combination is a usage error: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A directory to point `XDG_CONFIG_HOME` at that holds no `sendra/`
/// subdirectory at all — "no global config" for every test that does not
/// itself set one up.
fn empty_xdg_config_home() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

fn write_plain_request(dir: &Path) {
    std::fs::write(
        dir.join("req.yaml"),
        "method: GET\nurl: https://example.com\n",
    )
    .unwrap();
}

/// Pull the value off a `"  <label>: <value>"` line in `-v`'s provenance
/// report.
///
/// Used only for the config lines (`project config:`/`global config:`),
/// whose value is the whole rest of the line. The environment line has its
/// own extractor below, since its value follows a name and an arrow rather
/// than starting right after the label.
fn provenance_field<'a>(stderr: &'a str, label: &str) -> &'a str {
    let prefix = format!("  {label}: ");
    stderr
        .lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .unwrap_or_else(|| panic!("no `{prefix}` line in stderr: {stderr}"))
}

/// Pull the resolved file path off `-v`'s `"  environment: <name> → <path>"`
/// line, given the `<name>` already known to the caller.
fn provenance_environment_path<'a>(stderr: &'a str, name: &str) -> &'a str {
    let prefix = format!("  environment: {name} → ");
    stderr
        .lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .unwrap_or_else(|| panic!("no `{prefix}` line in stderr: {stderr}"))
}

/// Assert that a path `-v` printed actually names `expected_suffix` inside
/// the directory the test created — without requiring the whole absolute
/// path to match byte-for-byte.
///
/// **Why not a full-path comparison**: the printed path comes from
/// `find_project_config`/`find_environment`, both of which walk up from
/// `std::env::current_dir()` inside the spawned `sendra` process — and on
/// macOS, `current_dir()` reports the working directory *with* `/var`
/// resolved to its real location, `/private/var`, because `/var` is a
/// symlink there. `tempfile::tempdir()`'s own `.path()`, by contrast, is
/// built from `$TMPDIR` and is never resolved through that symlink. The two
/// disagree only in that one path segment, only on macOS (Linux has no such
/// symlink in its temp path, and Windows has no such symlink at all) — a
/// platform quirk in *which absolute prefix* the OS reports for the same
/// real file, not a disagreement about *which file* was found. Comparing
/// suffixes sidesteps it instead of trying to predict the OS-specific
/// canonical form (which `std::fs::canonicalize` cannot safely stand in
/// for here either — on Windows it prepends the verbatim `\\?\` prefix,
/// which `current_dir()` itself never returns, so canonicalizing the
/// expected side would trade a macOS-only failure for a Windows-only one).
fn assert_provenance_path_ends_with(printed: &str, expected_suffix: &Path) {
    assert!(
        Path::new(printed).ends_with(expected_suffix),
        "expected a path ending in {expected_suffix:?}, got {printed:?}"
    );
}

// --- config provenance: project / global / both / neither ----------------

#[test]
fn verbose_reports_neither_config_file_as_found() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let xdg = empty_xdg_config_home();
    write_plain_request(dir.path());

    let output = sendra(dir.path(), xdg.path(), &["run", "req.yaml", "-v"]);
    assert_success(&output);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("project config: none found"), "{stderr}");
    assert!(stderr.contains("global config: none found"), "{stderr}");
}

#[test]
fn verbose_reports_a_project_config_alone() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let xdg = empty_xdg_config_home();
    write_plain_request(dir.path());
    std::fs::create_dir_all(dir.path().join(".sendra")).unwrap();
    std::fs::write(
        dir.path().join(".sendra/config.yaml"),
        "timeout_seconds: 5\n",
    )
    .unwrap();

    let output = sendra(dir.path(), xdg.path(), &["run", "req.yaml", "-v"]);
    assert_success(&output);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_provenance_path_ends_with(
        provenance_field(&stderr, "project config"),
        &Path::new(".sendra").join("config.yaml"),
    );
    assert!(stderr.contains("global config: none found"), "{stderr}");
}

#[test]
fn verbose_reports_a_global_config_alone() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let xdg = empty_xdg_config_home();
    write_plain_request(dir.path());
    let global_config_dir = xdg.path().join("sendra");
    std::fs::create_dir_all(&global_config_dir).unwrap();
    std::fs::write(
        global_config_dir.join("config.yaml"),
        "timeout_seconds: 5\n",
    )
    .unwrap();

    let output = sendra(dir.path(), xdg.path(), &["run", "req.yaml", "-v"]);
    assert_success(&output);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("project config: none found"), "{stderr}");
    let expected_path = global_config_dir.join("config.yaml");
    assert!(
        stderr.contains(&format!("global config: {}", expected_path.display())),
        "{stderr}"
    );
}

#[test]
fn verbose_reports_both_config_files_when_both_apply() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let xdg = empty_xdg_config_home();
    write_plain_request(dir.path());

    std::fs::create_dir_all(dir.path().join(".sendra")).unwrap();
    std::fs::write(
        dir.path().join(".sendra/config.yaml"),
        "timeout_seconds: 5\n",
    )
    .unwrap();

    let global_config_dir = xdg.path().join("sendra");
    std::fs::create_dir_all(&global_config_dir).unwrap();
    std::fs::write(
        global_config_dir.join("config.yaml"),
        "timeout_seconds: 10\n",
    )
    .unwrap();

    let output = sendra(dir.path(), xdg.path(), &["run", "req.yaml", "-v"]);
    assert_success(&output);

    let stderr = String::from_utf8_lossy(&output.stderr);
    // The project path is compared by suffix, not exact match — see
    // `assert_provenance_path_ends_with`'s doc comment. The global path is
    // compared exactly: it comes straight from the `XDG_CONFIG_HOME` value
    // this test passed in as a literal environment variable, never through
    // `current_dir()`, so it carries none of that ambiguity.
    assert_provenance_path_ends_with(
        provenance_field(&stderr, "project config"),
        &Path::new(".sendra").join("config.yaml"),
    );
    let global_path = global_config_dir.join("config.yaml");
    assert!(
        stderr.contains(&format!("global config: {}", global_path.display())),
        "{stderr}"
    );
}

// --- environment provenance: --env / default fallback / none -------------

#[test]
fn verbose_reports_the_named_environment_and_its_file() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let xdg = empty_xdg_config_home();
    write_plain_request(dir.path());
    std::fs::create_dir_all(dir.path().join(".sendra/environments")).unwrap();
    std::fs::write(
        dir.path().join(".sendra/environments/staging.yaml"),
        "host: staging.example.com\n",
    )
    .unwrap();

    let output = sendra(
        dir.path(),
        xdg.path(),
        &["run", "req.yaml", "-v", "--env", "staging"],
    );
    assert_success(&output);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_provenance_path_ends_with(
        provenance_environment_path(&stderr, "staging"),
        &Path::new(".sendra")
            .join("environments")
            .join("staging.yaml"),
    );
}

#[test]
fn verbose_reports_the_default_environment_fallback() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let xdg = empty_xdg_config_home();
    write_plain_request(dir.path());
    std::fs::create_dir_all(dir.path().join(".sendra/environments")).unwrap();
    std::fs::write(
        dir.path().join(".sendra/environments/default.yaml"),
        "host: example.com\n",
    )
    .unwrap();

    let output = sendra(dir.path(), xdg.path(), &["run", "req.yaml", "-v"]);
    assert_success(&output);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_provenance_path_ends_with(
        provenance_environment_path(&stderr, "default"),
        &Path::new(".sendra")
            .join("environments")
            .join("default.yaml"),
    );
}

#[test]
fn verbose_reports_no_environment_found() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let xdg = empty_xdg_config_home();
    write_plain_request(dir.path());

    let output = sendra(dir.path(), xdg.path(), &["run", "req.yaml", "-v"]);
    assert_success(&output);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("environment: default (none found)"),
        "{stderr}"
    );
}

// --- override provenance: names, not values -------------------------------

#[test]
fn verbose_reports_var_and_header_override_names_but_not_values() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let xdg = empty_xdg_config_home();
    write_plain_request(dir.path());

    let output = sendra(
        dir.path(),
        xdg.path(),
        &[
            "run",
            "req.yaml",
            "-v",
            "--var",
            "token=super-secret-value",
            "-H",
            "Authorization: Bearer another-secret",
        ],
    );
    assert_success(&output);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--var: 1 set (token)"), "{stderr}");
    assert!(stderr.contains("-H: 1 set (Authorization)"), "{stderr}");
    assert!(
        !stderr.contains("super-secret-value") && !stderr.contains("another-secret"),
        "override values must never appear: {stderr}"
    );
}

#[test]
fn verbose_reports_no_overrides_when_none_were_passed() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let xdg = empty_xdg_config_home();
    write_plain_request(dir.path());

    let output = sendra(dir.path(), xdg.path(), &["run", "req.yaml", "-v"]);
    assert_success(&output);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--var: none"), "{stderr}");
    assert!(stderr.contains("-H: none"), "{stderr}");
}

// --- `-v` + `-q` is refused ------------------------------------------------

#[test]
fn verbose_with_quiet_is_refused_on_both_subcommands() {
    for subcommand in ["run", "test"] {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let xdg = empty_xdg_config_home();
        write_plain_request(dir.path());

        let output = sendra(
            dir.path(),
            xdg.path(),
            &[subcommand, "req.yaml", "-v", "-q"],
        );
        assert_usage_error(&output);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--verbose") && stderr.contains("--quiet"),
            "{subcommand}: {stderr}"
        );
        assert!(
            output.stdout.is_empty(),
            "{subcommand}: a rejected run must not have started"
        );
    }
}

// --- `-v` + `--json`: stderr-only, document unaffected --------------------

#[test]
fn verbose_with_json_still_prints_to_stderr_and_leaves_the_document_unaffected() {
    // `--dry-run` alongside `--json` here, so this exercises no real network
    // call and cannot be flaky in a sandboxed test environment — -v's
    // interaction with --json does not depend on a request actually being
    // sent.
    let dir = tempfile::tempdir().expect("a temporary directory");
    let xdg = empty_xdg_config_home();
    write_plain_request(dir.path());

    let output = sendra(
        dir.path(),
        xdg.path(),
        &["run", "req.yaml", "-v", "--dry-run", "--json"],
    );
    assert_success(&output);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("resolved"), "{stderr}");
    assert!(stderr.contains("project config: none found"), "{stderr}");

    // stdout is still exactly one JSON document — --json's own contract,
    // unaffected by -v printing to stderr alongside it.
    let _document: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("--json output must still be one parseable document even with -v");
}

// --- `-v` + `--dry-run`: works, and does not change --dry-run's output ---

#[test]
fn verbose_with_dry_run_prints_provenance_and_leaves_the_resolved_request_unchanged() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let xdg = empty_xdg_config_home();
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: POST\nurl: https://example.com/users\njson:\n  name: ada\n",
    )
    .unwrap();

    let with_verbose = sendra(
        dir.path(),
        xdg.path(),
        &["run", "req.yaml", "--dry-run", "-v"],
    );
    assert_success(&with_verbose);

    let stderr = String::from_utf8_lossy(&with_verbose.stderr);
    assert!(stderr.contains("resolved"), "{stderr}");
    assert!(stderr.contains("project config: none found"), "{stderr}");

    let without_verbose = sendra(dir.path(), xdg.path(), &["run", "req.yaml", "--dry-run"]);
    assert_success(&without_verbose);

    // --dry-run's own stdout output is exactly the same either way — -v adds
    // stderr narration, and changes nothing about what --dry-run prints.
    assert_eq!(
        with_verbose.stdout, without_verbose.stdout,
        "-v must not change --dry-run's own output"
    );
}

// --- omitting `-v` prints no provenance ------------------------------------

#[test]
fn omitting_verbose_prints_no_provenance() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let xdg = empty_xdg_config_home();
    write_plain_request(dir.path());

    let output = sendra(dir.path(), xdg.path(), &["run", "req.yaml"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("resolved"),
        "no -v was passed, so there should be no provenance report: {stderr}"
    );
}
