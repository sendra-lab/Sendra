mod app;

use std::io::{self, Stdout};
use std::panic;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use sendra_core::config::find_project_config;
use sendra_core::Document;

use app::{update, view, AppState, Message};

#[derive(Parser)]
#[command(name = "sendra-tui")]
struct Cli {
    /// Collection or request YAML file to load. Defaults to `collection.yaml`
    /// in the project root (the directory containing `.sendra/`, found the
    /// same way sendra-cli finds its project config), or the current
    /// directory if no `.sendra/` project is found.
    path: Option<PathBuf>,
}

/// Same walk-up `find_project_config` uses for `.sendra/config.yaml` — reused
/// here, rather than re-walking the tree, to pick the directory a bare
/// `collection.yaml` default resolves against.
fn default_collection_path(start_dir: &Path) -> PathBuf {
    let project_root = find_project_config(start_dir)
        .and_then(|config_path| config_path.parent()?.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| start_dir.to_path_buf());

    project_root.join("collection.yaml")
}

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

fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    initial_message: Message,
) -> io::Result<()> {
    let mut state = AppState::default();
    update(&mut state, initial_message);

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

    let cli = Cli::parse();
    let start_dir = std::env::current_dir()?;
    let path = cli
        .path
        .unwrap_or_else(|| default_collection_path(&start_dir));

    // Loading is plain sendra-core I/O — no terminal touched yet — and its
    // result is handed to the event loop as an ordinary `Message`, so it
    // flows through the same `update()` path every other event does rather
    // than being special-cased.
    let load_message = Message::CollectionLoaded(Box::new(Document::from_path(&path)));

    let mut terminal = init_terminal()?;
    let result = run(&mut terminal, load_message);
    restore_terminal();

    result
}
