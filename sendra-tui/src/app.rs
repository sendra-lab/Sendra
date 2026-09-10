use ratatui::Frame;

#[derive(Debug, Default)]
pub struct AppState {
    pub should_quit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
    Quit,
    Tick,
}

pub fn update(state: &mut AppState, msg: Message) {
    match msg {
        Message::Quit => state.should_quit = true,
        Message::Tick => {}
    }
}

pub fn view(_state: &AppState, frame: &mut Frame) {
    frame.render_widget(ratatui::widgets::Clear, frame.area());
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
