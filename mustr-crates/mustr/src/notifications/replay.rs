//! A stand-in for agent runs in debug builds, for screenshots and manual checks of
//! notifications without starting an agent: with `MUSTR_DEBUG_SHELL_REPLAY=<script.json>` set,
//! the threads' backend plays scripted changes to T3's threads over the real shell, as if agents
//! ran. Everything else, writes included, goes to the real backend, which still refuses writes
//! in dry runs. Release builds don't have it.
//!
//! The script names threads by ID or by title, and sets fields of their shell records (T3's wire
//! names). A step waits `after_ms` after the one before, and then, if it names one, until a file
//! exists. `"$NOW"` in a value is the time the step plays.
//!
//! ```json
//! { "steps": [
//!   { "after_ms": 2000, "thread": "Fix the flaky checkout e2e test",
//!     "set": { "status": "running", "latestRunId": "replay-1", "activityRunStatus": "running" } },
//!   { "after_ms": 3000, "when_file_exists": "/tmp/go", "thread": "Fix the flaky checkout e2e test",
//!     "set": { "pendingRuntimeRequest": { "id": "r1", "kind": "command", "createdAt": "$NOW" } } }
//! ] }
//! ```
//!
//! The shell's sequence numbers are renumbered as items pass, so T3's own updates aren't dropped
//! as stale after a scripted one.

use anyhow::{Context as _, Result, anyhow};
use futures::{
    FutureExt as _, StreamExt as _, channel::mpsc, future::BoxFuture, stream::BoxStream,
};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};
use t3_client::t3_api::{
    tagged::{
        OrchestrationV2Command, OrchestrationV2ShellStreamItem, OrchestrationV2ThreadStreamItem,
    },
    types::{
        OrchestrationV2DispatchCommandResult, OrchestrationV2GetTurnItemInput,
        OrchestrationV2GetTurnItemResult, OrchestrationV2ShellSnapshot,
        OrchestrationV2SubscribeShellInput, OrchestrationV2SubscribeThreadInput,
        OrchestrationV2ThreadDetailSnapshot, OrchestrationV2ThreadLaunchInput,
        OrchestrationV2ThreadLaunchResult, OrchestrationV2ThreadShell, ServerProvider,
    },
};
use thread_chat::{Access, AttachmentUpload, ThreadBackend};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub const ENV: &str = "MUSTR_DEBUG_SHELL_REPLAY";

#[derive(Debug, Deserialize)]
struct Script {
    steps: Vec<Step>,
}

#[derive(Clone, Debug, Deserialize)]
struct Step {
    #[serde(default)]
    after_ms: u64,
    #[serde(default)]
    when_file_exists: Option<PathBuf>,
    /// A thread's ID or title.
    thread: String,
    set: Map<String, Value>,
}

/// The backend, replaying the script in [`ENV`] if it's set.
pub fn wrap(backend: Arc<dyn ThreadBackend>) -> Arc<dyn ThreadBackend> {
    let Some(path) = std::env::var_os(ENV) else {
        return backend;
    };
    match load(Path::new(&path)) {
        Ok(script) => {
            log::warn!(
                "{ENV} is set: replaying {} scripted changes to threads",
                script.steps.len()
            );
            Arc::new(ReplayBackend {
                inner: backend,
                state: Arc::new(Mutex::new(State {
                    script: Some(script),
                    ..State::default()
                })),
            })
        }
        Err(error) => {
            log::error!("couldn't load {ENV}: {error:#}");
            backend
        }
    }
}

fn load(path: &Path) -> Result<Script> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {path:?}"))
}

#[derive(Default)]
struct State {
    /// The script, until the first subscription to the shell starts it.
    script: Option<Script>,
    /// Each thread's record as T3 last sent it.
    threads: std::collections::HashMap<String, Value>,
    /// The fields the script set, by thread.
    overlay: std::collections::HashMap<String, Map<String, Value>>,
}

impl State {
    /// Records T3's thread and returns it with the script's fields.
    fn patch(&mut self, thread: OrchestrationV2ThreadShell) -> OrchestrationV2ThreadShell {
        let Ok(record) = serde_json::to_value(&thread) else {
            return thread;
        };
        self.threads.insert(thread.id.clone(), record.clone());
        let Some(overlay) = self.overlay.get(&thread.id) else {
            return thread;
        };
        overlaid(record, overlay).unwrap_or(thread)
    }

    /// Plays one step, returning the thread as it is now.
    fn play(&mut self, step: &Step) -> Result<OrchestrationV2ThreadShell> {
        let (id, record) = self
            .threads
            .iter()
            .find(|(id, record)| **id == step.thread || record["title"] == step.thread)
            .map(|(id, record)| (id.clone(), record.clone()))
            .ok_or_else(|| anyhow!("no thread {:?} in the shell", step.thread))?;
        let now = OffsetDateTime::now_utc().format(&Rfc3339)?;
        let overlay = self.overlay.entry(id).or_default();
        for (key, value) in &step.set {
            overlay.insert(key.clone(), with_now(value.clone(), &now));
        }
        overlaid(record, overlay)
    }
}

