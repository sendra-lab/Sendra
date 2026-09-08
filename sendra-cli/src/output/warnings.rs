//! `--insecure`: the one-line warning printed whenever TLS certificate
//! verification resolves to off for a run, from either `insecure: true` in a
//! config file or `--insecure` on the command line.
//!
//! Printed to stderr, once, before any request-level output — the same
//! stream [`print_provenance`](super::print_provenance) and every error
//! already go to. **Unlike `-v`'s provenance banner, this is not gated
//! behind a flag of its own and is not suppressed by `-q`/`--quiet`.**
//! `-q` trims *narration* — the `→ <label>` lines, the response body — down
//! to the pass/fail answer; whether certificate verification is off for
//! this run is not narration, it is a fact about what is about to happen on
//! the wire, and the two flags answer different questions
//! (`--insecure`/`insecure: true` decides it, `-q` never reads it). It is
//! unaffected by `--json` for the same reason `-v`'s banner is: that flag's
//! stdout contract is about the *result* a run produced, not about how the
//! pipeline resolved to sending it insecurely.

use owo_colors::{OwoColorize, Stream};

/// Print the warning — call once per run, only when `insecure` resolved to
/// `true`, before the sending loop starts.
pub(crate) fn print_insecure_warning() {
    eprintln!("{}", render_insecure_warning());
}

/// The formatting itself, pure and separate from [`print_insecure_warning`]
/// for the same reason `provenance.rs`'s `render_provenance` is separate
/// from `print_provenance`: a test harness has no stderr to capture, so this
/// is the only part a test can actually see.
fn render_insecure_warning() -> String {
    format!(
        "{} TLS certificate verification is disabled for this run (--insecure / insecure: true) \
         — responses are not checked against a trusted certificate authority.",
        "⚠".if_supports_color(Stream::Stderr, |t| t.yellow())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_warning_names_both_the_flag_and_the_config_key() {
        let text = render_insecure_warning();
        assert!(text.contains("--insecure"), "{text}");
        assert!(text.contains("insecure: true"), "{text}");
        assert!(
            text.to_lowercase().contains("certificate"),
            "the warning should say what is actually disabled: {text}"
        );
    }
}
