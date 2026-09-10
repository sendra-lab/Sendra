//! Proves `sendra-tui`'s clean-exit audit (issue 13) holds for stdout being
//! piped/redirected: it must fail fast with one clear, human-readable line
//! on stderr — `main::require_interactive_stdout`, checked *before*
//! `init_terminal` ever calls `enable_raw_mode`/`EnterAlternateScreen` — not
//! hang waiting for terminal input, not crash with a raw crossterm/OS error,
//! and not silently write ANSI escape sequences into the pipe.
//!
//! Spawns the real compiled binary (the same `env!("CARGO_BIN_EXE_<name>")`
//! pattern `sendra-cli`'s own integration tests use — see
//! `sendra-cli/tests/cli_overrides.rs`) with its stdout piped back to this
//! test process, exactly what `sendra-tui > output.txt` or
//! `sendra-tui | cat` does to the child's stdout: it stops being a terminal.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn piped_stdout_fails_fast_with_a_clear_message_instead_of_a_raw_mode_crash() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sendra-tui"))
        // Piped, not inherited: this is what makes stdout stop being a
        // terminal from the child's point of view, the exact condition
        // `sendra-tui > output.txt` / `sendra-tui | cat` produce.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // No terminal input either, so a bug that *didn't* catch this case
        // and fell through to `enable_raw_mode`/blocking on a keypress can't
        // hang this test waiting for stdin — it would instead surface as
        // exactly the timeout this test asserts against below.
        .stdin(Stdio::null())
        .spawn()
        .expect("the compiled sendra-tui binary must launch");

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .expect("polling the child process must not itself fail")
        {
            break status;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "sendra-tui must exit immediately when stdout is piped, not hang — \
             it may have fallen through to raw-mode/terminal setup instead of \
             refusing up front"
        );
        std::thread::sleep(Duration::from_millis(20));
    };

    assert!(
        !status.success(),
        "piping stdout must be a refusal (non-zero exit), not a silent success"
    );

    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr was piped")
        .read_to_string(&mut stderr)
        .expect("reading the child's stderr must not fail");
    assert!(
        stderr.contains("interactive terminal"),
        "stderr must carry a clear, human-readable explanation, not a raw \
         crossterm/OS error with no context — got: {stderr:?}"
    );

    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("stdout was piped")
        .read_to_string(&mut stdout)
        .expect("reading the child's stdout must not fail");
    assert!(
        stdout.is_empty(),
        "refusing up front must mean nothing — no ANSI escape sequences, no \
         partial frame — ever reaches the piped stdout: got {stdout:?}"
    );
}
