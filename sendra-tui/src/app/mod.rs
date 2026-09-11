//! sendra-tui's Elm-style architecture, split into its three natural layers:
//!
//! - [`state`]: `AppState` and everything it is built from — the model.
//! - [`update`]: `update()`, the one function allowed to mutate `AppState`.
//! - [`view`]: `view()` and every `render_*`/`format_*` function under it.
//!
//! This is a pure reorganization of what used to be one `app.rs` file along
//! boundaries the module already had (state/model, reducer, rendering) —
//! no behavior changed, only where the code lives. `main.rs` is unaffected:
//! it still reaches everything it needs through `crate::app::{...}`, exactly
//! the names re-exported below.

mod state;
mod update;
mod view;

pub use state::{AppState, LoadState, Message, NamedEnvironment, RunState};
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

    use super::state::{AppState, LoadState, Message, NamedEnvironment};
    use super::update::update;

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

    pub(crate) fn loaded_state(yaml: &str) -> AppState {
        let mut state = AppState::default();
        let document = Document::from_yaml_str(yaml).expect("valid test YAML");
        update(
            &mut state,
            Message::CollectionLoaded {
                base_dir: PathBuf::from("."),
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
        AppState {
            environments: names
                .iter()
                .map(|name| named_environment(name, &[]))
                .collect(),
            ..AppState::default()
        }
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
}
