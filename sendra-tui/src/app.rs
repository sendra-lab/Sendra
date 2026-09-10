use std::path::{Path, PathBuf};

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use sendra_core::{Document, Environment, Request, SendraError};

/// Body preview is capped rather than shown in full — scrolling through a
/// large body is issue 13's job; this just keeps a multi-megabyte body from
/// making every draw slower without ever crashing on one.
const MAX_BODY_PREVIEW_CHARS: usize = 2000;

/// One environment sendra-tui found in `.sendra/environments/`, loaded (not
/// just named) so the overlay can show its variables before it's picked.
#[derive(Debug, Clone)]
pub struct NamedEnvironment {
    pub name: String,
    pub environment: Environment,
}

#[derive(Debug, Default)]
pub struct AppState {
    pub should_quit: bool,
    pub load_state: LoadState,
    /// Every environment discovered at startup, sorted by name.
    pub environments: Vec<NamedEnvironment>,
    /// Index into `environments` for the environment the detail pane
    /// resolves against, or `None` — the honest starting state from issue 5
    /// — until the user picks one in the overlay.
    pub active_environment: Option<usize>,
    /// `Some(cursor)` while the environment-picker overlay is open, `None`
    /// otherwise. The cursor is the overlay's own selection, separate from
    /// `active_environment`, so browsing the list and cancelling
    /// (`Message::CloseEnvironmentOverlay`) never touches what is active.
    pub environment_overlay: Option<usize>,
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
    EnvironmentsLoaded(Vec<NamedEnvironment>),
    /// Moves the collection-browser selection when the overlay is closed, or
    /// the overlay's own cursor when it is open — the same two messages
    /// issue 4 already wired to arrows/j-k, routed by `update()` to whichever
    /// list is currently on screen rather than adding a second pair of
    /// navigation messages for the overlay.
    SelectNext,
    SelectPrevious,
    OpenEnvironmentOverlay,
    /// Cancel: closes the overlay without changing `active_environment`.
    CloseEnvironmentOverlay,
    /// Confirm: sets `active_environment` to the overlay's cursor, then closes it.
    ConfirmEnvironmentSelection,
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
        Message::EnvironmentsLoaded(environments) => state.environments = environments,
        Message::SelectNext => select(state, 1),
        Message::SelectPrevious => select(state, -1),
        Message::OpenEnvironmentOverlay => {
            if state.environment_overlay.is_none() {
                state.environment_overlay = Some(state.active_environment.unwrap_or(0));
            }
        }
        Message::CloseEnvironmentOverlay => state.environment_overlay = None,
        Message::ConfirmEnvironmentSelection => {
            if let Some(cursor) = state.environment_overlay.take() {
                if !state.environments.is_empty() {
                    state.active_environment = Some(cursor);
                }
            }
        }
    }
}

/// Routes `SelectNext`/`SelectPrevious` to the overlay's cursor when it is
/// open, otherwise to the collection browser's selection — see the doc
/// comment on those `Message` variants for why one pair serves both lists.
fn select(state: &mut AppState, delta: isize) {
    if let Some(cursor) = &mut state.environment_overlay {
        *cursor = move_selection(*cursor, state.environments.len(), delta);
        return;
    }

    if let LoadState::Loaded {
        document, selected, ..
    } = &mut state.load_state
    {
        *selected = move_selection(*selected, document.requests().len(), delta);
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
            render_detail_pane(frame, panes[1], document, *selected, base_dir, state);
        }
    }

    if let Some(cursor) = state.environment_overlay {
        render_environment_overlay(frame, state, cursor);
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
    state: &AppState,
) {
    let Some(request) = document.requests().get(selected) else {
        frame.render_widget(Paragraph::new("No request selected."), area);
        return;
    };

    let active = active_environment(state);
    let environment = active.map_or_else(Environment::default, |named| named.environment.clone());

    let text = match resolve_preview(request, base_dir, &environment) {
        Ok(resolved) => format_resolved_request(&resolved),
        // Honest about what's actually active: with nothing selected yet
        // (issue 5's default), a `{{var}}` request surfaces the same
        // `VariableNotFound` core itself raises against an empty
        // environment; with one selected, the same error means that
        // environment specifically does not define the variable — either
        // way, the real sendra-core error is shown, never a faked value.
        Err(error) => match active {
            Some(named) => format!(
                "Could not resolve preview against environment '{}':\n{error}",
                named.name
            ),
            None => format!("Could not resolve preview (no environment selected yet):\n{error}"),
        },
    };

    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), area);
}

fn active_environment(state: &AppState) -> Option<&NamedEnvironment> {
    state
        .active_environment
        .and_then(|index| state.environments.get(index))
}

