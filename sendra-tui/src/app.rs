use ratatui::widgets::Paragraph;
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
    Loaded(Box<Document>),
    Failed(SendraError),
}

#[derive(Debug)]
pub enum Message {
    Quit,
    Tick,
    NoCollectionPath,
    CollectionLoaded(Box<Result<Document, SendraError>>),
}

pub fn update(state: &mut AppState, msg: Message) {
    match msg {
        Message::Quit => state.should_quit = true,
        Message::Tick => {}
        Message::NoCollectionPath => state.load_state = LoadState::NoPathProvided,
        Message::CollectionLoaded(result) => {
            state.load_state = match *result {
                Ok(document) => LoadState::Loaded(Box::new(document)),
                Err(error) => LoadState::Failed(error),
            };
        }
    }
}

pub fn view(state: &AppState, frame: &mut Frame) {
    let text = match &state.load_state {
        LoadState::Loading => "Loading collection...".to_string(),
        LoadState::NoPathProvided => "No collection path provided.".to_string(),
        LoadState::Loaded(document) => format!("Loaded {} request(s)", document.requests().len()),
        LoadState::Failed(error) => format!("Failed to load collection: {error}"),
    };
    frame.render_widget(Paragraph::new(text), frame.area());
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

    const MALFORMED_YAML: &str = "requests: [this is not valid yaml";

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
    fn collection_loaded_ok_stores_document() {
        let mut state = AppState::default();
        let document = Document::from_yaml_str(VALID_COLLECTION).expect("valid test YAML");

        update(
            &mut state,
            Message::CollectionLoaded(Box::new(Ok(document))),
        );

        match state.load_state {
            LoadState::Loaded(document) => assert_eq!(document.requests().len(), 2),
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
}
