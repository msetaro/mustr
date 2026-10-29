//! A scripted T3 for WP18's pages (usage, servers, project settings, source control, provider
//! instances): it records every call with its access level, and answers each RPC from what the
//! test gave it, so tests assert the exact payloads. Nothing here talks to T3.

use anyhow::{Result, anyhow};
use futures::{FutureExt as _, StreamExt as _, future::BoxFuture, stream::BoxStream};
use parking_lot::Mutex;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
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
        OrchestrationV2ThreadLaunchResult, ServerProvider,
    },
};
use thread_chat::{Access, AttachmentUpload, ThreadBackend, UnsupportedRpc};

/// One call T3 got.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Call {
    pub tag: String,
    pub payload: Value,
    pub access: Access,
}

impl Call {
    pub(crate) fn new(tag: &str, payload: Value, access: Access) -> Self {
        Self {
            tag: tag.to_string(),
            payload,
            access,
        }
    }
}

type Answer = std::result::Result<Value, String>;

#[derive(Default)]
pub(crate) struct FakeT3 {
    calls: Mutex<Vec<Call>>,
    /// Answers given once each, in order, before the standing ones.
    queued: Mutex<HashMap<String, VecDeque<Answer>>>,
    standing: Mutex<HashMap<String, Answer>>,
    unsupported: Mutex<HashSet<String>>,
    /// Refuses calls that need these, as a pairing without their scopes does.
    blocked: Mutex<HashMap<Access, String>>,
    /// The shell snapshot T3 serves, if any.
    shell: Mutex<Option<Value>>,
}

impl FakeT3 {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Answers every `tag` call with `answer`.
    pub(crate) fn answer(&self, tag: &str, answer: Value) {
        self.standing.lock().insert(tag.to_string(), Ok(answer));
    }

    pub(crate) fn has_answer(&self, tag: &str) -> bool {
        self.standing.lock().contains_key(tag)
    }

    /// Answers the next `tag` call with `answer`, then goes back to the standing answer.
    pub(crate) fn answer_once(&self, tag: &str, answer: Value) {
        self.queued
            .lock()
            .entry(tag.to_string())
            .or_default()
            .push_back(Ok(answer));
    }

    pub(crate) fn fail(&self, tag: &str, error: &str) {
        self.standing
            .lock()
            .insert(tag.to_string(), Err(error.to_string()));
    }

    /// Answers `tag` as a server without the RPC does.
    pub(crate) fn unsupported(&self, tag: &str) {
        self.unsupported.lock().insert(tag.to_string());
    }

    /// Serves a shell with these projects (each `{id, title, workspaceRoot, …}`, completed with
    /// T3's other fields) and no threads.
    pub(crate) fn serve_projects(&self, projects: Vec<Value>) {
        let projects = projects
            .into_iter()
            .map(|project| {
                let mut full = serde_json::json!({
                    "scripts": [], "createdAt": "2026-10-01T00:00:00.000Z",
                    "updatedAt": "2026-10-01T00:00:00.000Z", "defaultModelSelection": null
                });
                util::merge_json_value_into(project, &mut full);
                full
            })
            .collect::<Vec<_>>();
        *self.shell.lock() = Some(serde_json::json!({
            "schemaVersion": 1, "snapshotSequence": 1, "threads": [], "archivedThreads": [],
            "projects": projects
        }));
    }

    pub(crate) fn block(&self, access: Access, reason: &str) {
        self.blocked.lock().insert(access, reason.to_string());
    }

    /// Every call but the app's own server-config subscription.
    pub(crate) fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .iter()
            .filter(|call| call.tag != "subscribeServerConfig")
            .cloned()
            .collect()
    }

    pub(crate) fn calls_to(&self, tag: &str) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|call| call.tag == tag)
            .collect()
    }

    pub(crate) fn clear_calls(&self) {
        self.calls.lock().clear();
    }

    fn reply(&self, tag: &str, payload: Value, access: Access) -> Result<Value> {
        self.calls.lock().push(Call::new(tag, payload, access));
        if let Some(reason) = self.blocked.lock().get(&access) {
            return Err(anyhow!(reason.clone()));
        }
        if self.unsupported.lock().contains(tag) {
            return Err(anyhow::Error::new(UnsupportedRpc {
                tag: tag.to_string(),
            }));
        }
        let queued = self
            .queued
            .lock()
            .get_mut(tag)
            .and_then(|answers| answers.pop_front());
        let answer = queued.or_else(|| self.standing.lock().get(tag).cloned());
        match answer {
            Some(Ok(value)) => Ok(value),
            Some(Err(error)) => Err(anyhow!(error)),
            None => Err(anyhow!("the fake T3 has no answer for {tag}")),
        }
    }
}

fn ready<T: Send + 'static>(value: Result<T>) -> BoxFuture<'static, Result<T>> {
    futures::future::ready(value).boxed()
}

impl ThreadBackend for FakeT3 {
    fn shell_snapshot(&self) -> BoxFuture<'static, Result<OrchestrationV2ShellSnapshot>> {
        ready(match self.shell.lock().clone() {
            Some(shell) => serde_json::from_value(shell).map_err(Into::into),
            None => Err(anyhow!("the fake T3 has no shell")),
        })
    }

    fn subscribe_shell(
        &self,
        _: OrchestrationV2SubscribeShellInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ShellStreamItem>>> {
        Ok(futures::stream::pending().boxed())
    }

    fn thread_snapshot(
        &self,
        _: String,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadDetailSnapshot>> {
        ready(Err(anyhow!("the fake T3 has no threads")))
    }

    fn subscribe_thread(
        &self,
        _: OrchestrationV2SubscribeThreadInput,
    ) -> Result<BoxStream<'static, Result<OrchestrationV2ThreadStreamItem>>> {
        Err(anyhow!("the fake T3 has no threads"))
    }

    fn turn_item(
        &self,
        _: OrchestrationV2GetTurnItemInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2GetTurnItemResult>> {
        ready(Err(anyhow!("the fake T3 has no threads")))
    }

    fn providers(&self) -> BoxFuture<'static, Result<Vec<ServerProvider>>> {
        ready(Ok(Vec::new()))
    }

    fn dispatch(
        &self,
        _: OrchestrationV2Command,
    ) -> BoxFuture<'static, Result<OrchestrationV2DispatchCommandResult>> {
        ready(Err(anyhow!(
            "WP18's pages dispatch no orchestration commands"
        )))
    }

    fn launch_thread(
        &self,
        _: OrchestrationV2ThreadLaunchInput,
    ) -> BoxFuture<'static, Result<OrchestrationV2ThreadLaunchResult>> {
        ready(Err(anyhow!("WP18's pages launch no threads")))
    }

    fn call_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> BoxFuture<'static, Result<Value>> {
        ready(self.reply(tag, payload, access))
    }

    fn subscribe_raw(
        &self,
        tag: &str,
        payload: Value,
        access: Access,
    ) -> Result<BoxStream<'static, Result<Value>>> {
        self.calls.lock().push(Call::new(tag, payload, access));
        Ok(futures::stream::pending().boxed())
    }

    fn upload(&self, _: AttachmentUpload) -> BoxFuture<'static, Result<()>> {
        ready(Err(anyhow!("WP18's pages upload nothing")))
    }

    fn blocked_reason(&self, access: Access) -> Option<String> {
        self.blocked.lock().get(&access).cloned()
    }
}
