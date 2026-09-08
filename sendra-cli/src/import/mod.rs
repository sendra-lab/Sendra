//! `sendra import`: turn another tool's command into a Sendra request file.
//!
//! Just [`curl`] today. The I/O and printing live here — reading the
//! command from an argument or stdin, writing the generated YAML to stdout
//! or a file, and reporting whatever the converter could not carry over —
//! so [`curl::convert`] itself stays pure and testable against plain
//! strings.

mod curl;

use std::io::Read;
use std::path::PathBuf;

use crate::exit::Exit;
use crate::output::print_error_line;

/// Run `sendra import curl`.
///
/// `command` is the positional argument, when one was given; `None` means
/// read the whole of stdin instead, for a pipe-based workflow like
/// `pbpaste | sendra import curl`. `output` is `-o`/`--output`; `None`
/// writes the generated YAML to stdout, which is what composes with shell
/// redirection (`sendra import curl '...' > request.yaml`).
pub(crate) fn import_curl(command: Option<String>, output: Option<PathBuf>) -> Exit {
    let command = match command {
        Some(command) => command,
        None => {
            let mut buf = String::new();
            if let Err(err) = std::io::stdin().read_to_string(&mut buf) {
                print_error_line(format!("could not read a curl command from stdin: {err}"));
                return Exit::Failure;
            }
            buf
        }
    };

    let conversion = match curl::convert(&command) {
        Ok(conversion) => conversion,
        Err(err) => {
            print_error_line(format!("could not convert this curl command: {err}"));
            return Exit::Failure;
        }
    };

    // Printed before the YAML itself is written, so a note or a warning
    // about what did not convert is not lost in a scroll past a long
    // request file. Both go to stderr — the generated YAML is the only
    // thing this command ever writes to stdout, so it still composes with
    // `> request.yaml` even when there is something to report.
    for note in &conversion.invocation_notes {
        eprintln!("note: {note}");
    }
    if !conversion.unsupported_flags.is_empty() {
        eprintln!(
            "note: {} flag{} not converted: {}",
            conversion.unsupported_flags.len(),
            if conversion.unsupported_flags.len() == 1 {
                ""
            } else {
                "s"
            },
            conversion.unsupported_flags.join(", ")
        );
    }

    let yaml = serde_yaml::to_string(&conversion.request).expect("a Request always serializes");

    match output {
        Some(path) => {
            if let Err(err) = std::fs::write(&path, &yaml) {
                print_error_line(format!("could not write `{}`: {err}", path.display()));
                return Exit::Failure;
            }
        }
        None => print!("{yaml}"),
    }

    Exit::Ok
}
