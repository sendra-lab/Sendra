use std::path::{Path, PathBuf};

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use sendra_core::{Document, Environment, Request, Response, SendraError};

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
    /// The selected request's most recent run, if any has been started.
    pub run_state: RunState,
    /// Advanced by one on every `Message::Tick` (roughly every 100ms — see
    /// `next_message` in `main.rs`), and read only to pick which spinner
    /// glyph `render_status_bar` draws while `run_state` is `InFlight`. Not
    /// meaningful on its own; it exists purely to make the spinner animate.
    pub spinner_tick: usize,
    /// How many lines into the response panel's text the view is scrolled —
    /// see `render_response_panel`. Reset to `0` whenever it would otherwise
    /// point at a different run's text: a fresh `RunRequested` and a
    /// changed request-list selection both reset it, in `update` and
    /// `select` respectively.
    pub response_scroll: usize,
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
    /// Enter or `r` on the selected request. A no-op — see [`update`] — when a
    /// run is already in flight or nothing is loaded/selected; otherwise
    /// moves `run_state` to `RunState::InFlight`, which is `main`'s cue to
    /// actually spawn the request via [`crate::run_request::spawn`].
    RunRequested,
    /// The spawned run finished, with the real `sendra-core` result —
    /// success or failure — it produced. Always accepted, even while another
    /// message would normally be blocked by an in-flight run, since this is
    /// the message that ends that state.
    RunCompleted(Result<Response, SendraError>),
    /// PageDown on the response panel: scrolls its text down a few lines.
    /// A no-op whenever there is nothing showing that could scroll — see
    /// `render_response_panel`, which is the only place `response_scroll` is
    /// read and where the actual clamping against content length happens.
    ScrollResponseDown,
    /// PageUp on the response panel: scrolls its text up a few lines. See
    /// [`Message::ScrollResponseDown`].
    ScrollResponseUp,
}

/// What the selected request's most recent run did, if anything.
///
/// Holds the real `sendra_core::Response`/`SendraError` a run produced, not
/// a TUI-invented summary — `render_response_panel` formats it, but nothing
/// about the data itself is reshaped or approximated first.
///
/// **Always about the currently selected request.** Nothing here tracks
/// *which* request a completed run belongs to; instead, `select` resets this
/// back to `Idle` the moment the request-list selection actually moves, so
/// `Completed` can never be misread as an answer for a request other than
/// the one it was sent for.
#[derive(Debug, Default)]
pub enum RunState {
    #[default]
    Idle,
    InFlight,
    Completed(Result<Response, SendraError>),
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
    // While a run is in flight, every navigation/overlay/run message is
    // refused outright — the simplest correct behavior, and the one that
    // avoids the concurrent-state edge cases a second in-flight run, or a
    // selection change out from under one, would open up. `Quit`, `Tick` and
    // `RunCompleted` are exempt: quitting and the clock keep working
    // regardless, and `RunCompleted` is exactly the message that ends this
    // state, so blocking it would make the block permanent.
    if matches!(state.run_state, RunState::InFlight)
        && matches!(
            msg,
            Message::SelectNext
                | Message::SelectPrevious
                | Message::OpenEnvironmentOverlay
                | Message::CloseEnvironmentOverlay
                | Message::ConfirmEnvironmentSelection
                | Message::RunRequested
        )
    {
        return;
    }

    match msg {
        Message::Quit => state.should_quit = true,
        Message::Tick => state.spinner_tick = state.spinner_tick.wrapping_add(1),
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
        Message::RunRequested => {
            // A no-op with nothing loaded or nothing selected — there is no
            // request to run. `main` reads this same condition (a request
            // actually being selected) before deciding to spawn anything, so
            // the two checks have to agree: this one is what lets `main`
            // trust that "`run_state` became `InFlight`" means "there really
            // is a selected request to send".
            if request_is_selected(state) {
                state.run_state = RunState::InFlight;
                // A fresh run's text starts at the top, regardless of where
                // a previous run's was left scrolled.
                state.response_scroll = 0;
            }
        }
        Message::RunCompleted(result) => {
            state.run_state = RunState::Completed(result);
        }
        Message::ScrollResponseDown => {
            state.response_scroll = state.response_scroll.saturating_add(SCROLL_STEP_LINES);
        }
        Message::ScrollResponseUp => {
            state.response_scroll = state.response_scroll.saturating_sub(SCROLL_STEP_LINES);
        }
    }
}

