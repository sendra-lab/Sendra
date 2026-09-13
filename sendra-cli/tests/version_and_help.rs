//! Proves `--version` and `--help` work through the compiled `sendra`
//! binary, including for the `tui` subcommand — clap derives both from
//! `Cli`'s `#[command(version, about)]` and each `Command` variant's doc
//! comments (see `sendra-cli/src/cli.rs`), but nothing else in this crate's
//! test suite spawns the binary with either flag, so a regression that broke
//! one (a missing `version` attribute, a `Tui` variant losing its doc
//! comment) would otherwise go unnoticed until someone ran it by hand.

use std::process::Command;

fn run(args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_sendra"))
        .args(args)
        .output()
        .expect("the compiled sendra binary must launch");
    (
        output.status.success(),
        String::from_utf8(output.stdout).expect("stdout must be valid UTF-8"),
        String::from_utf8(output.stderr).expect("stderr must be valid UTF-8"),
    )
}

#[test]
fn version_prints_the_binary_name_and_cargo_version() {
    let (ok, stdout, _) = run(&["--version"]);
    assert!(ok, "`--version` must exit successfully");
    assert_eq!(
        stdout.trim(),
        format!("sendra {}", env!("CARGO_PKG_VERSION"))
    );

    // clap also offers the short form.
    let (ok, stdout_short, _) = run(&["-V"]);
    assert!(ok, "`-V` must exit successfully");
    assert_eq!(stdout_short, stdout, "`-V` and `--version` must agree");
}

#[test]
fn top_level_help_lists_every_subcommand() {
    let (ok, stdout, _) = run(&["--help"]);
    assert!(ok, "`--help` must exit successfully");
    for subcommand in ["run", "test", "init", "schema", "import", "tui"] {
        assert!(
            stdout.contains(subcommand),
            "top-level --help must list `{subcommand}`: {stdout}"
        );
    }
    assert!(
        stdout.contains("launch the interactive TUI") || stdout.contains("no subcommand"),
        "top-level --help must mention the bare-invocation TUI shortcut: {stdout}"
    );
}

#[test]
fn tui_help_documents_its_optional_path() {
    let (ok, stdout, _) = run(&["tui", "--help"]);
    assert!(ok, "`sendra tui --help` must exit successfully");
    assert!(
        stdout.contains("PATH"),
        "`sendra tui --help` must document the optional path argument: {stdout}"
    );
}
