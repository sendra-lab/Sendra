use ratatui::style::{Modifier, Style};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use sendra_core::{Document, SendraError};

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
    },
    Failed(SendraError),
}

#[derive(Debug)]
pub enum Message {
    Quit,
    Tick,
    NoCollectionPath,
    CollectionLoaded(Box<Result<Document, SendraError>>),
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
        Message::CollectionLoaded(result) => {
            state.load_state = match *result {
                Ok(document) => LoadState::Loaded {
                    document: Box::new(document),
                    selected: 0,
                },
                Err(error) => LoadState::Failed(error),
            };
        }
        Message::SelectNext => {
            if let LoadState::Loaded { document, selected } = &mut state.load_state {
                *selected = move_selection(*selected, document.requests().len(), 1);
            }
        }
        Message::SelectPrevious => {
            if let LoadState::Loaded { document, selected } = &mut state.load_state {
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
        LoadState::Loaded { document, selected } => render_request_list(frame, document, *selected),
    }
}

fn render_message(frame: &mut Frame, text: &str) {
    frame.render_widget(Paragraph::new(text.to_string()), frame.area());
}

fn render_request_list(frame: &mut Frame, document: &Document, selected: usize) {
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
    frame.render_stateful_widget(list, frame.area(), &mut list_state);
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
            Message::CollectionLoaded(Box::new(Ok(document))),
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
            Message::CollectionLoaded(Box::new(Ok(document))),
        );

        match state.load_state {
            LoadState::Loaded { document, selected } => {
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

        update(&mut state, Message::CollectionLoaded(Box::new(Err(error))));

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
}
