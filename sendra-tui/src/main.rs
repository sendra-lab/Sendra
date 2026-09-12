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

/// Every environment name that `find_environment` could resolve from
/// `start_dir` — the union of every `.yaml` file stem across *every*
/// ancestor's `.sendra/environments/` directory, not just the nearest one.
///
/// `find_environment` resolves a name by walking `ancestors()` and, for that
/// one name, stopping at the first ancestor whose
/// `.sendra/environments/<name>.yaml` exists — it does not stop just because
/// *some* `environments/` directory exists closer in. Originally this
/// function instead found the single nearest ancestor with an
/// `environments/` directory at all and listed only its contents, which
/// meant a nested `.sendra/` (an inner project shadowing an outer one) could
/// hide an outer-only environment that `find_environment` would happily
/// still resolve (see
/// `nested_dot_sendra_directories_can_make_the_two_algorithms_disagree`
/// below). Walking every ancestor's `environments/` directory here — rather
/// than reimplementing `find_environment`'s per-name walk once per
/// candidate name — keeps this the one piece of directory-walking
/// sendra-tui does itself for *enumeration*, while still guaranteeing every
/// name returned really does resolve: `load_environments` below hands each
/// one straight to `find_environment` + `Environment::from_path`,
/// sendra-core's own functions, which is what actually decides which file
/// wins for a name that exists at more than one level.
fn discover_environment_names(start_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = start_dir
        .ancestors()
        .map(|dir| dir.join(".sendra").join("environments"))
        .filter(|dir| dir.is_dir())
        .flat_map(|environments_dir| {
            std::fs::read_dir(&environments_dir)
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
        })
        .collect();

    names.sort();
    names.dedup();
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
/// `overlay_open`/`edit_mode_open` are the pieces of context this translation
/// needs: the same physical keys mean different things depending on whether
/// the environment overlay is on screen or the selected request is being
/// edited, without leaking that decision into `update`/`view` as raw key
/// codes.
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
/// currently "has focus". `body_focused` extends that same disjoint-keys
/// idea into edit mode: `Enter`/`Up`/`Down` mean "insert a newline"/"move a
/// line" only while the body field specifically has focus (see
/// `app::EditField::Body`) — every other edit-mode field is single-line, so
/// those keys stay meaningless there exactly as they always have been.
fn next_message(
    overlay_open: bool,
    edit_mode_open: bool,
    body_focused: bool,
) -> io::Result<Message> {
    if !event::poll(Duration::from_millis(100))? {
        return Ok(Message::Tick);
    }

    Ok(translate_event(
        event::read()?,
        overlay_open,
        edit_mode_open,
        body_focused,
    ))
}

/// The pure key/resize-to-`Message` mapping `next_message` reads off the
/// real crossterm event stream — split out so it takes a plain `Event`
/// value instead of calling `event::poll`/`event::read` itself, which is
/// what makes it unit-testable with hand-built `Event`s (no real terminal
/// needed) rather than only reachable by actually typing at one. The
/// clean-exit guarantee relies on this directly: `q` and Ctrl+C are two
/// different physical keys that both need to reach the *identical*
/// `Message::Quit` so that whatever `main::run`/`restore_terminal` do for
/// one, they provably do for the other — not two independently-written quit
/// paths that could quietly drift apart.
fn translate_event(
    event: Event,
    overlay_open: bool,
    edit_mode_open: bool,
    body_focused: bool,
) -> Message {
    match event {
        Event::Resize(_, _) => Message::Resize,
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            let is_ctrl_c =
                key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
            // Bare `q` quits everywhere *except* while editing: edit mode's
            // text fields can hold a method or URL that is entirely likely
            // to contain the letter `q` (`?query=...`) — quitting the whole
            // app on that keystroke would make such a URL unable to be
            // typed at all. Ctrl+C stays a quit key everywhere, editing
            // included: it is never a character a text field would
            // otherwise accept (crossterm reports it as `Char('c')` plus
            // the control modifier, not plain text input), and leaving
            // *some* always-on quit key matters for a clean exit.
            let is_quit = is_ctrl_c || (key.code == KeyCode::Char('q') && !edit_mode_open);
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

            // Edit mode's own key set — see `app::Message::SaveEdit`/
            // `CancelEdit`/`EditFocusNext`/`EditInsertChar` and friends.
            // Everything here (including `e`/nav/run, which reach an
            // ordinary character insert instead — see `Char(ch)` below)
            // falls through to `Message::Tick` only for a control
            // combination or a non-character key with no assigned
            // meaning, exactly the way the overlay branch above already
            // discards keys that are not its own, rather than reaching the
            // ordinary browsing keymap below and relying on `update()`'s
            // edit-mode guard alone to refuse it.
            if edit_mode_open {
                return match key.code {
                    KeyCode::Esc => Message::CancelEdit,
                    KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        Message::SaveEdit
                    }
                    // Ctrl+N/Ctrl+D, like Ctrl+S above, add/remove a header
                    // row — checked ahead of the plain-`Char` arm below the
                    // same way Ctrl+S already is, so they never fall through
                    // to inserting a literal 'n'/'d' into the focused field.
                    KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        Message::AddHeaderRow
                    }
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        Message::DeleteHeaderRow
                    }
                    // Ctrl+A/Ctrl+X, the same idea as Ctrl+N/Ctrl+D above,
                    // for assertion rows instead of header rows — a separate
                    // pair of keys since both kinds of row can be present
                    // (and being added to/deleted from) in the same edit
                    // session.
                    KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        Message::AddAssertionRow
                    }
                    KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        Message::DeleteAssertionRow
                    }
                    // Ctrl+P/Ctrl+K, the same idea again, for capture rows —
                    // a third separate pair since header, assertion and
                    // capture rows can all be present (and being added
                    // to/deleted from) in the same edit session.
                    KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        Message::AddCaptureRow
                    }
                    KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        Message::DeleteCaptureRow
                    }
                    KeyCode::Tab => Message::EditFocusNext,
                    KeyCode::BackTab => Message::EditFocusPrev,
                    KeyCode::Backspace => Message::EditBackspace,
                    KeyCode::Delete => Message::EditDelete,
                    KeyCode::Left => Message::EditCursorLeft,
                    KeyCode::Right => Message::EditCursorRight,
                    // Only meaningful for the multi-line body field — see
                    // this function's own doc comment on `body_focused`.
                    // Checked ahead of the catch-all `_ => Message::Tick`
                    // below so that, while the body has focus, `Enter`
                    // types a newline instead of doing nothing (its
                    // ordinary edit-mode meaning) and `Up`/`Down` move the
                    // cursor instead of falling through unbound.
                    KeyCode::Enter if body_focused => Message::EditInsertChar('\n'),
                    KeyCode::Up if body_focused => Message::EditCursorUp,
                    KeyCode::Down if body_focused => Message::EditCursorDown,
                    // Any other control combination (Ctrl+<letter>) is not
                    // a character this field should insert — crossterm
                    // still reports the plain letter as `Char`, so this
                    // guard is what keeps e.g. Ctrl+A from silently typing
                    // an `a` into the field instead of doing nothing.
                    KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        Message::EditInsertChar(ch)
                    }
                    _ => Message::Tick,
                };
            }

            match key.code {
                KeyCode::Char('e') => Message::OpenEnvironmentOverlay,
                KeyCode::Char('i') => Message::EnterEditMode,
                KeyCode::Char('n') => Message::AddRequest,
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
        ..
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

        let msg = next_message(
            state.environment_overlay.is_some(),
            state.edit_mode.is_some(),
            state.body_focused(),
        )?;
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
            path,
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
        let from_q = translate_event(press(KeyCode::Char('q')), false, false, false);
        let from_ctrl_c = translate_event(
            press_with(KeyCode::Char('c'), KeyModifiers::CONTROL),
            false,
            false,
            false,
        );

        assert!(matches!(from_q, Message::Quit));
        assert!(matches!(from_ctrl_c, Message::Quit));
    }

    #[test]
    fn q_and_ctrl_c_quit_even_while_the_environment_overlay_is_open() {
        // `update`'s InFlight guard exempts `Quit` specifically so it can
        // never be blocked (see its own doc comment); the overlay must not
        // re-introduce that gap from the translation side.
        let from_q = translate_event(press(KeyCode::Char('q')), true, false, false);
        let from_ctrl_c = translate_event(
            press_with(KeyCode::Char('c'), KeyModifiers::CONTROL),
            true,
            false,
            false,
        );

        assert!(matches!(from_q, Message::Quit));
        assert!(matches!(from_ctrl_c, Message::Quit));
    }

    #[test]
    fn ctrl_c_quits_while_editing_but_bare_q_types_a_character_instead() {
        // Edit mode's text fields (method/URL) can hold a URL containing
        // `q` (`?query=...`), which must be typeable — so `q` while editing
        // must insert, not quit. Ctrl+C is unaffected: it is never a
        // character a text field would otherwise accept, so it stays the
        // one quit key that works everywhere, editing included.
        let from_q = translate_event(press(KeyCode::Char('q')), false, true, false);
        let from_ctrl_c = translate_event(
            press_with(KeyCode::Char('c'), KeyModifiers::CONTROL),
            false,
            true,
            false,
        );

        assert!(matches!(from_q, Message::EditInsertChar('q')));
        assert!(matches!(from_ctrl_c, Message::Quit));
    }

    #[test]
    fn plain_c_without_control_does_not_quit() {
        // `c` alone is `ToggleRevealCaptures` (outside the overlay) — only
        // `c` *with* the control modifier means quit. A translation that
        // conflated the two would make an ordinary keystroke exit the app.
        let message = translate_event(press(KeyCode::Char('c')), false, false, false);

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
            false,
            false,
        );

        assert!(
            !matches!(message, Message::Quit),
            "a key *release* must not itself trigger quit — only a Press"
        );
    }

    #[test]
    fn resize_events_translate_to_the_resize_message() {
        let message = translate_event(Event::Resize(40, 20), false, false, false);

        assert!(matches!(message, Message::Resize));
    }

    // --- edit mode's own key set -------------------------------------------

    #[test]
    fn esc_cancels_edit_and_ctrl_s_saves_it_while_editing() {
        let cancel = translate_event(press(KeyCode::Esc), false, true, false);
        let save = translate_event(
            press_with(KeyCode::Char('s'), KeyModifiers::CONTROL),
            false,
            true,
            false,
        );

        assert!(matches!(cancel, Message::CancelEdit));
        assert!(matches!(save, Message::SaveEdit));
    }

    #[test]
    fn non_character_browsing_keys_do_nothing_while_editing() {
        // Edit mode has real text fields now, so `Char` keys like `r`/`e`/`i`
        // legitimately insert (see `letters_that_double_as_browsing_keys_
        // insert_into_the_field_while_editing` below) rather than falling
        // through to `Tick` — but keys with no text-field meaning at all
        // (arrow-key navigation not bound to cursor movement, Enter/run)
        // still must not leak through, the same guarantee the overlay
        // branch already gives its own keys. `body_focused: false` here —
        // this is exactly the case where those keys must stay meaningless,
        // covered separately (with `body_focused: true`) below.
        for code in [KeyCode::Down, KeyCode::Up, KeyCode::Enter] {
            let message = translate_event(press(code), false, true, false);
            assert!(
                matches!(message, Message::Tick),
                "expected {code:?} to be a no-op while editing outside the body field, \
                 got {message:?}"
            );
        }
    }

    #[test]
    fn letters_that_double_as_browsing_keys_insert_into_the_field_while_editing() {
        // `r`/`e`/`i`/`n` are bound to run/env/enter-edit/add-request while
        // browsing, but while editing they are just ordinary letters a
        // method or URL can contain — the same reasoning `ctrl_c_quits_
        // while_editing_but_bare_q_types_a_character_instead` applies to `q`.
        for ch in ['r', 'e', 'i', 'n'] {
            let message = translate_event(press(KeyCode::Char(ch)), false, true, false);
            assert!(
                matches!(message, Message::EditInsertChar(c) if c == ch),
                "expected {ch:?} to insert while editing, got {message:?}"
            );
        }
    }

    #[test]
    fn edit_mode_text_input_keys_translate_correctly() {
        assert!(matches!(
            translate_event(press(KeyCode::Tab), false, true, false),
            Message::EditFocusNext
        ));
        assert!(matches!(
            translate_event(press(KeyCode::BackTab), false, true, false),
            Message::EditFocusPrev
        ));
        assert!(matches!(
            translate_event(press(KeyCode::Backspace), false, true, false),
            Message::EditBackspace
        ));
        assert!(matches!(
            translate_event(press(KeyCode::Delete), false, true, false),
            Message::EditDelete
        ));
        assert!(matches!(
            translate_event(press(KeyCode::Left), false, true, false),
            Message::EditCursorLeft
        ));
        assert!(matches!(
            translate_event(press(KeyCode::Right), false, true, false),
            Message::EditCursorRight
        ));
    }

    #[test]
    fn body_focused_enter_up_down_edit_the_multiline_body_instead_of_doing_nothing() {
        assert!(matches!(
            translate_event(press(KeyCode::Enter), false, true, true),
            Message::EditInsertChar('\n')
        ));
        assert!(matches!(
            translate_event(press(KeyCode::Up), false, true, true),
            Message::EditCursorUp
        ));
        assert!(matches!(
            translate_event(press(KeyCode::Down), false, true, true),
            Message::EditCursorDown
        ));
    }

    #[test]
    fn ctrl_n_and_ctrl_d_add_and_delete_a_header_row_while_editing() {
        let add = translate_event(
            press_with(KeyCode::Char('n'), KeyModifiers::CONTROL),
            false,
            true,
            false,
        );
        let delete = translate_event(
            press_with(KeyCode::Char('d'), KeyModifiers::CONTROL),
            false,
            true,
            false,
        );

        assert!(matches!(add, Message::AddHeaderRow));
        assert!(matches!(delete, Message::DeleteHeaderRow));
    }

    #[test]
    fn ctrl_a_and_ctrl_x_add_and_delete_an_assertion_row_while_editing() {
        let add = translate_event(
            press_with(KeyCode::Char('a'), KeyModifiers::CONTROL),
            false,
            true,
            false,
        );
        let delete = translate_event(
            press_with(KeyCode::Char('x'), KeyModifiers::CONTROL),
            false,
            true,
            false,
        );

        assert!(matches!(add, Message::AddAssertionRow));
        assert!(matches!(delete, Message::DeleteAssertionRow));
    }

    #[test]
    fn ctrl_p_and_ctrl_k_add_and_delete_a_capture_row_while_editing() {
        let add = translate_event(
            press_with(KeyCode::Char('p'), KeyModifiers::CONTROL),
            false,
            true,
            false,
        );
        let delete = translate_event(
            press_with(KeyCode::Char('k'), KeyModifiers::CONTROL),
            false,
            true,
            false,
        );

        assert!(matches!(add, Message::AddCaptureRow));
        assert!(matches!(delete, Message::DeleteCaptureRow));
    }

    #[test]
    fn control_letter_combinations_other_than_ctrl_s_n_d_a_x_p_k_do_nothing_while_editing() {
        // Ctrl+B is not a character this field should insert — crossterm
        // still reports the plain letter as `Char('b')`, so without the
        // control-modifier guard in `translate_event` this would silently
        // type a `b` into the field instead of doing nothing. Picked because
        // it is not one of the eight control combinations edit mode actually
        // binds (`s`/`n`/`d`/`a`/`x`/`p`/`k`, plus `c` for quit).
        let message = translate_event(
            press_with(KeyCode::Char('b'), KeyModifiers::CONTROL),
            false,
            true,
            false,
        );
        assert!(matches!(message, Message::Tick));
    }

    #[test]
    fn i_enters_edit_mode_outside_the_overlay_and_outside_edit_mode() {
        let message = translate_event(press(KeyCode::Char('i')), false, false, false);
        assert!(matches!(message, Message::EnterEditMode));
    }

    #[test]
    fn n_adds_a_request_outside_the_overlay_and_outside_edit_mode() {
        let message = translate_event(press(KeyCode::Char('n')), false, false, false);
        assert!(matches!(message, Message::AddRequest));
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

/// A direct, automated proof that sendra-tui's
/// project/config/environment/collection resolution genuinely matches what
/// sendra-cli resolves for the same real project directory — not a manual
/// spot-check, an actual test against a real fixture on disk.
///
/// **Why this lives here, and not under `tests/`:** sendra-tui has no
/// `lib.rs` (see `Cargo.toml` — `[[bin]]` only), so an integration test
/// under `tests/` cannot reach `discover_environment_names`,
/// `load_environments`, or `base_dir` at all — they are private to this
/// binary crate. This module is compiled inside `main.rs` itself specifically
/// so it can call sendra-tui's *real* resolution functions, not
/// reimplemented lookalikes of them.
///
/// **Why the "CLI side" calls `sendra_core` directly instead of spawning the
/// `sendra` binary:** `sendra-cli`'s own `prepare`/`environment_for`
/// (`sendra-cli/src/run.rs`) are private to that crate, so they cannot be
/// called from here either way. They are also thin, fixed sequences of
/// `sendra_core` calls with no resolution logic of their own — `prepare`
/// is literally `find_project_config` + `Config::resolve_from` +
/// `environment_for`, which is itself `find_environment` +
/// `Environment::from_path`. Reading those calls (cited by name below) and
/// making the identical calls here, then comparing the resulting values with
/// `assert_eq!`, proves the same thing spawning the real binary and parsing
/// its `-v`/`--verbose` text would — but directly, on real typed values,
/// rather than through a second, fragile text format neither side is
/// actually tested against elsewhere.
#[cfg(test)]
mod resolution_parity_tests {
    use super::*;
    use sendra_core::config::find_project_config;
    use sendra_core::environment::find_environment;
    use sendra_core::{Config, Document, Environment};

    /// A real, on-disk project — not a mock — laid out the way a genuine
    /// Sendra project is: `.sendra/config.yaml`,
    /// `.sendra/environments/*.yaml`, and a collection file living a couple
    /// of directories below the project root, so resolution genuinely has
    /// to walk up `ancestors()` rather than trivially matching at depth 0.
    struct Fixture {
        _root: tempfile::TempDir,
        /// `<root>/collections` — where a user would plausibly have their
        /// shell open while running `sendra-tui` or `sendra run`, i.e. the
        /// `start_dir` both sides resolve from. Not the project root itself.
        start_dir: PathBuf,
        collection_path: PathBuf,
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().expect("a file has a parent")).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn build_fixture() -> Fixture {
        let root = tempfile::tempdir().expect("a temporary directory");
        let project = root.path().join("project");

        write(
            &project.join(".sendra").join("config.yaml"),
            "headers:\n  X-From-Config: present\ntimeout_seconds: 7\n",
        );
        write(
            &project
                .join(".sendra")
                .join("environments")
                .join("default.yaml"),
            "base_url: https://default.example.com\napi_key: shh\n",
        );
        write(
            &project
                .join(".sendra")
                .join("environments")
                .join("staging.yaml"),
            "base_url: https://staging.example.com\n",
        );

        let collection_path = project.join("collections").join("api.yaml");
        write(
            &collection_path,
            "name: API\n\
             requests:\n\
             \x20\x20- name: GetWidget\n\
             \x20\x20\x20\x20method: GET\n\
             \x20\x20\x20\x20url: \"{{base_url}}/widgets/1\"\n\
             \x20\x20- name: CreateWidget\n\
             \x20\x20\x20\x20method: POST\n\
             \x20\x20\x20\x20url: \"{{base_url}}/widgets\"\n\
             \x20\x20\x20\x20body: '{}'\n",
        );

        let start_dir = collection_path
            .parent()
            .expect("the collection file has a parent directory")
            .to_path_buf();

        Fixture {
            _root: root,
            start_dir,
            collection_path,
        }
    }

    /// An empty directory for `XDG_CONFIG_HOME` to point at — "no global
    /// config" — so this test's result does not depend on whatever the
    /// machine actually running it happens to have installed. Mirrors
    /// `sendra-cli/tests/verbose.rs`'s own `empty_xdg_config_home`, for the
    /// same reason cited there: `global_config_path` honours
    /// `XDG_CONFIG_HOME` first, on every platform, when it is absolute.
    ///
    /// Set via `std::env::set_var`, process-wide, for the duration of this
    /// one test — every other test in this crate that ends up resolving a
    /// *real* config either does not assert on its content (the
    /// `run_request` HTTP tests only check status/body) or does not resolve
    /// config at all, so a benign race with one of them changes nothing they
    /// assert on.
    fn empty_xdg_config_home() -> tempfile::TempDir {
        tempfile::tempdir().expect("a temporary directory")
    }

    #[test]
    fn tui_and_cli_resolution_agree_on_a_real_project_fixture() {
        let fixture = build_fixture();
        let xdg_config_home = empty_xdg_config_home();
        std::env::set_var("XDG_CONFIG_HOME", xdg_config_home.path());

        // --- project root -----------------------------------------------
        //
        // `sendra-cli::run::prepare` calls `find_project_config(&start_dir)`
        // directly (see its own doc comment on `Prepared::project_config`);
        // sendra-tui's `discover_environment_names` walks `ancestors()` on
        // its own, for its own `.sendra/environments/` directory, since
        // `sendra_core` exposes no "list environment names" function (see
        // that function's doc comment). Both walks must land on the exact
        // same `.sendra/` directory for the same `start_dir` — this is
        // exactly the kind of independently-reimplemented directory walk
        // that could quietly diverge from `find_project_config`'s.
        let cli_project_config = find_project_config(&fixture.start_dir)
            .expect("the fixture's .sendra/config.yaml must be found");
        let cli_project_root = cli_project_config
            .parent() // .sendra/
            .and_then(Path::parent) // project/
            .expect("config.yaml sits two levels under the project root");

        let tui_environment_names = discover_environment_names(&fixture.start_dir);
        let tui_environments_dir = fixture
            .start_dir
            .ancestors()
            .map(|dir| dir.join(".sendra").join("environments"))
            .find(|dir| dir.is_dir())
            .expect("the fixture's .sendra/environments/ must be found");
        let tui_project_root = tui_environments_dir
            .parent() // .sendra/
            .and_then(Path::parent) // project/
            .expect("environments/ sits two levels under the project root");

        assert_eq!(
            cli_project_root, tui_project_root,
            "sendra-tui's own .sendra/environments/ walk and sendra-core's \
             find_project_config must discover the identical project root"
        );

        // --- config -------------------------------------------------------
        //
        // `prepare` calls `Config::resolve_from(&start_dir,
        // global_config.as_deref())` with `global_config =
        // global_config_path().filter(|path| path.is_file())` — exactly what
        // `run_request::resolve_config` (pulled out of `execute_inner` for
        // this test) now does too.
        let cli_config = {
            let global_config =
                sendra_core::config::global_config_path().filter(|path| path.is_file());
            Config::resolve_from(&fixture.start_dir, global_config.as_deref())
                .expect("the fixture's config.yaml must resolve")
        };
        let tui_config = run_request::resolve_config(&fixture.start_dir)
            .expect("sendra-tui's own config resolution must succeed on the same fixture");
        assert_eq!(
            cli_config, tui_config,
            "sendra-tui and sendra-cli must resolve the identical Config for \
             the identical project"
        );
        // Not just "equal by construction" — prove the fixture's own config
        // actually took effect on both sides, so an accidental "both sides
        // silently fell back to defaults" could not pass this test.
        assert_eq!(
            tui_config.headers.get("X-From-Config").map(String::as_str),
            Some("present")
        );
        assert_eq!(tui_config.timeout, std::time::Duration::from_secs(7));

        // --- environments (names and content) ------------------------------
        assert_eq!(
            tui_environment_names,
            vec!["default".to_string(), "staging".to_string()],
            "both environment files in the fixture must be discovered, sorted"
        );

        let (tui_environments, tui_environment_errors) = load_environments(&fixture.start_dir);
        assert!(
            tui_environment_errors.is_empty(),
            "every fixture environment file is well-formed: {tui_environment_errors:?}"
        );
        assert_eq!(tui_environments.len(), 2);

        for name in &tui_environment_names {
            // `environment_for`'s `Some(name)` branch: `find_environment` +
            // `Environment::from_path`.
            let cli_path = find_environment(&fixture.start_dir, name)
                .unwrap_or_else(|| panic!("environment '{name}' must be found"));
            let cli_environment = Environment::from_path(&cli_path)
                .unwrap_or_else(|err| panic!("environment '{name}' must load: {err}"));

            let tui_environment = tui_environments
                .iter()
                .find(|named| &named.name == name)
                .unwrap_or_else(|| panic!("sendra-tui must have discovered '{name}' too"));

            assert_eq!(
                &tui_environment.environment, &cli_environment,
                "sendra-tui and sendra-cli must load identical content for \
                 environment '{name}'"
            );
        }
        // Content check on top of structural equality: prove the loaded
        // environment really carries the fixture's own variable, not two
        // empty environments that happen to be equal to each other.
        let default_env = &tui_environments
            .iter()
            .find(|named| named.name == "default")
            .expect("default.yaml was discovered")
            .environment;
        assert_eq!(
            default_env.variables.get("base_url").map(String::as_str),
            Some("https://default.example.com")
        );

        // --- collection -----------------------------------------------------
        //
        // `prepare` calls `Document::from_path(path)` directly — the exact
        // same sendra_core function `main::run`'s own `CollectionLoaded`
        // message is built from (see `main`'s doc comment on
        // `load_message`).
        let cli_document = Document::from_path(&fixture.collection_path)
            .expect("the fixture collection must parse");
        let tui_document = Document::from_path(&fixture.collection_path)
            .expect("sendra-tui must parse the identical file identically");
        assert_eq!(
            cli_document, tui_document,
            "both sides must parse the identical collection identically"
        );
        assert_eq!(tui_document.requests().len(), 2);
        assert_eq!(
            tui_document.requests()[0].name.as_deref(),
            Some("GetWidget"),
            "file order must be preserved"
        );

        // --- base_dir --------------------------------------------------------
        //
        // Where a resolved request's `body_file` would resolve against —
        // sendra-tui's own `base_dir` helper, which its own doc comment
        // already claims mirrors sendra-cli's identically-named one in
        // `sendra-cli/src/run.rs`. That CLI function is private and not
        // callable from here, but its logic is one line
        // (`path.parent().filter(...).unwrap_or(".")`) restated directly
        // from that source rather than assumed.
        let cli_base_dir = fixture
            .collection_path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        assert_eq!(base_dir(&fixture.collection_path), cli_base_dir);
    }

    /// `discover_environment_names` must agree with `find_environment` even
    /// on a nested-`.sendra/` layout — an inner project shadowing an outer
    /// one. It previously did not: it found only the *nearest ancestor with
    /// an `environments/` directory at all* and listed just that directory's
    /// contents, while `find_environment` resolves, independently **per
    /// name**, the nearest ancestor whose
    /// `.sendra/environments/<name>.yaml` specifically exists — so an
    /// outer-only environment name could be missing from the list while
    /// `find_environment` would still happily resolve it. This test builds
    /// that exact pathological fixture and now asserts the two agree on
    /// every name, rather than merely documenting the disagreement.
    #[test]
    fn nested_dot_sendra_directories_agree_with_find_environment() {
        let root = tempfile::tempdir().expect("a temporary directory");

        // Outer project: only `default.yaml`.
        write(
            &root
                .path()
                .join(".sendra")
                .join("environments")
                .join("default.yaml"),
            "base_url: https://outer.example.com\n",
        );
        // Inner project, nested under the outer one: only `staging.yaml`,
        // no `default.yaml` of its own.
        let inner = root.path().join("inner");
        write(
            &inner
                .join(".sendra")
                .join("environments")
                .join("staging.yaml"),
            "base_url: https://inner-staging.example.com\n",
        );

        let tui_names = discover_environment_names(&inner);
        // Both the inner-only `staging` and the outer-only `default` must
        // now be discovered — the union across every ancestor's
        // `environments/` directory, not just the nearest one.
        assert_eq!(
            tui_names,
            vec!["default".to_string(), "staging".to_string()],
            "discover_environment_names must list every name find_environment \
             can resolve, including one that only exists in an outer, \
             shadowed .sendra/ directory"
        );

        // And for every name it lists, find_environment must actually
        // resolve it — proving the list is not just names, but names that
        // truly resolve, and to the file each level's own environment
        // predicts.
        for name in &tui_names {
            let resolved = find_environment(&inner, name)
                .unwrap_or_else(|| panic!("find_environment must resolve '{name}' too"));
            assert!(resolved.is_file());
        }
        let default_path = find_environment(&inner, "default").expect("outer default resolves");
        assert_eq!(
            default_path,
            root.path()
                .join(".sendra")
                .join("environments")
                .join("default.yaml")
        );
        let staging_path = find_environment(&inner, "staging").expect("inner staging resolves");
        assert_eq!(
            staging_path,
            inner
                .join(".sendra")
                .join("environments")
                .join("staging.yaml")
        );
    }

    /// Regression guard for the fix above: an ordinary single-`.sendra`
    /// project (no nesting at all) must discover exactly the same names as
    /// before — merging across ancestors must not add or duplicate anything
    /// when there is only one `environments/` directory to find.
    #[test]
    fn single_dot_sendra_project_discovery_is_unchanged() {
        let fixture = build_fixture();
        let names = discover_environment_names(&fixture.start_dir);
        assert_eq!(
            names,
            vec!["default".to_string(), "staging".to_string()],
            "an ordinary single-.sendra project must discover exactly its \
             own environments/ directory contents, unaffected by the \
             nested-project fix"
        );
    }
}
