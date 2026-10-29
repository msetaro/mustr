//! Starting threads from a draft without leaving it, as T3's composer does
//! (`chat/useComposerSend.ts`, `chat/useMultipleModelSend.ts`):
//! - **In the background:** [`StartInBackground`] (`mod+Enter` or `mod+alt+Enter` on a draft, T3's
//!   `composer.sendBackground`) launches the thread and keeps the draft, with its project, where
//!   it works, and its model, for the next one. A toast offers to open the thread.
//! - **With several models:** shift-clicking models in a draft's model picker
//!   (`settings_bar.rs`) picks several, and the draft then starts one thread for each, every one
//!   in a new worktree from the base branch, in the background. Models that fail stay picked,
//!   with the prompt, to try again.
//!
//! Every thread starts with `orchestration.launchThread`, once per model, through the installed
//! backend, so dry runs and read-only pairings refuse them.

use crate::{
    commands::{self, Delivery, LaunchIds},
    new_thread::{ModelPick, NewThreadForm},
    thread_view::ThreadView,
};
use gpui::{App, Context, Entity, Window, actions};
use workspace::{Toast, Workspace, notifications::NotificationId};

actions!(
    thread_chat,
    [
        /// Starts the draft's agent thread without opening it, and keeps the draft for the next
        /// one.
        StartInBackground,
    ]
);

/// "Started in the background", with Open.
pub(crate) struct StartedToast;

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| register(workspace))
        .detach();
}

fn register(workspace: &mut Workspace) {
    // On a thread that isn't a draft, the keys go on to their other bindings (Send).
    workspace.register_action(|workspace, _: &StartInBackground, window, cx| {
        let Some(view) = workspace
            .active_item(cx)
            .and_then(|item| item.downcast::<ThreadView>())
        else {
            cx.propagate();
            return;
        };
        let Some(form) = view.read(cx).new_thread.clone() else {
            cx.propagate();
            return;
        };
        form.update(cx, |form, _| form.background = true);
        view.update(cx, |view, cx| view.send(Delivery::Auto, window, cx));
        // The composer sends once its attachments are up; otherwise the launch has taken the
        // request, or nothing was sent.
        if view.read(cx).images.send_when_uploaded.is_none() {
            form.update(cx, |form, _| form.background = false);
        }
    });
}

/// Whether the draft's launch starts its threads in the background: it was asked to, or it
/// starts several.
pub(crate) fn launches_in_background(form: &Entity<NewThreadForm>, cx: &App) -> bool {
    let form = form.read(cx);
    form.background || form.several_models().is_some()
}

impl ThreadView {
    /// Launches the draft's thread, or a thread for each of its models, in the background: the
    /// draft stays, and its composer empties once they've started.
    pub(crate) fn launch_in_background(
        &mut self,
        form: Entity<NewThreadForm>,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        form.update(cx, |form, _| form.background = false);
        let text = text.trim().to_string();
        let launches = match form.read(cx).launches(&text, cx) {
            Ok(launches) => launches,
            Err(reason) => {
                self.error = Some(reason);
                cx.notify();
                return;
            }
        };
        let picks = form
            .read(cx)
            .several_models()
            .map(<[ModelPick]>::to_vec)
            .unwrap_or_default();
        let several = !picks.is_empty();
        let sent_images = self.images.uploaded_keys();
        let started = launches
            .into_iter()
            .map(|launch| {
                let title = commands::title_from_message(&launch.new_thread.text);
                let mut input = commands::launch(launch.new_thread, LaunchIds::new());
                self.attach_mention_context(&mut input);
                self.attach_composer_images(&mut input);
                let launch_thread = self.backend.launch_thread(input);
                (launch.model_name, title, launch_thread)
            })
            .collect::<Vec<_>>();
        self.sending = true;
        self.error = None;
        cx.notify();
        let workspace = Workspace::for_window(window, cx).map(|workspace| workspace.downgrade());
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let mut results = Vec::new();
            for (model_name, title, launch_thread) in started {
                results.push((model_name, title, launch_thread.await));
            }
            this.update_in(cx, |this, window, cx| {
                this.sending = false;
                let mut opened = Vec::new();
                let mut failures = Vec::new();
                let mut failed_picks = Vec::new();
                for (index, (model_name, title, result)) in results.into_iter().enumerate() {
                    match result {
                        Ok(launched) => opened.push((launched.thread_id, title)),
                        Err(error) => {
                            failures.push(if several {
                                format!("Couldn't start {model_name}: {error:#}")
                            } else {
                                format!("{error:#}")
                            });
                            failed_picks.extend(picks.get(index).cloned());
                        }
                    }
                }
                if failures.is_empty() {
                    this.images.clear_sent(&sent_images);
                    // Unless the user has started on the next prompt meanwhile.
                    let unchanged = this.composer.read(cx).text(cx).trim() == text;
                    if unchanged {
                        this.composer
                            .update(cx, |editor, cx| editor.clear(window, cx));
                    }
                } else {
                    this.error = Some(failures.join("\n").into());
                    if several && !opened.is_empty() {
                        form.update(cx, |form, cx| form.keep_models(&failed_picks, cx));
                    }
                }
                if let Some(workspace) = &workspace {
                    show_started(&opened, workspace.clone(), cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }
}

/// "Started in the background", with Open for one thread.
fn show_started(opened: &[(String, String)], workspace: gpui::WeakEntity<Workspace>, cx: &mut App) {
    let (message, open): (String, _) = match opened {
        [] => return,
        [(thread_id, title)] => (
            format!("Started “{title}” in the background."),
            Some((thread_id.clone(), title.clone())),
        ),
        several => (
            format!("Started {} threads in the background.", several.len()),
            None,
        ),
    };
    let toast_workspace = workspace.clone();
    workspace
        .update(cx, |workspace, cx| {
            let mut toast = Toast::new(NotificationId::unique::<StartedToast>(), message);
            if let Some((thread_id, title)) = open {
                toast = toast.on_click("Open", move |window, cx| {
                    crate::open_thread(
                        thread_id.clone(),
                        title.clone().into(),
                        toast_workspace.clone(),
                        window,
                        cx,
                    )
                });
            }
            workspace.show_toast(toast.autohide(), cx)
        })
        .ok();
}
