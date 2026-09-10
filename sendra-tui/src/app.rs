use std::path::{Path, PathBuf};

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use sendra_core::{Document, Environment, Request, SendraError};

/// Body preview is capped rather than shown in full — scrolling through a
/// large body is issue 13's job; this just keeps a multi-megabyte body from
/// making every draw slower without ever crashing on one.
const MAX_BODY_PREVIEW_CHARS: usize = 2000;

#[derive(Debug, Default)]
pub struct AppState {
    pub should_quit: bool,
    pub load_state: LoadState,
}

#[derive(Debug, Default)]
pub enum LoadState {
    #[default]
    Loading,
    NoPathProvided,
    Loaded {
        document: Box<Document>,
        selected: usize,
        /// Directory `body_file`/multipart paths in the resolved preview
        /// resolve relative to — the directory containing the collection's
        /// own YAML file, exactly as `Request::resolve_body` expects.
        base_dir: PathBuf,
    },
    Failed(SendraError),
}

#[derive(Debug)]
pub enum Message {
    Quit,
    Tick,
    NoCollectionPath,
    CollectionLoaded {
        base_dir: PathBuf,
        result: Box<Result<Document, SendraError>>,
    },
    SelectNext,
    SelectPrevious,
}

/// Moves `selected` by `delta` (`1` or `-1`) through `len` items, wrapping at
/// both ends: past the last item goes to the first, and back past the first
/// goes to the last. A `len` of `0` leaves `selected` at `0`.
fn move_selection(selected: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let len = len as isize;
    let next = (selected as isize + delta).rem_euclid(len);
    next as usize
}

pub fn update(state: &mut AppState, msg: Message) {
    match msg {
        Message::Quit => state.should_quit = true,
        Message::Tick => {}
        Message::NoCollectionPath => state.load_state = LoadState::NoPathProvided,
        Message::CollectionLoaded { base_dir, result } => {
            state.load_state = match *result {
                Ok(document) => LoadState::Loaded {
                    document: Box::new(document),
                    selected: 0,
                    base_dir,
                },
                Err(error) => LoadState::Failed(error),
            };
        }
        Message::SelectNext => {
            if let LoadState::Loaded {
                document, selected, ..
            } = &mut state.load_state
            {
                *selected = move_selection(*selected, document.requests().len(), 1);
            }
        }
        Message::SelectPrevious => {
            if let LoadState::Loaded {
                document, selected, ..
            } = &mut state.load_state
            {
                *selected = move_selection(*selected, document.requests().len(), -1);
            }
        }
    }
}

pub fn view(state: &AppState, frame: &mut Frame) {
    match &state.load_state {
        LoadState::Loading => render_message(frame, "Loading collection..."),
        LoadState::NoPathProvided => render_message(frame, "No collection path provided."),
        LoadState::Failed(error) => {
            render_message(frame, &format!("Failed to load collection: {error}"));
        }
        LoadState::Loaded {
            document,
            selected,
            base_dir,
        } => {
            let panes = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                .split(frame.area());

            render_request_list(frame, panes[0], document, *selected);
            render_detail_pane(frame, panes[1], document, *selected, base_dir);
        }
    }
}

fn render_message(frame: &mut Frame, text: &str) {
    frame.render_widget(Paragraph::new(text.to_string()), frame.area());
}

