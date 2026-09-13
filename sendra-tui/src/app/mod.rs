//! sendra-tui's Elm-style architecture, split into its three natural layers:
//!
//! - [`state`]: `AppState` and everything it is built from — the model.
//! - [`update`]: `update()`, the one function allowed to mutate `AppState`.
//! - [`view`]: `view()` and every `render_*`/`format_*` function under it.
//!
//! Plus four small cross-cutting modules neither layer above should itself
//! contain: [`preview`], the sendra-core call sequences (substitution,
//! auth/query/body resolution) `view`'s read-only browsing preview and
//! auth-masking build on; [`theme`], the one palette every color/style
//! choice `view`'s render functions make comes from — see that module's own
//! doc comment for why it lives here rather than inside `view/` itself;
//! [`discovery`], finding candidate collection files near the current
//! directory for the welcome screen's picker; and [`logo`], decoding the
//! embedded logo PNG into colored half-block text that same screen draws.
//!
//! This is a pure reorganization of what used to be one `app.rs` file along
//! boundaries the module already had (state/model, reducer, rendering) —
//! no behavior changed, only where the code lives. `main.rs` is unaffected:
//! it still reaches everything it needs through `crate::app::{...}`, exactly
//! the names re-exported below.

mod discovery;
mod logo;
mod preview;
mod state;
mod theme;
mod update;
mod view;

pub use state::{AppState, LoadState, Message, NamedEnvironment, RunState};
pub(crate) use state::{AuthEdit, OAuthLoginState};
pub use update::update;
pub use view::{active_environment, view};

/// Test-only fixtures shared by `state`, `update` and `view`'s own test
/// modules. Before this split, one flat `mod tests` at the bottom of
/// `app.rs` could just define `loaded_state`/`named_environment`/
/// `sample_outcome`/etc. once and have every test in the file see them;
/// splitting into three files means the handful of fixtures actually used
/// on both sides of a split (a reducer test setting up a loaded collection,
/// a view test rendering that same setup) need one shared home rather than
/// three copies quietly drifting apart. Fixtures used by only one of the
/// three modules (e.g. `view`'s `response_with`/`buffer_to_string`) stay
/// local to that module's own test code instead of being pulled in here.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;

    use sendra_core::{
        AssertionReport, CaptureReport, Document, Environment, Response, SendraError,
    };

    use crate::run_request::RunOutcome;

    use super::state::{AppState, CollectionSession, LoadState, Message, NamedEnvironment};
    use super::update::update;
    use super::view::view;

    pub(crate) const VALID_COLLECTION: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
  - name: Two
    method: GET
    url: https://example.com/two
