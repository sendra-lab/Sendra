//! `-v`/`--verbose`: a fixed, one-time report of which config, environment
//! and override sources actually applied to this run.
//!
//! Printed to stderr, once, before any request-level output — the same
//! stream the `→ <label>` lines and every error already go to, and for the
//! same reason: this is diagnostic narration about how the run's pipeline
//! resolved, not part of its result. It stays on stderr, unaffected, even
//! under `--json`: that flag's stdout contract is about the *result* a run
//! produced, not about how the pipeline got there — see `-v`'s own doc
//! comment in `cli.rs` for the full reasoning.

use std::path::Path;

use owo_colors::{OwoColorize, Stream};

/// Print what [`prepare`](crate::run) actually resolved: the project and
/// global config files (or that none was found for either), the
/// environment selected by name and its file (or that none was found), and
/// whether `--var`/`-H` overrides were passed.
///
/// `environment_name` is whatever `--env` named, or `"default"` when it was
/// omitted — the same fallback [`environment_for`](crate::run) applies —
/// since the resolved [`Environment`](sendra_core::Environment) itself
/// carries the file it came from but not the name it was resolved under.
pub(crate) fn print_provenance(
    project_config: Option<&Path>,
    global_config: Option<&Path>,
    environment_name: &str,
    environment_source: Option<&Path>,
    var_overrides: &[(String, String)],
    headers: &[(String, String)],
) {
    eprint!(
        "{}",
        render_provenance(
            project_config,
            global_config,
            environment_name,
            environment_source,
            var_overrides,
            headers,
        )
    );
}

/// The formatting itself, pure and separate from [`print_provenance`] for the
/// same reason `errors.rs`'s `render_hint` is separate from `print_hint`: a
/// test harness has no stderr to capture, so this is the only part a test
/// can actually see.
fn render_provenance(
    project_config: Option<&Path>,
    global_config: Option<&Path>,
    environment_name: &str,
    environment_source: Option<&Path>,
    var_overrides: &[(String, String)],
    headers: &[(String, String)],
) -> String {
    let mut out = format!(
        "{}\n",
        "resolved".if_supports_color(Stream::Stderr, |t| t.dimmed())
    );

    out.push_str(&source_line("project config", project_config));
    out.push_str(&source_line("global config", global_config));

    out.push_str(&match environment_source {
        Some(path) => format!("  environment: {environment_name} → {}\n", path.display()),
        None => format!("  environment: {environment_name} (none found)\n"),
    });

    out.push_str(&override_line("--var", var_overrides));
    out.push_str(&override_line("-H", headers));

    out
}

fn source_line(label: &str, path: Option<&Path>) -> String {
    match path {
        Some(path) => format!("  {label}: {}\n", path.display()),
        None => format!("  {label}: none found\n"),
    }
}

/// `names`, not values — see `-v`'s own doc comment in `cli.rs` for why: a
/// `--var` or `-H` override often carries a token or password meant for a
/// request field, not for a terminal, a shell history file or a captured CI
/// log. The name answers the provenance question `-v` exists to answer
/// ("did my override apply?"); the value does not need to be on screen to
/// answer it, and `--dry-run` already exists for the case that does need to
/// see it.
fn override_line(label: &str, overrides: &[(String, String)]) -> String {
    if overrides.is_empty() {
        return format!("  {label}: none\n");
    }

    let names: Vec<&str> = overrides.iter().map(|(name, _)| name.as_str()).collect();
    format!(
        "  {label}: {} set ({})\n",
        overrides.len(),
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neither_config_file_reports_none_found() {
        let text = render_provenance(None, None, "default", None, &[], &[]);
        assert_eq!(
            text,
            "resolved\n\
             \x20 project config: none found\n\
             \x20 global config: none found\n\
             \x20 environment: default (none found)\n\
             \x20 --var: none\n\
             \x20 -H: none\n"
        );
    }

    #[test]
    fn a_project_config_alone_names_only_the_project_path() {
        let project = Path::new(".sendra/config.yaml");
        let text = render_provenance(Some(project), None, "default", None, &[], &[]);
        assert!(
            text.contains("project config: .sendra/config.yaml\n"),
            "{text}"
        );
        assert!(text.contains("global config: none found\n"), "{text}");
    }

    #[test]
    fn a_global_config_alone_names_only_the_global_path() {
        let global = Path::new("/home/user/.config/sendra/config.yaml");
        let text = render_provenance(None, Some(global), "default", None, &[], &[]);
        assert!(text.contains("project config: none found\n"), "{text}");
        assert!(
            text.contains("global config: /home/user/.config/sendra/config.yaml\n"),
            "{text}"
        );
    }

    #[test]
    fn both_config_files_are_named_when_both_apply() {
        let project = Path::new(".sendra/config.yaml");
        let global = Path::new("/home/user/.config/sendra/config.yaml");
        let text = render_provenance(Some(project), Some(global), "default", None, &[], &[]);
        assert!(
            text.contains("project config: .sendra/config.yaml\n"),
            "{text}"
        );
        assert!(
            text.contains("global config: /home/user/.config/sendra/config.yaml\n"),
            "{text}"
        );
    }

    #[test]
    fn a_resolved_environment_names_itself_and_its_file() {
        let source = Path::new(".sendra/environments/staging.yaml");
        let text = render_provenance(None, None, "staging", Some(source), &[], &[]);
        assert!(
            text.contains("environment: staging → .sendra/environments/staging.yaml\n"),
            "{text}"
        );
    }

    #[test]
    fn an_unfound_environment_still_names_what_was_asked_for() {
        // The default fallback with no `default.yaml` on disk: the name
        // tried is still worth showing, since it explains *why* there is no
        // file rather than leaving the reader to guess.
        let text = render_provenance(None, None, "default", None, &[], &[]);
        assert!(
            text.contains("environment: default (none found)\n"),
            "{text}"
        );
    }

    #[test]
    fn var_overrides_are_named_but_not_valued() {
        let vars = vec![
            ("token".to_string(), "super-secret".to_string()),
            ("base_url".to_string(), "https://example.com".to_string()),
        ];
        let text = render_provenance(None, None, "default", None, &vars, &[]);
        assert!(text.contains("--var: 2 set (token, base_url)\n"), "{text}");
        assert!(
            !text.contains("super-secret") && !text.contains("https://example.com"),
            "values must not appear: {text}"
        );
    }

    #[test]
    fn header_overrides_are_named_but_not_valued() {
        let headers = vec![("Authorization".to_string(), "Bearer abc123".to_string())];
        let text = render_provenance(None, None, "default", None, &[], &headers);
        assert!(text.contains("-H: 1 set (Authorization)\n"), "{text}");
        assert!(
            !text.contains("abc123"),
            "the header value must not appear: {text}"
        );
    }

    #[test]
    fn no_overrides_reports_none_for_both() {
        let text = render_provenance(None, None, "default", None, &[], &[]);
        assert!(text.contains("--var: none\n"), "{text}");
        assert!(text.contains("-H: none\n"), "{text}");
    }
}