fn overlaid(mut record: Value, overlay: &Map<String, Value>) -> Result<OrchestrationV2ThreadShell> {
    if let Some(record) = record.as_object_mut() {
        record.extend(overlay.clone());
    }
    serde_json::from_value(record).context("a scripted thread record")
}

fn with_now(value: Value, now: &str) -> Value {
    match value {
        Value::String(text) if text == "$NOW" => Value::String(now.to_string()),
        Value::Array(items) => Value::Array(items.into_iter().map(|v| with_now(v, now)).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key, with_now(value, now)))
                .collect(),
        ),
        other => other,
    }
}

struct ReplayBackend {
    inner: Arc<dyn ThreadBackend>,
    state: Arc<Mutex<State>>,
}

impl ReplayBackend {
    /// The script's steps as stream items, each once its time comes. A thread waits out the
    /// delays, so the stream needs no executor's timer.
    fn steps(&self) -> BoxStream<'static, OrchestrationV2ShellStreamItem> {
        let Some(script) = self.state.lock().script.take() else {
            return futures::stream::pending().boxed();
        };
        let (sender, receiver) = mpsc::unbounded();
        let spawned = std::thread::Builder::new()
            .name("shell-replay".to_string())
            .spawn(move || {
                for step in script.steps {
                    std::thread::sleep(Duration::from_millis(step.after_ms));
                    if let Some(path) = &step.when_file_exists {
                        while !path.exists() {
                            std::thread::sleep(Duration::from_millis(200));
                        }
                    }
                    if sender.unbounded_send(step).is_err() {
                        return;
                    }
                }
            });
        if let Err(error) = spawned {
            log::error!("couldn't start the shell replay: {error}");
        }
        let state = self.state.clone();
        receiver
            .filter_map(move |step| {
                let played = state.lock().play(&step);
                let item = match played {
                    Ok(thread) => {
                        log::info!("replayed a change to thread {:?}", step.thread);
                        serde_json::from_value(json!({
                            "kind": "thread.updated",
                            "location": "active",
                            "sequence": 0,
                            "thread": thread,
                        }))
                        .map_err(anyhow::Error::from)
                    }
                    Err(error) => Err(error),
                };
                futures::future::ready(
                    item.map_err(|error| log::error!("couldn't replay a step: {error:#}"))
                        .ok(),
                )
            })
            .boxed()
    }
}

impl ThreadBackend for ReplayBackend {
    fn shell_snapshot(&self) -> BoxFuture<'static, Result<OrchestrationV2ShellSnapshot>> {
        let snapshot = self.inner.shell_snapshot();
        let state = self.state.clone();
        async move {
            let mut snapshot = snapshot.await?;
            let mut state = state.lock();
            snapshot.threads = snapshot
                .threads
                .into_iter()
                .map(|thread| state.patch(thread))
                .collect();
            Ok(snapshot)
        }
        .boxed()
    }

    fn subscribe_shell(
        &self,
        input: OrchestrationV2SubscribeShellInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ShellStreamItem>>> {
        let sequence = Arc::new(AtomicI64::new(input.after_sequence.unwrap_or_default()));
        let real = self.inner.subscribe_shell(input)?;
        let state = self.state.clone();
        let real = real.map(move |item| {
            item.map(|item| match item {
                OrchestrationV2ShellStreamItem::ThreadUpdated(mut update) => {
                    update.thread = state.lock().patch(update.thread);
                    OrchestrationV2ShellStreamItem::ThreadUpdated(update)
                }
                OrchestrationV2ShellStreamItem::Snapshot(mut snapshot) => {
                    let mut state = state.lock();
                    snapshot.snapshot.threads = std::mem::take(&mut snapshot.snapshot.threads)
                        .into_iter()
                        .map(|thread| state.patch(thread))
                        .collect();
                    OrchestrationV2ShellStreamItem::Snapshot(snapshot)
                }
                other => other,
            })
        });
        let scripted = self.steps().map(Ok);
        Ok(futures::stream::select(real, scripted)
            .map(move |item| item.map(|item| renumber(item, &sequence)))
            .boxed())
    }

    fn thread_snapshot(
        &self,
        thread_id: String,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadDetailSnapshot>> {
        self.inner.thread_snapshot(thread_id)
    }

    fn subscribe_thread(
        &self,
        input: OrchestrationV2SubscribeThreadInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ThreadStreamItem>>> {
        self.inner.subscribe_thread(input)
    }

    fn turn_item(
        &self,
        input: OrchestrationV2GetTurnItemInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2GetTurnItemResult>> {
        self.inner.turn_item(input)
    }

    fn providers(&self) -> BoxFuture<'static, Result<Vec<ServerProvider>>> {
        self.inner.providers()
    }

    fn dispatch(
        &self,
        command: OrchestrationV2Command,
    ) -> BoxFuture<'static, Result<OrchestrationV2DispatchCommandResult>> {
        self.inner.dispatch(command)
    }

    fn launch_thread(
        &self,
        input: OrchestrationV2ThreadLaunchInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadLaunchResult>> {
        self.inner.launch_thread(input)
    }

    fn call_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> BoxFuture<'static, Result<Value>> {
        self.inner.call_raw(tag, payload, access)
    }

    fn subscribe_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> Result<BoxStream<'static, Result<Value>>> {
        self.inner.subscribe_raw(tag, payload, access)
    }

    fn upload(&self, upload: AttachmentUpload) -> BoxFuture<'static, Result<()>> {
        self.inner.upload(upload)
    }

    fn fetch_asset(&self, relative_url: &str) -> BoxFuture<'static, Result<Vec<u8>>> {
        self.inner.fetch_asset(relative_url)
    }

    fn write_blocked_reason(&self) -> Option<String> {
        self.inner.write_blocked_reason()
    }

    fn blocked_reason(&self, access: Access) -> Option<String> {
        self.inner.blocked_reason(access)
    }
}