/// The same substitution + auth/query/body resolution pipeline sendra-cli's
/// `--dry-run` runs before sending, reused directly rather than
/// reimplemented. Deliberately narrower than the CLI's full pipeline in two
/// ways, both because this is a preview, not a run: it skips OAuth token
/// acquisition (`Request::resolve_oauth`), a real network call with no place
/// in drawing a frame, and it skips `Config::apply`, since the TUI has no
/// project `Config` loaded anywhere yet — so the headers shown here are the
/// request's own, substituted, not the final wire-level set an actual run
/// would send after config defaults are layered on. `environment` is passed
/// in as a real, caller-chosen value rather than assumed here — an empty one
/// when nothing is active, matching issue 5, or the one the user picked in
/// the overlay — so any environment-level default (its own `auth` block
/// included) resolves exactly as `Environment::apply` already defines it.
fn resolve_preview(
    request: &Request,
    base_dir: &Path,
    environment: &Environment,
) -> Result<Request, SendraError> {
    environment
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

/// A rect roughly `percent_x`/`percent_y` of `area`, centered within it — the
/// standard ratatui popup-centering pattern, pure layout math with nothing
/// project-specific to reuse from elsewhere.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

/// `area` shrunk by one cell on every side — where content goes inside a
/// bordered `Block` drawn over the same `area`.
fn inset(area: Rect) -> Rect {
    Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

fn render_environment_overlay(frame: &mut Frame, state: &AppState, cursor: usize) {
    let area = centered_rect(70, 70, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title("Select environment — Enter to confirm, Esc to cancel"),
        area,
    );

    let inner = inset(area);

    if state.environments.is_empty() {
        frame.render_widget(
            Paragraph::new("No environments found in .sendra/environments/."),
            inner,
        );
        return;
    }

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(inner);

    let items: Vec<ListItem> = state
        .environments
        .iter()
        .enumerate()
        .map(|(index, named)| {
            let marker = if Some(index) == state.active_environment {
                "* "
            } else {
                "  "
            };
            ListItem::new(format!("{marker}{}", named.name))
        })
        .collect();
    let list = List::new(items).highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut list_state = ListState::default().with_selected(Some(cursor));
    frame.render_stateful_widget(list, panes[0], &mut list_state);

    // The cursor's environment, not necessarily the active one — showing
    // what a user is about to pick, before they confirm it.
    let highlighted = &state.environments[cursor.min(state.environments.len() - 1)];
    let mut lines = vec![format!("Variables for '{}':", highlighted.name)];
    if highlighted.environment.variables.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for (name, value) in &highlighted.environment.variables {
            lines.push(format!("  {name} = {value}"));
        }
    }
    frame.render_widget(
        Paragraph::new(lines.join("\n")).wrap(Wrap { trim: false }),
        panes[1],
    );
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

    fn named_environment(name: &str, variables: &[(&str, &str)]) -> NamedEnvironment {
        let mut environment = Environment::default();
        for (key, value) in variables {
            environment
                .variables
                .insert((*key).to_string(), (*value).to_string());
        }
        NamedEnvironment {
            name: name.to_string(),
            environment,
        }
    }

    fn state_with_environments(names: &[&str]) -> AppState {
        AppState {
            environments: names
                .iter()
                .map(|name| named_environment(name, &[]))
                .collect(),
            ..AppState::default()
        }
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

        let resolved = resolve_preview(request, Path::new("."), &Environment::default())
            .expect("no placeholders to fail on");

        assert_eq!(resolved.url, "https://example.com");
    }

    #[test]
    fn resolve_preview_surfaces_variable_not_found_with_no_environment() {
        let document = Document::from_yaml_str(
            "name: One\nmethod: GET\nurl: https://example.com/{{user_id}}\n",
        )
        .expect("valid single request");
        let request = &document.requests()[0];

        let error = resolve_preview(request, Path::new("."), &Environment::default()).expect_err(
            "a placeholder with no active environment must surface a real resolution error",
        );

        assert!(matches!(error, SendraError::VariableNotFound { .. }));
    }

    #[test]
    fn resolve_preview_resolves_against_a_real_environment() {
        let document = Document::from_yaml_str(
            "name: One\nmethod: GET\nurl: https://example.com/{{user_id}}\n",
        )
        .expect("valid single request");
        let request = &document.requests()[0];
        let mut environment = Environment::default();
        environment
            .variables
            .insert("user_id".to_string(), "42".to_string());

        let resolved = resolve_preview(request, Path::new("."), &environment)
            .expect("the environment defines the variable the request needs");

        assert_eq!(resolved.url, "https://example.com/42");
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

    #[test]
    fn environments_loaded_message_populates_state() {
        let mut state = AppState::default();

        update(
            &mut state,
            Message::EnvironmentsLoaded(vec![named_environment("default", &[])]),
        );

        assert_eq!(state.environments.len(), 1);
        assert_eq!(state.environments[0].name, "default");
    }

    #[test]
    fn open_overlay_starts_cursor_at_active_environment() {
        let mut state = state_with_environments(&["default", "staging"]);
        state.active_environment = Some(1);

        update(&mut state, Message::OpenEnvironmentOverlay);

        assert_eq!(state.environment_overlay, Some(1));
    }

    #[test]
    fn overlay_navigation_moves_cursor_not_collection_selection() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        state.environments = ["default", "staging", "prod"]
            .into_iter()
            .map(|name| named_environment(name, &[]))
            .collect();
        update(&mut state, Message::OpenEnvironmentOverlay);

        update(&mut state, Message::SelectNext);

        assert_eq!(state.environment_overlay, Some(1));
        assert_eq!(selected(&state), 0, "collection selection must not move");
    }

    #[test]
    fn confirm_sets_active_environment_and_closes_overlay() {
        let mut state = state_with_environments(&["default", "staging"]);
        update(&mut state, Message::OpenEnvironmentOverlay);
        update(&mut state, Message::SelectNext);

        update(&mut state, Message::ConfirmEnvironmentSelection);

        assert_eq!(state.active_environment, Some(1));
        assert_eq!(state.environment_overlay, None);
    }

    #[test]
    fn cancel_closes_overlay_without_changing_active_environment() {
        let mut state = state_with_environments(&["default", "staging"]);
        state.active_environment = Some(0);
        update(&mut state, Message::OpenEnvironmentOverlay);
        update(&mut state, Message::SelectNext);
        assert_eq!(state.environment_overlay, Some(1));

        update(&mut state, Message::CloseEnvironmentOverlay);

        assert_eq!(state.environment_overlay, None);
        assert_eq!(
            state.active_environment,
            Some(0),
            "cancel must leave the previously active environment untouched"
        );
    }
}