/// Lines moved per `PageUp`/`PageDown` on the response panel. Not tied to
/// the pane's actual height — this is the minimal scroll the issue asks
/// for, not the full viewport-aware paging of issue 13 — so a fixed step
/// that comfortably outruns typical pane heights is simplest.
const SCROLL_STEP_LINES: usize = 10;

/// Whether the collection browser currently has a request selected — true
/// exactly when `load_state` is `Loaded` and `selected` indexes a real
/// request, which is always the case for a non-empty collection but not for
/// an empty one.
fn request_is_selected(state: &AppState) -> bool {
    matches!(
        &state.load_state,
        LoadState::Loaded { document, selected, .. } if document.requests().get(*selected).is_some()
    )
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
        let next = move_selection(*selected, document.requests().len(), delta);
        if next != *selected {
            // A different request's preview/response is about to show, so a
            // previous run's result — and its scroll position — belong to a
            // request no longer on screen. See the doc comment on
            // `RunState` for why this is what keeps `Completed` always
            // meaning "the currently selected request's result".
            state.run_state = RunState::Idle;
            state.response_scroll = 0;
        }
        *selected = next;
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
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(1)])
                .split(frame.area());

            let panes = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                .split(rows[0]);

            render_request_list(frame, panes[0], document, *selected);
            render_detail_pane(frame, panes[1], document, *selected, base_dir, state);
            render_status_bar(frame, rows[1], state);
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

/// Once a run has completed for the currently selected request, this pane
/// shows the result **instead of** the request preview — replace, not a
/// toggle or a second tab. Two reasons: there is no natural third state to
/// toggle back and forth between once the answer to "what did this request
/// actually do" exists (the preview is only ever a stand-in for that
/// answer), and `RunState` already resets to `Idle` on every request-list
/// selection change (see its doc comment), so the preview reappears on its
/// own the moment it is relevant again — a toggle key would be one more
/// binding for a state that already un-does itself.
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

    if let RunState::Completed(result) = &state.run_state {
        render_response_panel(frame, area, result, state.response_scroll);
        return;
    }

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
fn render_response_panel(
    frame: &mut Frame,
    area: Rect,
    result: &Result<Response, SendraError>,
    scroll: usize,
) {
    let text = format_run_result(result);

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
    let footer = format!(
        "Line {}-{} of {total} — PgUp/PgDn to scroll",
        scroll.saturating_add(1).min(total.max(1)),
        last_visible,
    );
    frame.render_widget(
        Paragraph::new(footer).style(Style::new().add_modifier(Modifier::DIM)),
        rows[1],
    );
}

/// Mirrors sendra-cli's own response layout
/// (`sendra-cli/src/output/human.rs::print_response`/`print_status_line`)
/// closely enough that the two are a direct side-by-side match for the same
/// response: status code, status text and elapsed time on one line, every
/// header in the order the response actually carried them, a blank line,
/// then the body. Uncoloured, unlike the CLI's terminal output — colour is
/// the one thing this deliberately does not reproduce, since ratatui styling
/// is a separate concern from the data being correct.
/// The text `render_response_panel` shows: `format_response`'s layout on
/// success, or the real `SendraError`'s own `Display` — via `{error}`, the
/// same `thiserror`-derived message `sendra run` itself would print for the
/// same failure — on failure. On the failure path there is no response to
/// lay out, so this is the whole of it: the error, not a TUI-invented
/// summary of it.
fn format_run_result(result: &Result<Response, SendraError>) -> String {
    match result {
        Ok(response) => format_response(response),
        Err(error) => format!("Request failed:\n{error}"),
    }
}

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

/// Cycled by `spinner_tick` while a run is in flight — an ordinary braille
/// spinner, no library, since ratatui ships no widget for one.
const SPINNER_FRAMES: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];

