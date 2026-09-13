//! A run's response panel — `render_response_panel` and the `format_*`
//! helpers it builds on (`format_run_result`, `format_assertions`,
//! `format_capture_section`, `format_response`, `body_for_display`,
//! `claims_json`) — plus the run-history browser, `render_history_overlay`,
//! which renders a past entry through this exact same response panel.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use sendra_core::{AssertionReport, CaptureReport, Response};

use crate::run_request::RunOutcome;

use super::super::state::{AppState, HistoryOverlay, RunHistoryEntry, RUN_HISTORY_CAP};
use super::{format_error, modal_frame};

/// The completed-run half of the detail pane: a real response's status,
/// headers and body, or the real `SendraError` that stopped it — pulled
/// straight from the `Response`/`SendraError` already sitting in
/// `RunState::Completed`, nothing recomputed or re-derived.
///
/// **Not wrapped, and scrolled by whole lines only.** The request preview
/// above wraps long lines because a request body is authored by the same
/// person reading it and rarely wide; a response body has no such
/// guarantee — a minified JSON payload is one line that can run to
/// thousands of characters — so wrapping it would make `response_scroll`'s
/// "line N" meaningless (a wrapped line renders as several visual rows,
/// and the count depends on pane width). Scrolling raw lines and letting a
/// too-wide one clip at the pane edge is the same trade-off `less` (without
/// `-S`) and most response viewers make, and it is what keeps the scroll
/// math in this function simple and always correct rather than an
/// approximation of ratatui's own wrapping.
///
/// The last line is always a footer — `Line a-b of n` plus the scroll keys
/// — never only shown once content overflows, so the pane never scrolls
/// silently: there is always something on screen saying whether there is
/// more, exactly the same posture `truncate_body` takes with its own
/// `[truncated to N characters]` marker.
pub(crate) fn render_response_panel(
    frame: &mut Frame,
    area: Rect,
    outcome: &RunOutcome,
    scroll: usize,
    reveal_captures: bool,
) {
    let text = format_run_result(outcome, reveal_captures);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let content_height = rows[0].height as usize;
    let max_scroll = total.saturating_sub(content_height.max(1));
    let scroll = scroll.min(max_scroll);

    let paragraph = Paragraph::new(text.clone()).scroll((scroll.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(paragraph, rows[0]);

    let last_visible = (scroll + content_height).min(total);
    let reveal_hint = if outcome.capture.is_empty() {
        String::new()
    } else if reveal_captures {
        "  |  c: hide captures".to_string()
    } else {
        "  |  c: reveal captures".to_string()
    };
    let footer = format!(
        "Line {}-{} of {total} — PgUp/PgDn to scroll{reveal_hint}",
        scroll.saturating_add(1).min(total.max(1)),
        last_visible,
    );
    frame.render_widget(
        Paragraph::new(footer).style(Style::new().add_modifier(Modifier::DIM)),
        rows[1],
    );
}

/// The text `render_response_panel` shows: on success, the response laid out
/// by `format_response`, followed by the assertion and capture sections
/// (`format_assertions`/`format_capture_section`) — always both, even when
/// their reports are empty, so "no assertions declared" reads distinctly
/// from either a passing or a failing assertion block, and likewise for
/// captures. On failure, [`format_error`] over the real
/// [`run_request::RunError`]'s own `Display` — the same wording
/// `sendra run` itself would print for the same `sendra_core::SendraError`
/// (`RunError::Core`), or, for the one failure that is not core's to report
/// (`RunError::RuntimeUnavailable` — see that variant's doc comment), the
/// honest reason the pipeline never even reached the network. Either way
/// there is no response to lay out and nothing was checked or captured, so
/// this is the whole of it: the real error, not a TUI-invented summary of
/// it and not two sections falsely claiming "no assertions declared" for a
/// request that may well have some.
pub(crate) fn format_run_result(outcome: &RunOutcome, reveal_captures: bool) -> String {
    match &outcome.result {
        Ok(response) => {
            let mut text = format_response(response);
            text.push('\n');
            text.push_str(&format_assertions(&outcome.assertions));
            text.push('\n');
            text.push_str(&format_capture_section(&outcome.capture, reveal_captures));
            text
        }
        Err(error) => format_error("Request failed", error),
    }
}

/// `assertions` heading, one line per check (`✓`/`✗` plus core's own
/// expectation/failure wording — [`sendra_core::AssertionResult`] renders
/// the words, this only lays them out), then a pass/fail count — the same
/// per-assertion granularity and the same wording as sendra-cli's own
/// `print_assertions` in `sendra-cli/src/output/human.rs`, reimplemented
/// (uncoloured) rather than imported for the same cross-crate reason
/// `body_for_display` is. **Declared distinctly from "declared and all
/// passed"**: an empty report — no `assertions:` block, or an empty one —
/// renders `no assertions declared` instead of silently matching the "0
/// failed" case a passing block would also produce, mirroring the
/// skipped-vs-passed distinction sendra-cli's own `--junit` output already
/// makes for the same report type.
fn format_assertions(report: &AssertionReport) -> String {
    if report.is_empty() {
        return "assertions\n  no assertions declared".to_string();
    }

    let mut lines = vec!["assertions".to_string()];
    for result in report.results() {
        match &result.failure {
            None => lines.push(format!("  ✓ {}", result.expectation)),
            Some(detail) => lines.push(format!("  ✗ {} — {detail}", result.expectation)),
        }
    }

    if report.passed() {
        lines.push(format!("  {} passed", report.passed_count()));
    } else {
        lines.push(format!(
            "  {} passed, {} failed",
            report.passed_count(),
            report.failed_count()
        ));
    }

    lines.join("\n")
}

/// A captured value that has not been revealed this session — the TUI's own
/// interactive counterpart to `--show-captures`'s default, not a value
/// sendra-cli itself ever prints to a terminal (its own `print_capture`
/// never shows the value at all, revealed or not; only `--json` carries it,
/// gated by that same flag). Same placeholder text `--json`'s
/// `REDACTED_CAPTURE_VALUE` uses (`sendra-cli/src/output/json.rs`), so the
/// two tools agree on what "hidden" is spelled as.
pub(crate) const REDACTED_CAPTURE_VALUE: &str = "<redacted>";

/// `capture` heading, one line per entry — a captured value shown as
/// [`REDACTED_CAPTURE_VALUE`] unless `reveal` is true, a failed entry's real
/// `CaptureFailure` message always shown regardless of `reveal` (a failure
/// never carried a value to begin with, so there is nothing to redact — the
/// same rule `--json`'s `CaptureRecord` follows: "failures is never
/// redacted") — then, like [`format_assertions`], `no captures declared`
/// for an empty report rather than looking like a capture block that
/// declared nothing wrong.
///
/// `reveal` is `AppState::reveal_captures` — session-only and reset on every
/// new run and every selection change (see its doc comment); nothing here
/// writes it anywhere durable, so a masked capture is masked again the next
/// time this function runs unless the user asks again.
fn format_capture_section(report: &CaptureReport, reveal: bool) -> String {
    if report.is_empty() {
        return "capture\n  no captures declared".to_string();
    }

    let mut lines = vec!["capture".to_string()];
    for result in report.results() {
        let from = format!("{} from `{}`", result.variable, result.path);
        match result.failure() {
            None => {
                let value = if reveal {
                    result.value().unwrap_or_default()
                } else {
                    REDACTED_CAPTURE_VALUE
                };
                lines.push(format!("  ✓ {from} = {value}"));
            }
            Some(failure) => lines.push(format!("  ✗ {from} — {failure}")),
        }
    }

    lines.join("\n")
}

/// Mirrors sendra-cli's own response layout
/// (`sendra-cli/src/output/human.rs::print_response`/`print_status_line`)
/// closely enough that the two are a direct side-by-side match for the same
/// response: status code, status text and elapsed time on one line, every
/// header in the order the response actually carried them, a blank line,
/// then the body. Uncoloured, unlike the CLI's terminal output — colour is
/// the one thing this deliberately does not reproduce, since ratatui styling
/// is a separate concern from the data being correct.
fn format_response(response: &Response) -> String {
    let mut lines = vec![format!(
        "{} {}  {} ms",
        response.status,
        response.status_text,
        response.elapsed.as_millis()
    )];

    for (name, value) in &response.headers {
        lines.push(format!("{name}: {value}"));
    }

    if !response.body.is_empty() {
        lines.push(String::new());
        lines.push(body_for_display(response));
    }

    lines.join("\n")
}

/// Pretty-prints the body when `Content-Type` claims JSON and it actually
/// parses as JSON, otherwise returns it unchanged — the same rule, sniffed
/// the same way, as sendra-cli's `body_for_display`/`claims_json` in
/// `output/human.rs`. Reimplemented here rather than imported, since
/// sendra-tui depends on sendra-core and not on sendra-cli, and this is
/// display formatting, not something sendra-core itself does or should do.
fn body_for_display(response: &Response) -> String {
    if !claims_json(&response.headers) {
        return response.body.clone();
    }

    match serde_json::from_str::<serde_json::Value>(&response.body) {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|_| response.body.clone()),
        Err(_) => response.body.clone(),
    }
}

/// Whether these response headers say the body is JSON — see the doc
/// comment on [`body_for_display`] for why this mirrors sendra-cli's own
/// `claims_json` instead of calling it.
fn claims_json(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .any(|(_, value)| {
            let media_type = value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            media_type == "application/json" || media_type.ends_with("+json")
        })
}

/// The run-history browser (`CollectionSession::history_overlay`) — a list
/// of the selected request's past runs, or (once `Message::ViewHistoryEntry`
/// picks one) that entry's full result through the exact same
/// `render_response_panel` a live run's own response uses, so a historical
/// entry is never shown through a second, differently-formatted rendering
/// path — see `RunHistoryEntry`'s own doc comment.
pub(crate) fn render_history_overlay(
    frame: &mut Frame,
    state: &AppState,
    overlay: &HistoryOverlay,
) {
    let entries = state.selected_history();

    if let Some(index) = overlay.viewing {
        let title = match entries.get(index) {
            Some(entry) => format!(
                "Run history — {} ago (esc back)",
                format_elapsed(entry.completed_at)
            ),
            None => "Run history (esc back)".to_string(),
        };
        let inner = modal_frame(frame, 85, 85, title);
        match entries.get(index) {
            Some(entry) => render_response_panel(
                frame,
                inner,
                &entry.outcome,
                overlay.view_scroll,
                overlay.view_reveal_captures,
            ),
            None => {
                frame.render_widget(Paragraph::new("This run is no longer available."), inner);
            }
        }
        return;
    }

    let inner = modal_frame(
        frame,
        70,
        70,
        "Run history — Enter to view, Space to expand, Esc to close",
    );

    if entries.is_empty() {
        frame.render_widget(
            Paragraph::new("No runs yet for this request. Press enter/r to run it."),
            inner,
        );
        return;
    }

    let items: Vec<ListItem> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let summary = match &entry.outcome.result {
                Ok(response) => {
                    let assertions = if entry.outcome.assertions.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " — {} passed, {} failed",
                            entry.outcome.assertions.passed_count(),
                            entry.outcome.assertions.failed_count()
                        )
                    };
                    format!("{}{assertions}", response.status)
                }
                Err(error) => format!("failed — {error}"),
            };
            let marker = if overlay.expanded.contains(&index) {
                "v"
            } else {
                ">"
            };
            let heading = Line::from(format!(
                "{marker} {} ago  —  {summary}",
                format_elapsed(entry.completed_at)
            ));
            if !overlay.expanded.contains(&index) {
                return ListItem::new(heading);
            }
            let mut lines = vec![heading];
            lines.extend(
                format_history_entry_detail(entry)
                    .into_iter()
                    .map(Line::from),
            );
            ListItem::new(Text::from(lines))
        })
        .collect();
    let list = List::new(items).highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut list_state = ListState::default()
        .with_selected(Some(overlay.cursor.min(entries.len().saturating_sub(1))));

    let dropped = state.selected_history_dropped();
    if dropped == 0 {
        frame.render_stateful_widget(list, inner, &mut list_state);
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);
    frame.render_stateful_widget(list, rows[0], &mut list_state);
    let run_word = if dropped == 1 { "run" } else { "runs" };
    frame.render_widget(
        Paragraph::new(format!(
            "{dropped} older {run_word} dropped (history capped at {RUN_HISTORY_CAP} per request)"
        ))
        .style(Style::new().add_modifier(Modifier::DIM)),
        rows[1],
    );
}