";

    pub(crate) const THREE_REQUEST_COLLECTION: &str = "\
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

    pub(crate) const MALFORMED_YAML: &str = "requests: [this is not valid yaml";

    /// A collection of `n` requests named `Request0`..`Request{n-1}` — used
    /// to build a collection taller than a small test terminal, to exercise
    /// the request list's own scrolling.
    pub(crate) fn many_request_collection(n: usize) -> String {
        let mut yaml = String::from("name: test\nrequests:\n");
        for i in 0..n {
            yaml.push_str(&format!(
                "  - name: Request{i}\n    method: GET\n    url: https://example.com/{i}\n"
            ));
        }
        yaml
    }

    /// Where `loaded_state` points `LoadState::Loaded::path`/`base_dir` at —
    /// one real, writable directory shared by every test in this binary
    /// (`OnceLock` rather than a fresh `tempfile::tempdir()` per call), so
    /// `Message::SaveEdit`'s real `Document::save_to_path` has somewhere
    /// genuine to write to. `TempDir::keep()` deliberately leaks this one
    /// directory rather than deleting it when the `TempDir` guard would
    /// otherwise drop: there is nowhere in this fixture's signature to hold
    /// that guard alive for the rest of the test binary's run, and one
    /// leaked scratch directory per test binary (not one per `loaded_state`
    /// call — hundreds of tests call it) is an acceptable, bounded cost for
    /// tests that need `SaveEdit` to genuinely reach disk.
    fn save_test_scratch_dir() -> &'static std::path::Path {
        use std::sync::OnceLock;
        static DIR: OnceLock<PathBuf> = OnceLock::new();
        DIR.get_or_init(|| {
            tempfile::tempdir()
                .expect("a scratch directory for save-to-disk tests")
                .keep()
        })
    }

    pub(crate) fn loaded_state(yaml: &str) -> AppState {
        use std::sync::atomic::{AtomicU64, Ordering};

        let mut state = AppState::default();
        let document = Document::from_yaml_str(yaml).expect("valid test YAML");

        // Each call gets its own file name in the shared scratch directory —
        // never written ahead of time (`Document::save_to_path` creates it
        // via `rename` on first save, the same as it would for a brand-new
        // collection file), just a real path a save can actually land on.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = save_test_scratch_dir().join(format!("collection-{id}.yaml"));

        update(
            &mut state,
            Message::CollectionLoaded {
                base_dir: save_test_scratch_dir().to_path_buf(),
                path,
                result: Box::new(Ok(document)),
            },
        );
        state
    }

    pub(crate) fn named_environment(name: &str, variables: &[(&str, &str)]) -> NamedEnvironment {
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

    pub(crate) fn state_with_environments(names: &[&str]) -> AppState {
        let mut state = AppState::default();
        state.environments = names
            .iter()
            .map(|name| named_environment(name, &[]))
            .collect();
        state
    }

    pub(crate) fn selected(state: &AppState) -> usize {
        match state.load_state {
            LoadState::Loaded { selected, .. } => selected,
            ref other => panic!("expected LoadState::Loaded, got {other:?}"),
        }
    }

    pub(crate) fn sample_response(status: u16) -> Response {
        Response {
            status,
            status_text: "OK".to_string(),
            headers: Vec::new(),
            body: String::new(),
            elapsed: std::time::Duration::from_millis(1),
            redirects: Vec::new(),
        }
    }

    /// A successful `RunOutcome` with the given status and empty
    /// assertion/capture reports — the "no assertions/captures declared"
    /// case, which is what most `RunState`-plumbing tests actually need;
    /// tests about assertions/captures themselves build their own.
    pub(crate) fn sample_outcome(status: u16) -> RunOutcome {
        RunOutcome {
            result: Ok(sample_response(status)),
            assertions: AssertionReport::default(),
            capture: CaptureReport::default(),
        }
    }

    pub(crate) fn failed_outcome(error: SendraError) -> RunOutcome {
        RunOutcome {
            result: Err(error.into()),
            assertions: AssertionReport::default(),
            capture: CaptureReport::default(),
        }
    }

    /// Types `text` into whichever field is currently focused, one
    /// `Message::EditInsertChar` per character — the same path a real
    /// keystroke takes through `main::translate_event`, not a shortcut that
    /// pokes `TextField` directly.
    pub(crate) fn type_into_focused_field(state: &mut AppState, text: &str) {
        for ch in text.chars() {
            update(state, Message::EditInsertChar(ch));
        }
    }

    /// `Message::EditBackspace` `n` times — enough to clear a field of known
    /// length before typing a replacement, since there is no "select all"
    /// message.
    pub(crate) fn backspace_n(state: &mut AppState, n: usize) {
        for _ in 0..n {
            update(state, Message::EditBackspace);
        }
    }

    /// Builds a `state` whose `LoadState::Loaded` really points at `path` on
    /// disk (unlike `loaded_state`'s own shared scratch file, this lets a
    /// test control exactly what's on disk before and after `SaveEdit`) —
    /// shared by every test that needs to inspect real bytes on a real
    /// filesystem rather than just the in-memory `Document`.
    pub(crate) fn state_loaded_from(path: &std::path::Path) -> AppState {
        let mut state = AppState::default();
        let document = Document::from_path(path).expect("the fixture file must parse");
        update(
            &mut state,
            Message::CollectionLoaded {
                base_dir: path.parent().unwrap().to_path_buf(),
                path: path.to_path_buf(),
                result: Box::new(Ok(document)),
            },
        );
        state
    }

    /// The whole `Document` behind `state`'s active session — what tests
    /// about `Document::Single`/`Document::Collection`'s own shape need,
    /// rather than one request out of it.
    pub(crate) fn saved_document(state: &CollectionSession) -> &Document {
        match &state.load_state {
            LoadState::Loaded { document, .. } => document,
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        }
    }

    /// Opens a brand-new collection as a real tab — exactly the
    /// `Message::CollectionOpened` shape `main::run`'s own loop builds after
    /// intercepting `Message::ConfirmOpenCollectionPath` (see that variant's
    /// own doc comment), parsed from `yaml` and written to a real file in its
    /// own temp directory so `Message::SaveEdit` has somewhere genuine to
    /// land for this tab too — the same "real disk, not just an in-memory
    /// `Document`" bar every other save/delete/environment-edit test already
    /// holds itself to. Returns the id `AppState::open_session` assigned it,
    /// the `TempDir` guard (kept alive by the caller — see
    /// `state_with_saved_environment`'s own doc comment on why dropping it
    /// early would be fatal), and the file's path.
    pub(crate) fn open_second_collection(
        state: &mut AppState,
        yaml: &str,
    ) -> (u64, tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = dir.path().join("second.yaml");
        std::fs::write(&path, yaml).unwrap();
        let document = Document::from_path(&path).expect("valid test YAML");
        update(
            state,
            Message::CollectionOpened {
                base_dir: dir.path().to_path_buf(),
                path,
                result: Box::new(Ok(document)),
                environments: Vec::new(),
                environment_errors: Vec::new(),
            },
        );
        (state.active().id, dir, state_path(state))
    }

    /// The real, on-disk path of the currently active session — what
    /// `open_second_collection` hands back so a test can later reload that
    /// exact file with a fresh `Document::from_path`.
    pub(crate) fn state_path(state: &AppState) -> PathBuf {
        match &state.load_state {
            LoadState::Loaded { path, .. } => path.clone(),
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        }
    }

    /// Every visible cell of a rendered `Buffer`, row by row, as plain text —
    /// what every screen-rendering test asserts against instead of poking
    /// ratatui's own `Buffer`/`Cell` types directly.
    pub(crate) fn buffer_to_string(buffer: &ratatui::buffer::Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Renders `state` through the real `view()` into a `TestBackend` and
    /// returns the resulting screen as plain text — shared by every test
    /// across `browser`/`edit_form`/`environment`/`response`'s own test
    /// modules that needs to assert on what actually reaches the screen,
    /// not just on `AppState` fields. Tall enough that the edit pane's auth
    /// section and its "Resolved auth" preview line are never clipped by
    /// the pane's own height.
    pub(crate) fn render_screen(state: &AppState) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        terminal
            .draw(|frame| view(state, frame))
            .expect("rendering must not panic");
        buffer_to_string(terminal.backend().buffer())
    }

    /// A minimal, fixed-shape `Response` for formatting tests — status
    /// `201 Created`, `headers`/`body` as given, shared by every response
    /// formatting test that needs a real `Response` to format rather than
    /// one hand-built per test.
    pub(crate) fn response_with(headers: &[(&str, &str)], body: &str) -> Response {
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

    /// Evaluates `yaml`'s `assertions:` block against `response`, via the
    /// real `Assertions::evaluate` — shared by every test that checks
    /// formatting of a real `AssertionReport` rather than a hand-built one.
    pub(crate) fn evaluate_assertions(yaml: &str, response: &Response) -> AssertionReport {
        let request = Document::from_yaml_str(yaml)
            .expect("valid test request")
            .requests()[0]
            .clone();
        request
            .assertions
            .expect("the test YAML declares an `assertions:` block")
            .evaluate(response)
    }

    /// Same as [`evaluate_assertions`] for a `capture:` block, via the real
    /// `Captures::evaluate`.
    pub(crate) fn evaluate_capture(yaml: &str, response: &Response) -> CaptureReport {
        let request = Document::from_yaml_str(yaml)
            .expect("valid test request")
            .requests()[0]
            .clone();
        request
            .capture
            .expect("the test YAML declares a `capture:` block")
            .evaluate(response, &Environment::default())
    }

    /// A loaded collection whose one request substitutes `{{base_url}}`,
    /// paired with a real environment file on disk that defines it —
    /// everything `preview::resolve_browsing_preview`'s real pipeline needs
    /// to resolve a live URL, and everything `Message::SaveEnvironmentEdit`
    /// needs to persist an edit to. Shared by `view`'s own `environment` and
    /// `mod` test modules, both of which need a real, saved environment to
    /// edit rather than `state_with_environments`'s sourceless ones.
    pub(crate) fn loaded_state_with_saved_environment(
        variable_value: &str,
    ) -> (AppState, tempfile::TempDir, PathBuf) {
        let mut state = loaded_state(
            "name: test\nrequests:\n  - name: One\n    method: GET\n    \
             url: 'https://example.com/{{base_url}}'\n",
        );

        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = sendra_core::environment::environment_path(dir.path(), "staging");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("base_url: {variable_value}\n")).unwrap();
        let environment = Environment::from_path(&path).expect("the fixture file must parse");

        state.environments = vec![NamedEnvironment {
            name: "staging".to_string(),
            environment,
        }];
        state.active_environment = Some(0);

        (state, dir, path)
    }
}