/// The one-line status bar under the panes: idle hint, in-flight spinner, or
/// a one-line summary of what `render_response_panel` is showing in full
/// above it — this line never carries anything the panel doesn't already
/// say, it just makes the outcome visible even when the panel itself has
/// scrolled somewhere else.
fn render_status_bar(frame: &mut Frame, area: Rect, state: &AppState) {
    let text = match &state.run_state {
        RunState::Idle => "Enter/r: run selected request".to_string(),
        RunState::InFlight => {
            let frame_char = SPINNER_FRAMES[state.spinner_tick % SPINNER_FRAMES.len()];
            format!("{frame_char} Running request...")
        }
        RunState::Completed(Ok(response)) => {
            format!("Done — {} (Enter/r to run again)", response.status)
        }
        RunState::Completed(Err(error)) => {
            format!("Failed — {error} (Enter/r to run again)")
        }
    };
    frame.render_widget(Paragraph::new(text), area);
}

/// The environment the detail pane resolves against and a run sends
/// against — the one `active_environment` names, or `None` before the user
/// has picked one. `pub` (not `fn`-private) because `main` needs the exact
/// same lookup to resolve a run's request against, rather than a second
/// implementation of "which environment is active" living outside `app.rs`.
pub fn active_environment(state: &AppState) -> Option<&NamedEnvironment> {
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

    fn sample_response(status: u16) -> Response {
        Response {
            status,
            status_text: "OK".to_string(),
            headers: Vec::new(),
            body: String::new(),
            elapsed: std::time::Duration::from_millis(1),
            redirects: Vec::new(),
        }
    }

    #[test]
    fn run_requested_moves_run_state_to_in_flight_when_a_request_is_selected() {
        let mut state = loaded_state(VALID_COLLECTION);

        update(&mut state, Message::RunRequested);

        assert!(matches!(state.run_state, RunState::InFlight));
    }

    #[test]
    fn run_requested_is_a_no_op_with_nothing_loaded() {
        let mut state = AppState::default();

        update(&mut state, Message::RunRequested);

        assert!(matches!(state.run_state, RunState::Idle));
    }

    #[test]
    fn run_completed_stores_the_real_response_in_run_state() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);

        update(&mut state, Message::RunCompleted(Ok(sample_response(200))));

        match state.run_state {
            RunState::Completed(Ok(response)) => assert_eq!(response.status, 200),
            other => panic!("expected RunState::Completed(Ok(_)), got {other:?}"),
        }
    }

    #[test]
    fn run_completed_stores_the_real_error_in_run_state() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");

        update(&mut state, Message::RunCompleted(Err(error)));

        assert!(matches!(state.run_state, RunState::Completed(Err(_))));
    }

    #[test]
    fn navigation_and_a_second_run_are_blocked_while_a_run_is_in_flight() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        state.environments = ["default", "staging"]
            .into_iter()
            .map(|name| named_environment(name, &[]))
            .collect();
        update(&mut state, Message::RunRequested);
        assert!(matches!(state.run_state, RunState::InFlight));

        update(&mut state, Message::SelectNext);
        update(&mut state, Message::SelectPrevious);
        update(&mut state, Message::OpenEnvironmentOverlay);
        update(&mut state, Message::RunRequested);

        assert_eq!(
            selected(&state),
            0,
            "selection must not move while a run is in flight"
        );
        assert_eq!(
            state.environment_overlay, None,
            "the environment overlay must not open while a run is in flight"
        );
        assert!(
            matches!(state.run_state, RunState::InFlight),
            "a second RunRequested must not restart or otherwise disturb the in-flight run"
        );
    }

    #[test]
    fn run_completed_is_accepted_while_a_run_is_in_flight() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);

        update(&mut state, Message::RunCompleted(Ok(sample_response(204))));

        assert!(
            matches!(state.run_state, RunState::Completed(Ok(_))),
            "RunCompleted must end the in-flight state even though it would \
             otherwise be blocked by it"
        );
    }

    #[test]
    fn navigation_works_again_once_a_run_has_completed() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::RunRequested);
        update(&mut state, Message::RunCompleted(Ok(sample_response(200))));

        update(&mut state, Message::SelectNext);

        assert_eq!(selected(&state), 1);
    }

    #[test]
    fn quit_still_works_while_a_run_is_in_flight() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);

        update(&mut state, Message::Quit);

        assert!(state.should_quit);
    }

    #[test]
    fn selecting_a_different_request_resets_run_state_and_scroll() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::RunRequested);
        update(&mut state, Message::RunCompleted(Ok(sample_response(200))));
        update(&mut state, Message::ScrollResponseDown);
        assert!(matches!(state.run_state, RunState::Completed(Ok(_))));
        assert_eq!(state.response_scroll, SCROLL_STEP_LINES);

        update(&mut state, Message::SelectNext);

        assert!(
            matches!(state.run_state, RunState::Idle),
            "a previous run's result must not be attributed to the newly selected request"
        );
        assert_eq!(state.response_scroll, 0);
    }

    #[test]
    fn selecting_the_same_request_again_does_not_reset_run_state() {
        // A collection of one: `SelectNext`/`SelectPrevious` always land back
        // on the same request, so nothing about the result should be
        // disturbed — only an actual change of selection resets it.
        let mut state = loaded_state(VALID_COLLECTION);
        // Trim to a single request so every `SelectNext` is a no-op move.
        if let LoadState::Loaded { document, .. } = &mut state.load_state {
            let mut trimmed = Document::from_yaml_str(
                "name: test\nrequests:\n  - name: One\n    method: GET\n    url: https://example.com\n",
            )
            .unwrap();
            std::mem::swap(document.as_mut(), &mut trimmed);
        }
        update(&mut state, Message::RunRequested);
        update(&mut state, Message::RunCompleted(Ok(sample_response(200))));

        update(&mut state, Message::SelectNext);

        assert!(matches!(state.run_state, RunState::Completed(Ok(_))));
    }

    #[test]
    fn starting_a_new_run_resets_the_previous_scroll_position() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        update(&mut state, Message::RunCompleted(Ok(sample_response(200))));
        update(&mut state, Message::ScrollResponseDown);
        assert_eq!(state.response_scroll, SCROLL_STEP_LINES);

        update(&mut state, Message::RunRequested);

        assert_eq!(state.response_scroll, 0);
    }

    #[test]
    fn scroll_down_then_up_returns_to_the_top() {
        let mut state = AppState::default();

        update(&mut state, Message::ScrollResponseDown);
        assert_eq!(state.response_scroll, SCROLL_STEP_LINES);

        update(&mut state, Message::ScrollResponseUp);
        assert_eq!(state.response_scroll, 0);
    }

    #[test]
    fn scroll_up_from_the_top_saturates_at_zero() {
        let mut state = AppState::default();

        update(&mut state, Message::ScrollResponseUp);

        assert_eq!(state.response_scroll, 0);
    }

    fn response_with(headers: &[(&str, &str)], body: &str) -> Response {
        Response {
            status: 201,
            status_text: "Created".to_string(),
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            body: body.to_string(),
            elapsed: std::time::Duration::from_millis(42),
            redirects: Vec::new(),
        }
    }

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
    fn run_completed_err_renders_the_real_sendra_error_text() {
        let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");
        let expected = error.to_string();

        let text = format_run_result(&Err(error));

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
        let result: Result<Response, SendraError> = Ok(response_with(&[], &huge_body));

        // 5000 body lines, plus the status line and the blank separator line
        // `format_response` always puts ahead of a non-empty body.
        let total_lines = 5002;
        let backend = TestBackend::new(60, 5);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");

        // Scrolled absurdly far past the end of the content — proving the
        // clamp in `render_response_panel` (not just ratatui's own
        // clipping) keeps the footer's line numbers sane rather than
        // reporting a scroll position past `total`.
        terminal
            .draw(|frame| {
                render_response_panel(frame, frame.area(), &result, usize::MAX);
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
        update(&mut state, Message::RunCompleted(Ok(response)));

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
        update(&mut state, Message::RunCompleted(Err(error)));

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

    fn buffer_to_string(buffer: &ratatui::buffer::Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