/// The lines shown, indented, beneath a history-list row that
/// `Message::ToggleHistoryEntryExpanded` has expanded — genuinely more than
/// the one-line summary above it (which only ever shows a status code and a
/// pass/fail count), without switching into `ViewHistoryEntry`'s full
/// response panel: the status line (code, text, elapsed time — the same
/// triple `format_response`'s own first line carries), then the full
/// `format_assertions`/`format_capture_section` breakdown for a successful
/// run, or the real error for a failed one. Captures are never revealed here
/// regardless of `HistoryOverlay::view_reveal_captures` — that flag is about
/// `viewing`'s own response panel, not this inline glance, and a value
/// sitting unrevealed in the full view should not leak out through a
/// lighter-weight one.
fn format_history_entry_detail(entry: &RunHistoryEntry) -> Vec<String> {
    let body = match &entry.outcome.result {
        Ok(response) => {
            let mut text = format!(
                "{} {}  {} ms",
                response.status,
                response.status_text,
                response.elapsed.as_millis()
            );
            text.push('\n');
            text.push_str(&format_assertions(&entry.outcome.assertions));
            text.push('\n');
            text.push_str(&format_capture_section(&entry.outcome.capture, false));
            text
        }
        Err(error) => format_error("Request failed", error),
    };
    body.lines().map(|line| format!("    {line}")).collect()
}