fn render_request_list(frame: &mut Frame, area: Rect, document: &Document, selected: usize) {
    let items: Vec<ListItem> = document
        .requests()
        .iter()
        .map(|request| {
            let name = request.name.as_deref().unwrap_or("(unnamed)");
            ListItem::new(format!("{} {name}", request.method))
        })
        .collect();

    let list = List::new(items).highlight_style(Style::new().add_modifier(Modifier::REVERSED));

    let mut list_state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn render_detail_pane(
    frame: &mut Frame,
    area: Rect,
    document: &Document,
    selected: usize,
    base_dir: &Path,
) {
    let Some(request) = document.requests().get(selected) else {
        frame.render_widget(Paragraph::new("No request selected."), area);
        return;
    };

    let text = match resolve_preview(request, base_dir) {
        Ok(resolved) => format_resolved_request(&resolved),
        // No environment is selectable yet (that lands in a later issue), so
        // this runs substitution against an empty environment: a request with
        // no `{{var}}` placeholders resolves exactly as it will once
        // environments exist, and one that does reference a variable surfaces
        // the real `SendraError::VariableNotFound` core itself would raise,
        // shown as-is rather than faked.
        Err(error) => format!("Could not resolve preview (no environment selected yet):\n{error}"),
    };

    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), area);
}

/// The same substitution + auth/query/body resolution pipeline sendra-cli's
/// `--dry-run` runs before sending, reused directly rather than
/// reimplemented. Deliberately narrower than the CLI's full pipeline in two
/// ways, both because this is a preview, not a run: it skips OAuth token
/// acquisition (`Request::resolve_oauth`), a real network call with no place
/// in drawing a frame, and it skips `Config::apply`, since the TUI has no
/// project `Config` loaded anywhere yet — so the headers shown here are the
/// request's own, substituted, not the final wire-level set an actual run
/// would send after config defaults are layered on.
fn resolve_preview(request: &Request, base_dir: &Path) -> Result<Request, SendraError> {
    Environment::default()
        .apply(request)
        .and_then(|request| request.resolve_auth())
        .and_then(|request| request.resolve_query())
        .and_then(|request| request.resolve_body(base_dir))
}

fn format_resolved_request(request: &Request) -> String {
    let mut lines = vec![
        format!("Method: {}", request.method),
        format!("URL:    {}", request.url),
        String::new(),
        "Headers:".to_string(),
    ];

    if request.headers.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for (name, value) in &request.headers {
            lines.push(format!("  {name}: {value}"));
        }
    }

    lines.push(String::new());
    lines.push("Body:".to_string());
    match request.body.as_deref().filter(|body| !body.is_empty()) {
        Some(body) => {
            let (preview, truncated) = truncate_body(body);
            lines.push(preview);
            if truncated {
                lines.push(format!(
                    "... [truncated to {MAX_BODY_PREVIEW_CHARS} characters]"
                ));
            }
        }
        None => lines.push("  (none)".to_string()),
    }

    lines.join("\n")
}

