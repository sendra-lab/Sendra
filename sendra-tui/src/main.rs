mod app;

use std::io::{self, Stdout};
use std::panic;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::{update, view, AppState, Message};

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

fn install_panic_hook() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));
}

fn init_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

/// The only place allowed to touch crossterm event types directly — translates
/// a poll/read result into a `Message`, keeping `update`/`view` crossterm-agnostic.
fn next_message() -> io::Result<Message> {
    if !event::poll(Duration::from_millis(100))? {
        return Ok(Message::Tick);
    }

    match event::read()? {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            let is_quit = key.code == KeyCode::Char('q')
                || (key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL));
            if is_quit {
                Ok(Message::Quit)
            } else {
                Ok(Message::Tick)
            }
        }
        _ => Ok(Message::Tick),
    }
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    let mut state = AppState::default();

    loop {
        terminal.draw(|frame| view(&state, frame))?;

        let msg = next_message()?;
        update(&mut state, msg);

        if state.should_quit {
            return Ok(());
        }
    }
}

fn main() -> io::Result<()> {
    install_panic_hook();

    let mut terminal = init_terminal()?;
    let result = run(&mut terminal);
    restore_terminal();

    result
}