/// A short, human "N ago" rendering of `when` relative to now — `"just now"`
/// under a second, otherwise whole seconds/minutes/hours, coarsest unit
/// only (`"2h"`, never `"2h 3m"`): enough to place a run in time relative to
/// the others in the list without pulling in a date/time-formatting
/// dependency this crate has no other use for. `when` in the future (a clock
/// adjustment mid-session, the only realistic cause) reads as `"just now"`
/// rather than a nonsensical negative duration.
fn format_elapsed(when: std::time::SystemTime) -> String {
    let elapsed = match when.elapsed() {
        Ok(elapsed) => elapsed,
        Err(_) => return "just now".to_string(),
    };
    let secs = elapsed.as_secs();
    if secs == 0 {
        "just now".to_string()
    } else if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

#[cfg(test)]
mod tests {
    use sendra_core::{AssertionReport, CaptureReport, Document};

    use crate::app::state::{AppState, Message};
    use crate::app::test_support::*;
    use crate::app::update::update;
    use crate::app::view::view;
    use crate::run_request::RunOutcome;

    use super::*;

    #[test]
    fn format_response_matches_the_status_headers_body_layout() {
        let response = response_with(&[("X-Request-Id", "abc123")], "plain text body");

        let text = format_response(&response);

        assert_eq!(
            text,
            "201 Created  42 ms\nX-Request-Id: abc123\n\nplain text body"
        );
    }

    #[test]
    fn format_response_omits_the_body_section_when_the_body_is_empty() {
        let response = response_with(&[], "");

        let text = format_response(&response);

        assert_eq!(text, "201 Created  42 ms");
    }

    #[test]
    fn body_for_display_pretty_prints_a_json_content_type() {
        let response = response_with(
            &[("Content-Type", "application/json; charset=utf-8")],
            "{\"id\":1,\"name\":\"widget\"}",
        );

        let displayed = body_for_display(&response);

        assert_eq!(displayed, "{\n  \"id\": 1,\n  \"name\": \"widget\"\n}");
    }

    #[test]
    fn body_for_display_leaves_non_json_content_types_untouched() {
        let response = response_with(&[("Content-Type", "text/plain")], "{\"id\":1}");

        let displayed = body_for_display(&response);

        assert_eq!(
            displayed, "{\"id\":1}",
            "a non-JSON content type must not be re-formatted"
        );
    }

    #[test]
    fn body_for_display_leaves_malformed_json_untouched() {
        let response = response_with(&[("Content-Type", "application/json")], "not json");

        let displayed = body_for_display(&response);

        assert_eq!(
            displayed, "not json",
            "a JSON content type whose body does not actually parse must be shown verbatim, not dropped or panicked on"
        );
    }

    #[test]
    fn claims_json_matches_a_vendor_json_suffix() {
        assert!(claims_json(&[(
            "content-type".to_string(),
            "application/vnd.api+json".to_string()
        )]));
    }

    #[test]
    fn format_assertions_marks_a_request_with_no_assertions_distinctly() {
        let text = format_assertions(&AssertionReport::default());

        assert_eq!(text, "assertions\n  no assertions declared");
    }

    #[test]
    fn format_assertions_shows_every_result_individually_with_pass_fail() {
        let response = response_with(&[], "");
        let report = evaluate_assertions(
            "method: GET\nurl: https://example.com\nassertions:\n  status: 201\n  status_in: [404]\n",
            &response,
        );

        let text = format_assertions(&report);

        assert!(
            text.contains("✓ status is 201"),
            "the passing assertion must be shown on its own line: {text}"
        );
        assert!(
            text.contains("✗ status is one of [404]"),
            "the failing assertion must be shown on its own line, not folded into an aggregate: {text}"
        );
        assert!(
            text.contains("1 passed, 1 failed"),
            "a mixed report must show both counts: {text}"
        );
        assert_ne!(
            text,
            format_assertions(&AssertionReport::default()),
            "a report with real (even if all-failing) results must not read the same as \
             \"no assertions declared\""
        );
    }

    #[test]
    fn format_assertions_all_passing_reads_differently_from_none_declared() {
        let response = response_with(&[], "");
        let report = evaluate_assertions(
            "method: GET\nurl: https://example.com\nassertions:\n  status: 201\n",
            &response,
        );

        let text = format_assertions(&report);

        assert!(text.contains("1 passed"));
        assert!(
            !text.contains("no assertions declared"),
            "an assertions block that all passed must not be confused with none being declared: {text}"
        );
    }

    #[test]
    fn format_capture_section_marks_a_request_with_no_captures_distinctly() {
        let text = format_capture_section(&CaptureReport::default(), true);

        assert_eq!(text, "capture\n  no captures declared");
    }

    #[test]
    fn format_capture_section_masks_the_value_by_default() {
        let response = response_with(&[("X-Token", "super-secret")], "");
        let report = evaluate_capture(
            "method: GET\nurl: https://example.com\ncapture:\n  token: {header: X-Token}\n",
            &response,
        );

        let path = report.results()[0].path.clone();
        let masked = format_capture_section(&report, false);

        assert!(
            masked.contains(&format!("token from `{path}` = {REDACTED_CAPTURE_VALUE}")),
            "the value must be masked by default: {masked}"
        );
        assert!(
            !masked.contains("super-secret"),
            "the real captured value must not appear when not revealed: {masked}"
        );
    }

    #[test]
    fn format_capture_section_reveals_the_value_when_asked() {
        let response = response_with(&[("X-Token", "super-secret")], "");
        let report = evaluate_capture(
            "method: GET\nurl: https://example.com\ncapture:\n  token: {header: X-Token}\n",
            &response,
        );

        let path = report.results()[0].path.clone();
        let revealed = format_capture_section(&report, true);

        assert!(
            revealed.contains(&format!("token from `{path}` = super-secret")),
            "the real value must appear once revealed: {revealed}"
        );
    }

    #[test]
    fn format_capture_section_never_masks_a_failure() {
        // No `X-Token` header in the response, so the capture fails — and a
        // failure never had a value to redact in the first place, the same
        // rule sendra-cli's own `--json` `CaptureRecord` follows.
        let response = response_with(&[], "");
        let report = evaluate_capture(
            "method: GET\nurl: https://example.com\ncapture:\n  token: {header: X-Token}\n",
            &response,
        );

        let path = report.results()[0].path.clone();
        let masked = format_capture_section(&report, false);
        let revealed = format_capture_section(&report, true);

        assert_eq!(
            masked, revealed,
            "a failed capture has no value, so masking it must not change its rendering"
        );
        assert!(
            !masked.contains(REDACTED_CAPTURE_VALUE),
            "a failure is shown as a failure, not as a redacted value: {masked}"
        );
        assert!(masked.contains(&format!("✗ token from `{path}`")));
    }

    #[test]
    fn view_masks_captures_by_default_reveals_on_toggle_and_remasks_on_a_new_run() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = loaded_state(VALID_COLLECTION);
        let response = response_with(&[("X-Token", "super-secret")], "");
        let capture = evaluate_capture(
            "method: GET\nurl: https://example.com\ncapture:\n  token: {header: X-Token}\n",
            &response,
        );
        let outcome = || RunOutcome {
            result: Ok(response.clone()),
            assertions: AssertionReport::default(),
            capture: capture.clone(),
        };

        let render = |state: &AppState| {
            let backend = TestBackend::new(100, 15);
            let mut terminal = Terminal::new(backend).expect("a test terminal builds");
            terminal
                .draw(|frame| view(state, frame))
                .expect("rendering must not panic");
            buffer_to_string(terminal.backend().buffer())
        };

        // A masked/revealed capture *line* is what's under test —
        // `= <redacted>` vs `= super-secret` — not whether "super-secret"
        // appears anywhere on screen at all: the raw `X-Token` response
        // header carries the same value and is shown in full regardless (by
        // design; see the doc comment on `format_capture_section`), so a
        // whole-screen search for the string would fail for the wrong
        // reason even when masking is working correctly.
        let capture_line_contains = |screen: &str, needle: &str| {
            screen
                .lines()
                .any(|line| line.contains("token from") && line.contains(needle))
        };

        update(&mut state, Message::RunRequested);
        let collection_id = state.active().id;
        update(
            &mut state,
            Message::RunCompleted {
                collection_id,
                outcome: outcome(),
            },
        );
        let masked_screen = render(&state);
        assert!(
            capture_line_contains(&masked_screen, REDACTED_CAPTURE_VALUE),
            "captures must be masked by default:\n{masked_screen}"
        );
        assert!(
            !capture_line_contains(&masked_screen, "super-secret"),
            "the capture line must not show the real value by default:\n{masked_screen}"
        );

        update(&mut state, Message::ToggleRevealCaptures);
        let revealed_screen = render(&state);
        assert!(
            capture_line_contains(&revealed_screen, "super-secret"),
            "the real value must be on the capture line after the reveal keybinding:\n{revealed_screen}"
        );

        update(&mut state, Message::RunRequested);
        let collection_id = state.active().id;
        update(
            &mut state,
            Message::RunCompleted {
                collection_id,
                outcome: outcome(),
            },
        );
        let next_run_screen = render(&state);
        assert!(
            capture_line_contains(&next_run_screen, REDACTED_CAPTURE_VALUE),
            "a new run must not carry the reveal over — masked again by default:\n{next_run_screen}"
        );
        assert!(
            !capture_line_contains(&next_run_screen, "super-secret"),
            "a new run's capture line must not still show the real value:\n{next_run_screen}"
        );
    }

    #[test]
    fn run_completed_err_renders_the_real_sendra_error_text() {
        let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");
        let expected = error.to_string();

        let text = format_run_result(&failed_outcome(error), false);

        assert!(
            text.contains(&expected),
            "the panel must show the real SendraError text, got: {text}"
        );
    }

    /// A large body, drawn into a small area, must not overflow the pane,
    /// panic, or scroll past its own content — the actual `render_widget`
    /// call proves this rather than just the scroll-offset arithmetic
    /// (`format_response`/`format_run_result` above), since ratatui's own
    /// clipping is part of what makes this safe.
    #[test]
    fn a_large_body_renders_into_a_small_area_without_panicking() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let huge_body = (0..5000)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = RunOutcome {
            result: Ok(response_with(&[], &huge_body)),
            assertions: AssertionReport::default(),
            capture: CaptureReport::default(),
        };

        // Computed from the real formatted text rather than hand-counted,
        // so this stays correct however `format_run_result` lays out the
        // status/headers/body plus the assertions/capture sections below it.
        let total_lines = format_run_result(&outcome, false).lines().count();
        let backend = TestBackend::new(60, 5);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");

        // Scrolled absurdly far past the end of the content — proving the
        // clamp in `render_response_panel` (not just ratatui's own
        // clipping) keeps the footer's line numbers sane rather than
        // reporting a scroll position past `total`.
        terminal
            .draw(|frame| {
                render_response_panel(frame, frame.area(), &outcome, usize::MAX, false);
            })
            .expect("drawing a huge, over-scrolled body must not panic");

        let buffer = terminal.backend().buffer();
        let footer_row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, buffer.area.height - 1)].symbol())
            .collect();
        assert!(
            footer_row.contains(&format!("of {total_lines}")),
            "the footer must report the real total line count, got: {footer_row:?}"
        );
        assert!(
            !footer_row.contains(&format!("of {}", total_lines + 1)),
            "an over-scroll must not be reported as if it went past the real total"
        );
    }

    /// `Home`/`End` end to end, through the real `update()` + `view()`: `End`
    /// jumps straight to the bottom of a long body in one step (no repeated
    /// `PageDown`s needed), and `Home` from there jumps straight back to the
    /// top.
    #[test]
    fn home_and_end_jump_the_response_panel_to_the_real_top_and_bottom() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let body = (0..300)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = RunOutcome {
            result: Ok(response_with(&[], &body)),
            assertions: AssertionReport::default(),
            capture: CaptureReport::default(),
        };
        let total_lines = format_run_result(&outcome, false).lines().count();

        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        let collection_id = state.active().id;
        update(
            &mut state,
            Message::RunCompleted {
                collection_id,
                outcome,
            },
        );

        let render = |state: &AppState| {
            let backend = TestBackend::new(60, 10);
            let mut terminal = Terminal::new(backend).expect("a test terminal builds");
            terminal
                .draw(|frame| view(state, frame))
                .expect("rendering must not panic");
            buffer_to_string(terminal.backend().buffer())
        };

        update(&mut state, Message::ScrollResponseBottom);
        let bottom_screen = render(&state);
        assert!(
            bottom_screen.contains(&format!("-{total_lines} of {total_lines}")),
            "End must jump straight to the real end of the body in one step, not \
             partway through it:\n{bottom_screen}"
        );

        update(&mut state, Message::ScrollResponseTop);
        let top_screen = render(&state);
        assert!(
            top_screen.contains("Line 1-"),
            "Home must jump straight back to the real top of the response:\n{top_screen}"
        );
        assert!(
            top_screen.contains("201 Created"),
            "the top of the response must show the status line:\n{top_screen}"
        );
    }

    /// Renders a completed successful run through the real `view()` — not
    /// just `format_response`/`render_response_panel` in isolation — and
    /// reads the actual character buffer back, so this is what the terminal
    /// would really show: proof the status, a header and the body all land
    /// on screen together, replacing the request preview as documented on
    /// `render_detail_pane`.
    #[test]
    fn view_renders_a_completed_success_as_a_real_response_panel() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        let response = response_with(&[("X-Request-Id", "abc123")], "hello world");
        let collection_id = state.active().id;
        update(
            &mut state,
            Message::RunCompleted {
                collection_id,
                outcome: RunOutcome {
                    result: Ok(response),
                    assertions: AssertionReport::default(),
                    capture: CaptureReport::default(),
                },
            },
        );

        let backend = TestBackend::new(100, 15);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        terminal
            .draw(|frame| view(&state, frame))
            .expect("rendering a completed run must not panic");

        let screen = buffer_to_string(terminal.backend().buffer());
        assert!(
            screen.contains("201 Created"),
            "the real status must be on screen:\n{screen}"
        );
        assert!(
            screen.contains("X-Request-Id: abc123"),
            "a real response header must be on screen:\n{screen}"
        );
        assert!(
            screen.contains("hello world"),
            "the real response body must be on screen:\n{screen}"
        );
    }

    /// Same as above for the failure path: a real `SendraError`'s message
    /// must appear on screen, not a generic "failed" placeholder.
    #[test]
    fn view_renders_a_completed_failure_with_the_real_error_message() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");
        let expected_message = error.to_string();
        let collection_id = state.active().id;
        update(
            &mut state,
            Message::RunCompleted {
                collection_id,
                outcome: failed_outcome(error),
            },
        );

        let backend = TestBackend::new(100, 15);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        terminal
            .draw(|frame| view(&state, frame))
            .expect("rendering a failed run must not panic");

        let screen = buffer_to_string(terminal.backend().buffer());
        assert!(
            screen.contains(&expected_message),
            "the real SendraError message must be on screen, not a generic \
             placeholder:\n{screen}\nexpected to find: {expected_message}"
        );
    }

    #[test]
    fn response_panel_scroll_reclamps_to_a_shrunk_area() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let body = (0..200)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = RunOutcome {
            result: Ok(response_with(&[], &body)),
            assertions: AssertionReport::default(),
            capture: CaptureReport::default(),
        };
        let total_lines = format_run_result(&outcome, false).lines().count();

        let backend = TestBackend::new(80, 40);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        // A scroll position that was in range for an 80x40 area but is
        // nowhere close to the top of the much shorter area used below.
        let deep_scroll = total_lines - 5;

        terminal
            .draw(|frame| {
                render_response_panel(frame, frame.area(), &outcome, deep_scroll, false);
            })
            .expect("initial draw must not panic");

        terminal.backend_mut().resize(80, 4);
        terminal
            .draw(|frame| {
                render_response_panel(frame, frame.area(), &outcome, deep_scroll, false);
            })
            .expect("drawing the same stale scroll offset into a shrunk area must not panic");

        let screen = buffer_to_string(terminal.backend().buffer());
        assert!(
            !screen.trim().is_empty(),
            "a re-clamped scroll must still show real content, not a blank pane:\n{screen}"
        );
        assert!(
            screen.contains(&format!("of {total_lines}")),
            "the footer must report the real total even after the area shrank:\n{screen}"
        );
    }

    /// Proof that expanding a history-list row shows genuinely more detail
    /// than the collapsed summary, in place, without switching into
    /// `ViewHistoryEntry`'s full response panel — and that collapsing it
    /// again removes that detail. The collapsed row only ever shows a status
    /// code and a pass/fail count (see the list-building code in
    /// `render_history_overlay`); the expanded row must additionally show
    /// the per-assertion breakdown `format_assertions` produces, which the
    /// collapsed summary never does.
    #[test]
    fn expanding_a_history_row_shows_more_detail_than_the_collapsed_summary() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = loaded_state(VALID_COLLECTION);
        let response = response_with(&[], "");
        let assertions = evaluate_assertions(
            "method: GET\nurl: https://example.com\nassertions:\n  status: 201\n",
            &response,
        );
        let outcome = RunOutcome {
            result: Ok(response),
            assertions,
            capture: CaptureReport::default(),
        };

        update(&mut state, Message::RunRequested);
        let collection_id = state.active().id;
        update(
            &mut state,
            Message::RunCompleted {
                collection_id,
                outcome,
            },
        );
        update(&mut state, Message::OpenHistoryOverlay);

        // Rendered through `render_history_overlay` directly, not the full
        // `view` — the live response panel behind the modal shows this same
        // outcome's assertion text too, which would make a whole-screen scan
        // pass even if the *overlay itself* never rendered any detail at all.
        let render = |state: &AppState| {
            let backend = TestBackend::new(100, 20);
            let mut terminal = Terminal::new(backend).expect("a test terminal builds");
            let overlay = state.history_overlay.clone().expect("just opened");
            terminal
                .draw(|frame| render_history_overlay(frame, state, &overlay))
                .expect("rendering must not panic");
            buffer_to_string(terminal.backend().buffer())
        };

        let collapsed = render(&state);
        assert!(
            !collapsed.contains("status is 201"),
            "the collapsed row must not already show the per-assertion detail:\n{collapsed}"
        );

        update(&mut state, Message::ToggleHistoryEntryExpanded);
        let expanded = render(&state);
        assert!(
            expanded.contains("status is 201"),
            "expanding the row must show the real assertion detail in place:\n{expanded}"
        );
        assert!(
            expanded.contains("1 passed"),
            "expanding the row must show the pass/fail breakdown:\n{expanded}"
        );

        update(&mut state, Message::ToggleHistoryEntryExpanded);
        let recollapsed = render(&state);
        assert!(
            !recollapsed.contains("status is 201"),
            "toggling again must collapse the row back down:\n{recollapsed}"
        );
    }

    /// Proof of the `RUN_HISTORY_CAP` eviction decision demonstrated live:
    /// running one request past the cap drops its oldest entry and the
    /// overlay says so ("N older runs were dropped") rather than discarding
    /// it silently.
    #[test]
    fn history_overlay_shows_the_dropped_count_once_the_cap_is_exceeded() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = loaded_state(VALID_COLLECTION);
        let collection_id = state.active().id;
        for _ in 0..=RUN_HISTORY_CAP {
            update(&mut state, Message::RunRequested);
            let outcome = RunOutcome {
                result: Ok(response_with(&[], "")),
                assertions: AssertionReport::default(),
                capture: CaptureReport::default(),
            };
            update(
                &mut state,
                Message::RunCompleted {
                    collection_id,
                    outcome,
                },
            );
        }
        update(&mut state, Message::OpenHistoryOverlay);

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        terminal
            .draw(|frame| view(&state, frame))
            .expect("rendering must not panic");
        let screen = buffer_to_string(terminal.backend().buffer());

        assert!(
            screen.contains("1 older run dropped"),
            "exactly one run was evicted by the cap and the overlay must say so:\n{screen}"
        );
        assert!(
            screen.contains(&format!("capped at {RUN_HISTORY_CAP} per request")),
            "the notice must name the real cap, not a hardcoded number:\n{screen}"
        );
    }
}
