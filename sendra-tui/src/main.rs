mod app;
mod run_request;

use std::io::{self, IsTerminal, Stdout};
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

/// Restores the terminal before the default hook prints the panic message —
/// but **only** for a panic on the thread that actually owns the terminal.
/// `panic::set_hook` installs one hook for the whole process: every thread
/// that panics runs it, including the run-request thread `run_request::spawn`
/// puts each HTTP send on. That thread's panics are caught with
/// `catch_unwind` and turned into a failed run (see that function's doc
/// comment) rather than ever reaching here as an unwind, but the hook itself
/// still fires at the moment of the panic regardless of whether something
/// upstack goes on to catch it — `catch_unwind` does not suppress hook
/// invocation. Without this thread check, a bug in the request pipeline
/// would call `restore_terminal()` (disabling raw mode, leaving the
/// alternate screen) out from under a main loop that is still very much
/// alive and about to draw its next frame — corrupting a session that never
/// actually crashed, instead of the run simply showing up as a failed
/// request in the response panel like any other error.
/// Whether a panic on `panicking_thread` should restore the terminal, given
/// `install_panic_hook` was itself called from `main_thread` — pulled out of
/// the hook closure as a pure, two-`ThreadId` comparison so this gating
/// decision is unit-testable directly (construct a background thread, take
/// its real `ThreadId`, assert the predicate) rather than only observable by
/// actually panicking a real terminal session.
fn panic_should_restore_terminal(
    panicking_thread: std::thread::ThreadId,
    main_thread: std::thread::ThreadId,
) -> bool {
    panicking_thread == main_thread
}

