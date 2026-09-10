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
use sendra_core::Document;

use app::{update, view, AppState, Message};

#[derive(Parser)]
#[command(name = "sendra-tui")]
struct Cli {
    /// Collection or request YAML file to load.
    path: Option<PathBuf>,
}

/// Where a resolved request's `body_file`/multipart paths resolve against:
/// the directory containing the collection's own YAML file, not the
/// process's current directory — mirrors sendra-cli's own `base_dir` helper
/// (`sendra-cli/src/run.rs`) for the same reason: `body_file: ./payload.json`
/// written inside a collection means the file beside it, wherever the
/// command was typed from. A bare filename with no parent resolves against
/// `.`, the same thing it already means to `path` itself.
fn base_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
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
                return Ok(Message::Quit);
            }

            match key.code {
                KeyCode::Down | KeyCode::Char('j') => Ok(Message::SelectNext),
                KeyCode::Up | KeyCode::Char('k') => Ok(Message::SelectPrevious),
                _ => Ok(Message::Tick),
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

    // Loading is plain sendra-core I/O — no terminal touched yet — and its
    // result is handed to the event loop as an ordinary `Message`, so it
    // flows through the same `update()` path every other event does rather
    // than being special-cased. With no path given, there is nothing to load
    // and no file to guess at — that goes through the same path as a real
    // load outcome, rather than being decided before the architecture sees it.
    let load_message = match cli.path {
        Some(path) => Message::CollectionLoaded {
            base_dir: base_dir(&path).to_path_buf(),
            result: Box::new(Document::from_path(&path)),
        },
        None => Message::NoCollectionPath,
    };

    let mut terminal = init_terminal()?;
    let result = run(&mut terminal, load_message);
    restore_terminal();

    result
}
