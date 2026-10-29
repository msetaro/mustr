//! A stand-in for what agents do, in debug builds, for screenshots and manual checks of the
//! timeline and composer without starting an agent: with `MUSTR_DEBUG_THREAD_FIXTURE=<file>`
//! set, every thread view plays the file's events on top of what T3 sent, as if an agent had
//! asked a question, written a message, or rendered a page. `$THREAD` in the file is the
//! thread's id. Nothing goes to T3; writes still go to the real backend, which refuses them in
//! dry runs. Release builds don't have it.
//!
//! ```json
//! { "events": [
//!   { "type": "turn-item.updated", "payload": { "type": "assistant_message", "id": "f1", … } },
//!   { "type": "runtime-request.updated", "payload": { "id": "r1", "kind": "user_input", … } }
//! ] }
//! ```

use crate::thread_view::{Loadable, ThreadView};
use serde_json::Value;
use std::sync::OnceLock;
use t3_api::types::OrchestrationV2DomainEvent;

pub(crate) const ENV: &str = "MUSTR_DEBUG_THREAD_FIXTURE";

/// The fixture's events, as JSON with `$THREAD` still in them, read once.
fn fixture() -> Option<&'static Vec<Value>> {
    static FIXTURE: OnceLock<Option<Vec<Value>>> = OnceLock::new();
    FIXTURE
        .get_or_init(|| {
            let path = std::env::var_os(ENV)?;
            let text = std::fs::read_to_string(&path)
                .inspect_err(|error| log::error!("couldn't read {ENV}: {error}"))
                .ok()?;
            let fixture = serde_json::from_str::<Value>(&text)
                .inspect_err(|error| log::error!("couldn't read {ENV}: {error}"))
                .ok()?;
            fixture["events"].as_array().cloned()
        })
        .as_ref()
}

/// Whether a fixture plays. Its items aren't on T3, so find in thread looks in what's loaded.
pub(crate) fn active() -> bool {
    fixture().is_some()
}

impl ThreadView {
    /// Plays the fixture's events on the thread, once for each snapshot T3 sends.
    pub(crate) fn apply_debug_fixture(&mut self) {
        let Some(events) = fixture() else {
            return;
        };
        let Loadable::Loaded(state) = &mut self.state else {
            return;
        };
        if state.debug_fixture_applied {
            return;
        }
        state.debug_fixture_applied = true;
        let thread_id = state.thread_id().to_string();
        for (index, event) in events.iter().enumerate() {
            let text = event.to_string().replace("$THREAD", &thread_id);
            let Ok(mut event) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if let Some(fields) = event.as_object_mut() {
                fields
                    .entry("id")
                    .or_insert_with(|| format!("fixture-{index}").into());
                fields
                    .entry("threadId")
                    .or_insert_with(|| thread_id.clone().into());
                fields
                    .entry("occurredAt")
                    .or_insert_with(|| chrono::Utc::now().to_rfc3339().into());
            }
            match serde_json::from_value::<OrchestrationV2DomainEvent>(event) {
                Ok(event) => {
                    state.apply_fixture_event(event);
                }
                Err(error) => log::error!("{ENV}: event {index} doesn't decode: {error}"),
            }
        }
    }
}