fn install_panic_hook() {
    let default_hook = panic::take_hook();
    let main_thread_id = std::thread::current().id();
    panic::set_hook(Box::new(move |info| {
        if panic_should_restore_terminal(std::thread::current().id(), main_thread_id) {
            restore_terminal();
        }
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
///
/// `Event::Resize` gets its own explicit arm to `Message::Resize` rather than
/// falling into the catch-all `Message::Tick` below — see that variant's doc
/// comment in `app.rs` for why no further action is needed here: ratatui's
/// `Terminal::draw` (called at the top of every loop iteration in `run`)
/// already autoresizes against the backend's real size before rendering, so
/// simply returning any message — waking the loop for its next `draw` call —
/// is what actually re-layouts the screen. Verified against
/// `ratatui_core::terminal::render`/`resize` (ratatui 0.30 / ratatui-core
/// 0.1.2) rather than assumed.
///
/// `Down`/`Up`/`j`/`k` are bound to `SelectNext`/`SelectPrevious` outside the
/// overlay branch below and to nothing else — see the doc comment on
/// [`app::Message::ScrollResponseDown`] for why the response panel's own
/// scroll deliberately lives on a disjoint set of keys (`PageUp`/`PageDown`/
/// `Home`/`End`) instead of overloading the same arrows based on which pane
/// currently "has focus".
fn next_message(overlay_open: bool) -> io::Result<Message> {
    if !event::poll(Duration::from_millis(100))? {
        return Ok(Message::Tick);
    }

    Ok(translate_event(event::read()?, overlay_open))
}

/// The pure key/resize-to-`Message` mapping `next_message` reads off the
/// real crossterm event stream — split out so it takes a plain `Event`
/// value instead of calling `event::poll`/`event::read` itself, which is
/// what makes it unit-testable with hand-built `Event`s (no real terminal
/// needed) rather than only reachable by actually typing at one. Issue 13's
/// clean-exit audit relies on this directly: `q` and Ctrl+C are two
/// different physical keys that both need to reach the *identical*
/// `Message::Quit` so that whatever `main::run`/`restore_terminal` do for
/// one, they provably do for the other — not two independently-written quit
/// paths that could quietly drift apart.
fn translate_event(event: Event, overlay_open: bool) -> Message {
    match event {
        Event::Resize(_, _) => Message::Resize,
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            let is_quit = key.code == KeyCode::Char('q')
                || (key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL));
            if is_quit {
                return Message::Quit;
            }

            if overlay_open {
                return match key.code {
                    KeyCode::Esc => Message::CloseEnvironmentOverlay,
                    KeyCode::Enter => Message::ConfirmEnvironmentSelection,
                    KeyCode::Down | KeyCode::Char('j') => Message::SelectNext,
                    KeyCode::Up | KeyCode::Char('k') => Message::SelectPrevious,
                    _ => Message::Tick,
                };
            }

            match key.code {
                KeyCode::Char('e') => Message::OpenEnvironmentOverlay,
                KeyCode::Down | KeyCode::Char('j') => Message::SelectNext,
                KeyCode::Up | KeyCode::Char('k') => Message::SelectPrevious,
                KeyCode::Enter | KeyCode::Char('r') => Message::RunRequested,
                KeyCode::PageDown => Message::ScrollResponseDown,
                KeyCode::PageUp => Message::ScrollResponseUp,
                KeyCode::Home => Message::ScrollResponseTop,
                KeyCode::End => Message::ScrollResponseBottom,
                KeyCode::Char('c') => Message::ToggleRevealCaptures,
                _ => Message::Tick,
            }
        }
        _ => Message::Tick,
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

/// A clear, up-front refusal when stdout is not a real terminal (piped to a
/// file or another process, e.g. `sendra-tui > output.txt` or
/// `sendra-tui | cat`), checked before `init_terminal` ever calls
/// `enable_raw_mode`/`EnterAlternateScreen`. Without this, those calls either
/// fail with a raw crossterm/OS error whose message says nothing about *why*
/// (no such thing as raw mode on a pipe), or — worse, depending on platform —
/// succeed on a redirected stdout and start writing ANSI escape sequences
/// into whatever file or pipe is on the other end, silently producing
/// garbage instead of a TUI. Checking `stdout` specifically (not `stdin`)
/// matches how this actually gets triggered in practice: piping the *output*
/// of an interactive full-screen program somewhere it cannot be interactive.
///
/// Printed directly with `eprintln!` and exited here, rather than returned
/// as an `io::Error` for `main`'s `?` to propagate: `main`'s `io::Result`
/// return type prints an error via `Debug` (`Error: Custom { kind: ..., .. }`),
/// which is exactly the confusing, implementation-flavored message this
/// check exists to avoid — the point is one plain, human-readable line.
fn require_interactive_stdout() {
    if io::stdout().is_terminal() {
        return;
    }
    eprintln!(
        "sendra-tui requires an interactive terminal on stdout; it looks like \
         stdout has been redirected or piped. Run it directly in a terminal."
    );
    std::process::exit(1);
}

fn main() -> io::Result<()> {
    require_interactive_stdout();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn press(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn press_with(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    // --- clean-exit audit: q and Ctrl+C must be provably identical -------

    #[test]
    fn q_and_ctrl_c_both_translate_to_the_identical_quit_message() {
        let from_q = translate_event(press(KeyCode::Char('q')), false);
        let from_ctrl_c =
            translate_event(press_with(KeyCode::Char('c'), KeyModifiers::CONTROL), false);

        assert!(matches!(from_q, Message::Quit));
        assert!(matches!(from_ctrl_c, Message::Quit));
    }

    #[test]
    fn q_and_ctrl_c_quit_even_while_the_environment_overlay_is_open() {
        // `update`'s InFlight guard exempts `Quit` specifically so it can
        // never be blocked (see its own doc comment); the overlay must not
        // re-introduce that gap from the translation side.
        let from_q = translate_event(press(KeyCode::Char('q')), true);
        let from_ctrl_c =
            translate_event(press_with(KeyCode::Char('c'), KeyModifiers::CONTROL), true);

        assert!(matches!(from_q, Message::Quit));
        assert!(matches!(from_ctrl_c, Message::Quit));
    }

    #[test]
    fn plain_c_without_control_does_not_quit() {
        // `c` alone is `ToggleRevealCaptures` (outside the overlay) — only
        // `c` *with* the control modifier means quit. A translation that
        // conflated the two would make an ordinary keystroke exit the app.
        let message = translate_event(press(KeyCode::Char('c')), false);

        assert!(!matches!(message, Message::Quit));
    }

    #[test]
    fn a_key_release_event_is_not_treated_as_a_press() {
        let message = translate_event(
            Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )),
            false,
        );

        assert!(
            !matches!(message, Message::Quit),
            "a key *release* must not itself trigger quit — only a Press"
        );
    }

    #[test]
    fn resize_events_translate_to_the_resize_message() {
        let message = translate_event(Event::Resize(40, 20), false);

        assert!(matches!(message, Message::Resize));
    }

    // --- clean-exit audit: panic-hook thread gating -----------------------

    #[test]
    fn panic_hook_restores_the_terminal_only_for_the_thread_that_installed_it() {
        let main_thread = std::thread::current().id();
        assert!(
            panic_should_restore_terminal(main_thread, main_thread),
            "a panic on the same thread the hook was installed from (the real \
             main thread, in production) must restore the terminal"
        );

        let background_thread = std::thread::spawn(|| std::thread::current().id())
            .join()
            .expect("the helper thread must not itself panic");
        assert_ne!(
            background_thread, main_thread,
            "the test setup must actually produce two distinct thread ids"
        );
        assert!(
            !panic_should_restore_terminal(background_thread, main_thread),
            "a panic on any other thread — like run_request::spawn's background \
             run thread — must NOT restore the terminal out from under a main \
             loop that is still alive and running"
        );
    }
}
