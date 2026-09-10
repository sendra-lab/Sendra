mod app;
mod run_request;

use std::io::{self, Stdout};
use std::panic;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use sendra_core::environment::find_environment;
use sendra_core::{Document, Environment, SendraError};

use app::{
    active_environment, update, view, AppState, LoadState, Message, NamedEnvironment, RunState,
};

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

/// Every environment name found in the nearest `.sendra/environments/`
/// walking up from `start_dir` — the same `ancestors()` walk
/// `find_project_config`/`find_environment` use internally to locate
/// `.sendra/`, applied here for enumeration rather than a single lookup by
/// name. sendra-core has a function to resolve *one named* environment
/// (`find_environment`) but none to list what names exist, so this is the
/// one piece of directory-walking sendra-tui does itself; loading each name
/// once found is handed straight to `find_environment` +
/// `Environment::from_path` below, sendra-core's own functions, not
/// reimplemented parsing.
fn discover_environment_names(start_dir: &Path) -> Vec<String> {
    let Some(environments_dir) = start_dir
        .ancestors()
        .map(|dir| dir.join(".sendra").join("environments"))
        .find(|dir| dir.is_dir())
    else {
        return Vec::new();
    };

    let mut names: Vec<String> = std::fs::read_dir(&environments_dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("yaml") {
                return None;
            }
            path.file_stem()?.to_str().map(str::to_string)
        })
        .collect();

    names.sort();
    names
}

/// Loads every discovered environment via `find_environment` +
/// `Environment::from_path` — the exact functions sendra-cli's own
/// `environment_for` uses for a single named environment (see
/// `sendra-cli/src/run.rs`) — skipping (rather than failing the whole list
/// over) any one file that turns out unreadable, but **not silently**: the
/// name and the real `SendraError` `Environment::from_path` returned are
/// collected into the second half of the return value rather than dropped,
/// so a malformed or unreadable environment file becomes a visible error in
/// the overlay (`app::render_error`) instead of a file that just never
/// appears in the list with nothing to say why.
///
/// A file `find_environment` can no longer find at all (deleted between
/// `discover_environment_names`'s directory listing and this lookup) is the
/// one case still skipped with no error: there is no `SendraError` to show
/// for a path that is simply gone, and the far likelier explanation — it was
/// never there to begin with — is exactly what `discover_environment_names`
/// already ruled out by listing the directory itself.
fn load_environments(start_dir: &Path) -> (Vec<NamedEnvironment>, Vec<(String, SendraError)>) {
    let mut environments = Vec::new();
    let mut errors = Vec::new();

    for name in discover_environment_names(start_dir) {
        let Some(path) = find_environment(start_dir, &name) else {
            continue;
        };
        match Environment::from_path(path) {
            Ok(environment) => environments.push(NamedEnvironment { name, environment }),
            Err(error) => errors.push((name, error)),
        }
    }

    (environments, errors)
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
/// `overlay_open` is the one piece of context this translation needs: the
/// same physical keys mean different things depending on whether the
/// environment overlay is on screen, without leaking that decision into
/// `update`/`view` as raw key codes.
fn next_message(overlay_open: bool) -> io::Result<Message> {
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

            if overlay_open {
                return Ok(match key.code {
                    KeyCode::Esc => Message::CloseEnvironmentOverlay,
                    KeyCode::Enter => Message::ConfirmEnvironmentSelection,
                    KeyCode::Down | KeyCode::Char('j') => Message::SelectNext,
                    KeyCode::Up | KeyCode::Char('k') => Message::SelectPrevious,
                    _ => Message::Tick,
                });
            }

            match key.code {
                KeyCode::Char('e') => Ok(Message::OpenEnvironmentOverlay),
                KeyCode::Down | KeyCode::Char('j') => Ok(Message::SelectNext),
                KeyCode::Up | KeyCode::Char('k') => Ok(Message::SelectPrevious),
                KeyCode::Enter | KeyCode::Char('r') => Ok(Message::RunRequested),
                KeyCode::PageDown => Ok(Message::ScrollResponseDown),
                KeyCode::PageUp => Ok(Message::ScrollResponseUp),
                KeyCode::Char('c') => Ok(Message::ToggleRevealCaptures),
                _ => Ok(Message::Tick),
            }
        }
        _ => Ok(Message::Tick),
    }
}

/// Extracts what a run needs — the selected request, the active environment
/// (an empty one when none is active, matching the detail pane's own
/// fallback), and the collection's `base_dir` — right after `update()` has
/// just moved `run_state` to `InFlight` for a `Message::RunRequested`.
///
/// A plain function rather than inlined at the one call site so the
/// `LoadState::Loaded` destructure — the same shape `render_detail_pane`
/// already matches on — has a name, and so a future second call site (there
/// is none today) would not have to duplicate it.
fn selected_run(state: &AppState) -> Option<(sendra_core::Request, Environment, PathBuf)> {
    let LoadState::Loaded {
        document,
        selected,
        base_dir,
    } = &state.load_state
    else {
        return None;
    };
    let request = document.requests().get(*selected)?.clone();
    let environment = active_environment(state)
        .map(|named| named.environment.clone())
        .unwrap_or_default();
    Some((request, environment, base_dir.clone()))
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    load_message: Message,
    environments: Vec<NamedEnvironment>,
    environment_errors: Vec<(String, SendraError)>,
) -> io::Result<()> {
    let mut state = AppState::default();
    update(&mut state, load_message);
    update(
        &mut state,
        Message::EnvironmentsLoaded {
            environments,
            errors: environment_errors,
        },
    );

    // Carries `Message::RunCompleted` back from whichever thread
    // `run_request::spawn` put the send on into this loop, which is the only
    // place allowed to call `update()` — the same rule every other message
    // source (crossterm events, the startup loads above) already follows.
    let (run_tx, run_rx) = mpsc::channel::<Message>();

    loop {
        terminal.draw(|frame| view(&state, frame))?;

        // Drained before the next blocking key poll below, so a run that
        // finished while the terminal was waiting for a keypress is reflected
        // on the very next frame instead of waiting for the user to press
        // something first.
        while let Ok(msg) = run_rx.try_recv() {
            update(&mut state, msg);
        }

        let msg = next_message(state.environment_overlay.is_some())?;
        let is_run_request = matches!(msg, Message::RunRequested);
        let was_already_running = matches!(state.run_state, RunState::InFlight);
        update(&mut state, msg);

        // Spawn exactly when this message is the one that just moved
        // `run_state` from anything else to `InFlight` — `was_already_running`
        // rules out a `RunRequested` that `update()` refused because a run
        // was already in flight, so this never spawns a second send on top of
        // one still running.
        if is_run_request && !was_already_running {
            if let (RunState::InFlight, Some((request, environment, base_dir))) =
                (&state.run_state, selected_run(&state))
            {
                let tx = run_tx.clone();
                run_request::spawn(request, environment, base_dir, move |result| {
                    let _ = tx.send(Message::RunCompleted(result));
                });
            }
        }

        if state.should_quit {
            return Ok(());
        }
    }
}

fn main() -> io::Result<()> {
    install_panic_hook();

    let cli = Cli::parse();
    let start_dir = std::env::current_dir()?;

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
    let (environments, environment_errors) = load_environments(&start_dir);

    let mut terminal = init_terminal()?;
    let result = run(
        &mut terminal,
        load_message,
        environments,
        environment_errors,
    );
    restore_terminal();

    result
}