/// Gives each item the next sequence number, so the shell applies T3's items and the script's
/// in the order they arrive.
fn renumber(
    item: OrchestrationV2ShellStreamItem,
    last: &AtomicI64,
) -> OrchestrationV2ShellStreamItem {
    let next = || last.fetch_add(1, Ordering::SeqCst) + 1;
    match item {
        OrchestrationV2ShellStreamItem::ThreadUpdated(mut update) => {
            update.sequence = next();
            OrchestrationV2ShellStreamItem::ThreadUpdated(update)
        }
        OrchestrationV2ShellStreamItem::ThreadRemoved(mut removed) => {
            removed.sequence = next();
            OrchestrationV2ShellStreamItem::ThreadRemoved(removed)
        }
        OrchestrationV2ShellStreamItem::ProjectUpdated(mut update) => {
            update.sequence = next();
            OrchestrationV2ShellStreamItem::ProjectUpdated(update)
        }
        OrchestrationV2ShellStreamItem::ProjectRemoved(mut removed) => {
            removed.sequence = next();
            OrchestrationV2ShellStreamItem::ProjectRemoved(removed)
        }
        OrchestrationV2ShellStreamItem::Snapshot(snapshot) => {
            last.fetch_max(snapshot.snapshot.snapshot_sequence, Ordering::SeqCst);
            OrchestrationV2ShellStreamItem::Snapshot(snapshot)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread_git::tests::{RecordingBackend, shell, shell_thread};
    use pretty_assertions::assert_eq;

    #[test]
    fn scripted_changes_play_over_the_real_shell() {
        let backend = RecordingBackend::with_shell(shell(vec![shell_thread(json!({
            "id": "a", "title": "Add retries", "status": "idle",
        }))]));
        let script: Script = serde_json::from_value(json!({ "steps": [{
            "after_ms": 0, "thread": "Add retries",
            "set": {
                "status": "running", "latestRunId": "replay-1",
                "pendingRuntimeRequest": { "id": "r1", "kind": "command", "createdAt": "$NOW" },
            },
        }] }))
        .unwrap();
        let replay = ReplayBackend {
            inner: backend,
            state: Arc::new(Mutex::new(State {
                script: Some(script),
                ..State::default()
            })),
        };
        let snapshot = futures::executor::block_on(replay.shell_snapshot()).unwrap();
        assert_eq!(snapshot.threads[0].latest_run_id, None);

        let mut stream = replay
            .subscribe_shell(OrchestrationV2SubscribeShellInput {
                after_sequence: Some(snapshot.snapshot_sequence),
                omit_idle_subagents: None,
                omit_older_settled: None,
                request_completion_marker: None,
            })
            .unwrap();
        let item = futures::executor::block_on(stream.next()).unwrap().unwrap();
        let OrchestrationV2ShellStreamItem::ThreadUpdated(update) = item else {
            panic!("expected a thread update, got {item:?}");
        };
        assert_eq!(update.sequence, snapshot.snapshot_sequence + 1);
        assert_eq!(update.thread.latest_run_id.as_deref(), Some("replay-1"));
        let request = update.thread.pending_runtime_request.unwrap();
        assert!(OffsetDateTime::parse(&request.created_at, &Rfc3339).is_ok());

        // T3's next record of the thread keeps the script's fields.
        let again = futures::executor::block_on(replay.shell_snapshot()).unwrap();
        assert_eq!(again.threads[0].latest_run_id.as_deref(), Some("replay-1"));
    }
}