fn truncate_body(body: &str) -> (String, bool) {
    if body.chars().count() <= MAX_BODY_PREVIEW_CHARS {
        (body.to_string(), false)
    } else {
        (body.chars().take(MAX_BODY_PREVIEW_CHARS).collect(), true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_COLLECTION: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
  - name: Two
    method: GET
    url: https://example.com/two
";

    const THREE_REQUEST_COLLECTION: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
  - name: Two
    method: GET
    url: https://example.com/two
  - name: Three
    method: GET
    url: https://example.com/three
";

    const MALFORMED_YAML: &str = "requests: [this is not valid yaml";

    fn loaded_state(yaml: &str) -> AppState {
        let mut state = AppState::default();
        let document = Document::from_yaml_str(yaml).expect("valid test YAML");
        update(
            &mut state,
            Message::CollectionLoaded {
                base_dir: PathBuf::from("."),
                result: Box::new(Ok(document)),
            },
        );
        state
    }

    fn selected(state: &AppState) -> usize {
        match state.load_state {
            LoadState::Loaded { selected, .. } => selected,
            ref other => panic!("expected LoadState::Loaded, got {other:?}"),
        }
    }

    #[test]
    fn quit_message_sets_should_quit() {
        let mut state = AppState::default();
        assert!(!state.should_quit);

        update(&mut state, Message::Quit);

        assert!(state.should_quit);
    }

    #[test]
    fn tick_message_leaves_state_unchanged() {
        let mut state = AppState::default();

        update(&mut state, Message::Tick);

        assert!(!state.should_quit);
    }

    #[test]
    fn collection_loaded_ok_stores_document_with_selection_at_zero() {
        let mut state = AppState::default();
        let document = Document::from_yaml_str(VALID_COLLECTION).expect("valid test YAML");

        update(
            &mut state,
            Message::CollectionLoaded {
                base_dir: PathBuf::from("."),
                result: Box::new(Ok(document)),
            },
        );

        match state.load_state {
            LoadState::Loaded {
                document, selected, ..
            } => {
                assert_eq!(document.requests().len(), 2);
                assert_eq!(selected, 0);
            }
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        }
    }

    #[test]
    fn no_collection_path_message_sets_no_path_provided() {
        let mut state = AppState::default();

        update(&mut state, Message::NoCollectionPath);

        assert!(matches!(state.load_state, LoadState::NoPathProvided));
    }

    #[test]
    fn collection_loaded_err_stores_error() {
        let mut state = AppState::default();
        let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");

        update(
            &mut state,
            Message::CollectionLoaded {
                base_dir: PathBuf::from("."),
                result: Box::new(Err(error)),
            },
        );

        assert!(matches!(state.load_state, LoadState::Failed(_)));
    }

    #[test]
    fn select_next_advances_selection() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);

        update(&mut state, Message::SelectNext);

        assert_eq!(selected(&state), 1);
    }

    #[test]
    fn select_previous_moves_selection_back() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::SelectNext);
        update(&mut state, Message::SelectNext);

        update(&mut state, Message::SelectPrevious);

        assert_eq!(selected(&state), 1);
    }

    #[test]
    fn select_next_wraps_from_last_to_first() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::SelectNext);
        update(&mut state, Message::SelectNext);
        assert_eq!(selected(&state), 2);

        update(&mut state, Message::SelectNext);

        assert_eq!(selected(&state), 0);
    }

    #[test]
    fn select_previous_wraps_from_first_to_last() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        assert_eq!(selected(&state), 0);

        update(&mut state, Message::SelectPrevious);

        assert_eq!(selected(&state), 2);
    }

    #[test]
    fn selection_messages_are_ignored_without_a_loaded_collection() {
        let mut state = AppState::default();

        update(&mut state, Message::SelectNext);
        update(&mut state, Message::SelectPrevious);

        assert!(matches!(state.load_state, LoadState::Loading));
    }

    #[test]
    fn resolve_preview_succeeds_for_a_request_with_no_placeholders() {
        let document = Document::from_yaml_str(
            "name: One\nmethod: GET\nurl: https://example.com\nheaders:\n  Accept: application/json\n",
        )
        .expect("valid single request");
        let request = &document.requests()[0];

        let resolved =
            resolve_preview(request, Path::new(".")).expect("no placeholders to fail on");

        assert_eq!(resolved.url, "https://example.com");
    }

    #[test]
    fn resolve_preview_surfaces_variable_not_found_with_no_environment() {
        let document = Document::from_yaml_str(
            "name: One\nmethod: GET\nurl: https://example.com/{{user_id}}\n",
        )
        .expect("valid single request");
        let request = &document.requests()[0];

        let error = resolve_preview(request, Path::new(".")).expect_err(
            "a placeholder with no active environment must surface a real resolution error",
        );

        assert!(matches!(error, SendraError::VariableNotFound { .. }));
    }

    #[test]
    fn body_under_the_cap_is_shown_in_full() {
        let (preview, truncated) = truncate_body("short body");
        assert_eq!(preview, "short body");
        assert!(!truncated);
    }

    #[test]
    fn body_over_the_cap_is_truncated_not_panicked_on() {
        let huge = "x".repeat(MAX_BODY_PREVIEW_CHARS * 3);

        let (preview, truncated) = truncate_body(&huge);

        assert_eq!(preview.chars().count(), MAX_BODY_PREVIEW_CHARS);
        assert!(truncated);
    }
}
