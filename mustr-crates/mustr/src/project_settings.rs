//! A project's settings (`mustr-docs/PARITY.md` §2.2, "Project settings"), as T3's project panel
//! has them (`settings/ProjectSettingsPanel.tsx`, `ProjectDefaultsSettings.tsx`):
//! - **Name:** `projects.mutate` with `project.update` `{title}`, which needs
//!   `orchestration:operate`.
//! - **New threads:** the model and the workspace (the checkout or a new worktree) the project's
//!   new threads start with, or T3's defaults. T3's app keeps these in T3's settings as the
//!   project's overrides (`server.updateSettings` `{projectSettingsOverrides: {<id>: …}}`, which
//!   needs `settings:write`), and mustr's new threads read them there
//!   (`thread_chat::thread_settings::thread_defaults`). A project's override entry is replaced
//!   whole, so a change sends the entry T3 has with that one key changed, keeping keys mustr
//!   doesn't know.
//! - **Scripts:** listed; the terminal panel's Run ▾ runs them.
//!
//! It opens from a thread's menu and Home's projects ([`thread_chat::OpenProjectSettings`]), and
//! from the palette for the open thread's project ([`ProjectSettings`]).

#[cfg(test)]
mod tests;

use crate::t3_settings::{muted, row, section};
use anyhow::{Result, anyhow};
use editor::{Editor, EditorEvent};
use futures::{FutureExt as _, future::BoxFuture};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, Task, Window, div, px,
};
use mustr_ui::{ButtonStyle, Card, SegmentedControl, TextButton, palette, tokens, tokens::scaled};
use serde_json::{Map, Value, json};
use std::{rc::Rc, sync::Arc};
use t3_client::t3_api::types::{ModelSelection, ThreadEnvMode};
use thread_chat::{
    Access, OpenProjectSettings, ServerConfigs, ThreadBackend, ThreadShells, ThreadView,
    thread_settings::{agent_choices, selection_instance_id, selection_model},
};
use ui::{
    Color, ContextMenu, DropdownMenu, DropdownStyle, Headline, HeadlineSize, Icon, IconName,
    IconPosition, Label, LabelCommon as _, LabelSize, Tooltip, prelude::*,
};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

const CONTENT_MAX_WIDTH: f32 = 880.;

gpui::actions!(
    mustr,
    [
        /// Opens the settings of the open thread's project: its name, and the model and
        /// workspace its new threads start with.
        ProjectSettings,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, action: &OpenProjectSettings, window, cx| {
            show(workspace, Some(action.project_id.clone()), window, cx)
        });
        workspace.register_action(|workspace, _: &ProjectSettings, window, cx| {
            let thread_id = workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<ThreadView>())
                .and_then(|view| view.read(cx).thread_id().map(str::to_string));
            let project_id = thread_id.and_then(|thread_id| {
                let shells = ThreadShells::global(cx);
                let shells = shells.read(cx);
                Some(shells.shell()?.threads.get(&thread_id)?.project_id.clone())
            });
            show(workspace, project_id, window, cx)
        });
    })
    .detach();
}

/// Opens the settings page on `project_id` (or the first project), or shows it there.
pub fn show(
    workspace: &mut Workspace,
    project_id: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .panes()
        .iter()
        .find_map(|pane| pane.read(cx).items_of_type::<ProjectSettingsView>().next());
    if let Some(existing) = existing {
        if let Some(project_id) = project_id {
            existing.update(cx, |view, cx| view.select_project(project_id, window, cx));
        }
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let backend = thread_chat::backend(cx);
    let view = cx.new(|cx| ProjectSettingsView::new(backend, project_id, window, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

// What's sent.

/// `projects.mutate` renaming a project (`project.update` `{title}`).
pub(crate) fn rename_mutation(project_id: &str, title: &str, command_id: String) -> Value {
    json!({
        "type": "project.update",
        "commandId": command_id,
        "projectId": project_id,
        "title": title,
    })
}

/// The project's override entry in T3's raw settings, as an object.
fn override_entry(settings: &Value, project_id: &str) -> Map<String, Value> {
    settings
        .get("projectSettingsOverrides")
        .and_then(|overrides| overrides.get(project_id))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// The `server.updateSettings` patch that sets one of the project's overrides (`Some`) or goes
/// back to T3's setting (`None`): the whole entry, with that key changed, since T3 replaces a
/// project's entry whole.
pub(crate) fn override_patch(
    settings: &Value,
    project_id: &str,
    key: &str,
    value: Option<Value>,
) -> Value {
    let mut entry = override_entry(settings, project_id);
    match value {
        Some(value) => {
            entry.insert(key.to_string(), value);
        }
        None => {
            entry.remove(key);
        }
    }
    json!({ "projectSettingsOverrides": { project_id: entry } })
}

/// A setting as the project has it: its own override, or the project's legacy field (until T3
/// folds those into the overrides), or `None` for T3's setting.
fn project_value<'a>(
    settings: &'a Value,
    project: Option<&'a Value>,
    project_id: &str,
    key: &str,
) -> Option<&'a Value> {
    let folded = settings
        .get("projectSettingsFolded")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    settings
        .get("projectSettingsOverrides")
        .and_then(|overrides| overrides.get(project_id))
        .and_then(|entry| entry.get(key))
        .filter(|value| !value.is_null())
        .or_else(|| {
            project
                .filter(|_| !folded)
                .and_then(|project| project.get(key))
                .filter(|value| !value.is_null())
        })
}

fn call_raw(
    backend: &dyn ThreadBackend,
    tag: &'static str,
    payload: Value,
    access: Access,
) -> BoxFuture<'static, Result<Value>> {
    backend.call_raw(tag, payload, access)
}

/// T3's settings as JSON: `server.getSettings`, or `server.getConfig`'s on servers without it.
fn load_settings(backend: Arc<dyn ThreadBackend>) -> BoxFuture<'static, Result<Value>> {
    async move {
        match call_raw(
            backend.as_ref(),
            "server.getSettings",
            json!({}),
            Access::Read,
        )
        .await
        {
            Err(error) if thread_chat::is_unsupported_rpc(&error) => {
                let config = call_raw(
                    backend.as_ref(),
                    "server.getConfig",
                    json!({}),
                    Access::Read,
                )
                .await?;
                config
                    .get("settings")
                    .cloned()
                    .ok_or_else(|| anyhow!("T3 Code's config has no settings"))
            }
            settings => settings,
        }
    }
    .boxed()
}

// The page.

enum Loadable {
    Loading,
    Failed(SharedString),
    Loaded(Value),
}

pub struct ProjectSettingsView {
    focus_handle: FocusHandle,
    backend: Option<Arc<dyn ThreadBackend>>,
    configs: Entity<ServerConfigs>,
    shells: Entity<ThreadShells>,
    project_id: Option<String>,
    settings: Loadable,
    /// The server doesn't have `server.updateSettings`.
    write_unsupported: bool,
    /// The change being saved.
    saving: Option<&'static str>,
    error: Option<SharedString>,
    name: Entity<Editor>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ProjectSettingsView {
    pub(crate) fn new(
        backend: Option<Arc<dyn ThreadBackend>>,
        project_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let configs = ServerConfigs::global(cx);
        let shells = ThreadShells::global(cx);
        let name = cx.new(|cx| Editor::single_line(window, cx));
        let subscriptions = vec![
            cx.observe_in(&shells, window, |this, _, window, cx| {
                this.fill_name(false, window, cx);
                cx.notify();
            }),
            cx.observe(&configs, |_, _, cx| cx.notify()),
            cx.subscribe(&name, |_, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    cx.notify();
                }
            }),
        ];
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            backend,
            configs,
            shells,
            project_id,
            settings: Loadable::Loading,
            write_unsupported: false,
            saving: None,
            error: None,
            name,
            _tasks: Vec::new(),
            _subscriptions: subscriptions,
        };
        this.fill_name(true, window, cx);
        this.load(cx);
        this
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            self.settings = Loadable::Failed("mustr isn't connected to T3 Code".into());
            return;
        };
        let load = load_settings(backend);
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                this.settings = match result {
                    Ok(settings) => Loadable::Loaded(settings),
                    Err(error) => {
                        log::warn!("couldn't read T3's settings: {error:#}");
                        Loadable::Failed(
                            format!("Couldn't read T3 Code's settings: {error:#}").into(),
                        )
                    }
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// The project shown: the one asked for, else the first by name.
    fn project(&self, cx: &App) -> Option<Value> {
        let shell = self.shells.read(cx).shell()?;
        let project = match &self.project_id {
            Some(id) => shell.projects.get(id),
            None => shell
                .projects
                .values()
                .min_by_key(|project| project.title.to_lowercase()),
        }?;
        serde_json::to_value(project).ok()
    }

    fn current_project_id(&self, cx: &App) -> Option<String> {
        self.project(cx)
            .and_then(|project| project.get("id")?.as_str().map(str::to_string))
    }

    pub(crate) fn select_project(
        &mut self,
        project_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.project_id.as_deref() != Some(project_id.as_str()) {
            self.project_id = Some(project_id);
            self.error = None;
            self.fill_name(true, window, cx);
            cx.notify();
        }
    }

    /// Shows the project's name in its field, unless the user is changing it.
    fn fill_name(&mut self, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(title) = self
            .project(cx)
            .and_then(|project| project.get("title")?.as_str().map(str::to_string))
        else {
            return;
        };
        let editing = self.name.focus_handle(cx).is_focused(window);
        if force || (!editing && self.saving.is_none()) {
            self.name.update(cx, |editor, cx| {
                if editor.text(cx) != title {
                    editor.set_text(title, window, cx);
                }
            });
        }
    }

    fn blocked(&self, access: Access) -> Option<String> {
        if access == Access::SettingsWrite && self.write_unsupported {
            return Some("This T3 Code can't change its settings from mustr".into());
        }
        match &self.backend {
            Some(backend) => backend.blocked_reason(access),
            None => Some("mustr isn't connected to T3 Code".into()),
        }
    }

    pub(crate) fn rename(&mut self, cx: &mut Context<Self>) {
        let (Some(backend), Some(project_id)) = (self.backend.clone(), self.current_project_id(cx))
        else {
            return;
        };
        let title = self.name.read(cx).text(cx).trim().to_string();
        if title.is_empty() || self.saving.is_some() {
            return;
        }
        let call = call_raw(
            backend.as_ref(),
            "projects.mutate",
            rename_mutation(&project_id, &title, thread_chat::commands::new_id()),
            Access::Operate,
        );
        self.saving = Some("title");
        self.error = None;
        cx.notify();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                this.saving = None;
                if let Err(error) = result {
                    log::warn!("couldn't rename the project: {error:#}");
                    this.error = Some(format!("Couldn't rename the project: {error:#}").into());
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Sets (or with `None`, clears) one of the project's overrides in T3's settings, showing it
    /// at once and T3's settings once it answers.
    pub(crate) fn set_override(
        &mut self,
        key: &'static str,
        value: Option<Value>,
        cx: &mut Context<Self>,
    ) {
        let (Some(backend), Some(project_id)) = (self.backend.clone(), self.current_project_id(cx))
        else {
            return;
        };
        let Loadable::Loaded(before) = &self.settings else {
            return;
        };
        if self.saving.is_some() {
            return;
        }
        let before = before.clone();
        let patch = override_patch(&before, &project_id, key, value);
        let mut after = before.clone();
        if let Some(settings) = after.as_object_mut() {
            let overrides = settings
                .entry("projectSettingsOverrides")
                .or_insert_with(|| json!({}));
            if let (Value::Object(overrides), Value::Object(changes)) =
                (overrides, &patch["projectSettingsOverrides"])
            {
                overrides.extend(changes.clone());
            }
        }
        self.settings = Loadable::Loaded(after);
        self.saving = Some(key);
        self.error = None;
        let call = call_raw(
            backend.as_ref(),
            "server.updateSettings",
            json!({ "patch": patch }),
            Access::SettingsWrite,
        );
        cx.notify();
        let configs = self.configs.clone();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                this.saving = None;
                match result {
                    Ok(settings) => {
                        if let Ok(typed) = serde_json::from_value(settings.clone()) {
                            configs.update(cx, |configs, cx| configs.set_settings(typed, cx));
                        }
                        this.settings = Loadable::Loaded(settings);
                    }
                    Err(error) => {
                        this.settings = Loadable::Loaded(before);
                        if thread_chat::is_unsupported_rpc(&error) {
                            this.write_unsupported = true;
                        } else {
                            log::warn!("couldn't change the project's settings: {error:#}");
                            this.error =
                                Some(format!("Couldn't save the setting: {error:#}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    #[cfg(test)]
    pub(crate) fn settings_value(&self) -> Option<&Value> {
        match &self.settings {
            Loadable::Loaded(settings) => Some(settings),
            _ => None,
        }
    }
}

// Rendering.

impl ProjectSettingsView {
    fn dropdown(
        &self,
        id: &'static str,
        label: String,
        entries: Vec<(String, Rc<dyn Fn(&mut Self, &mut Context<Self>)>)>,
        selected: Option<usize>,
        blocked: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let key = SharedString::from(format!(
            "{id}-{}-{}-{}",
            entries.len(),
            selected.unwrap_or(usize::MAX),
            self.project_id.as_deref().unwrap_or_default()
        ));
        let menu = window.use_keyed_state(key, cx, move |window, cx| {
            ContextMenu::new(window, cx, move |mut menu, _, _| {
                for (index, (label, choose)) in entries.into_iter().enumerate() {
                    let this = this.clone();
                    menu = menu.toggleable_entry(
                        label,
                        selected == Some(index),
                        IconPosition::End,
                        None,
                        move |_, cx| {
                            let choose = choose.clone();
                            this.update(cx, |this, cx| choose(this, cx)).ok();
                        },
                    );
                }
                menu
            })
        });
        div()
            .id(id)
            .debug_selector(move || id.to_string())
            .when_some(blocked.clone(), |this, reason| {
                this.tooltip(Tooltip::text(reason))
            })
            .child(
                DropdownMenu::new(id, label, menu)
                    .style(DropdownStyle::Outlined)
                    .disabled(blocked.is_some() || self.saving.is_some()),
            )
            .into_any_element()
    }

    fn render_header(
        &self,
        project: Option<&Value>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let projects = self
            .shells
            .read(cx)
            .shell()
            .map(|shell| {
                let mut projects = shell
                    .projects
                    .values()
                    .map(|project| (project.id.clone(), project.title.clone()))
                    .collect::<Vec<_>>();
                projects.sort_by_cached_key(|(id, title)| (title.to_lowercase(), id.clone()));
                projects
            })
            .unwrap_or_default();
        let current = project.and_then(|project| project.get("id")?.as_str().map(str::to_string));
        let selected = projects
            .iter()
            .position(|(id, _)| Some(id) == current.as_ref());
        let label = project
            .and_then(|project| project.get("title")?.as_str().map(str::to_string))
            .unwrap_or_else(|| "Choose a project".into());
        let entries = projects
            .into_iter()
            .map(|(id, title)| {
                let choose: Rc<dyn Fn(&mut Self, &mut Context<Self>)> =
                    Rc::new(move |this: &mut Self, cx: &mut Context<Self>| {
                        this.project_id = Some(id.clone());
                        this.error = None;
                        this.pending_name_refresh(cx);
                    });
                (title, choose)
            })
            .collect::<Vec<_>>();
        let picker = self.dropdown(
            "project-settings-project",
            label,
            entries,
            selected,
            None,
            window,
            cx,
        );
        let root =
            project.and_then(|project| project.get("workspaceRoot")?.as_str().map(str::to_string));
        h_flex()
            .w_full()
            .flex_wrap()
            .gap_3()
            .items_start()
            .child(
                v_flex()
                    .flex_1()
                    .min_w(scaled(px(260.)))
                    .gap_0p5()
                    .child(Headline::new("Project settings").size(HeadlineSize::Large))
                    .child(muted(match root {
                        Some(root) => format!("{root} · T3 Code's app shares these settings."),
                        None => "Choose a project to change its name and the defaults of its \
                                 new threads."
                            .into(),
                    })),
            )
            .child(div().flex_none().child(picker))
            .into_any_element()
    }

    /// Refills the name once the page has the new project (from a menu, without a window).
    fn pending_name_refresh(&mut self, cx: &mut Context<Self>) {
        let title = self
            .project(cx)
            .and_then(|project| project.get("title")?.as_str().map(str::to_string));
        if let Some(title) = title {
            self.name.update(cx, |editor, cx| {
                editor.buffer().update(cx, |buffer, cx| {
                    if let Some(buffer) = buffer.as_singleton() {
                        buffer.update(cx, |buffer, cx| buffer.set_text(title, cx));
                    }
                })
            });
        }
        cx.notify();
    }

    fn render_name(&self, project: &Value, cx: &mut Context<Self>) -> AnyElement {
        let palette = palette(cx);
        let title = project
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let typed = self.name.read(cx).text(cx);
        let changed = !typed.trim().is_empty() && typed.trim() != title;
        let blocked = self.blocked(Access::Operate);
        let this = cx.entity().downgrade();
        let control = h_flex()
            .gap_2()
            .child(
                div()
                    .w(scaled(px(240.)))
                    .px_2()
                    .py_1()
                    .rounded(scaled(tokens::CONTROL_RADIUS))
                    .border(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
                    .bg(palette.background.hsla())
                    .key_context("MustrProjectName")
                    .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| this.rename(cx)))
                    .child(self.name.clone()),
            )
            .child(
                div()
                    .debug_selector(|| "project-settings-save-name".into())
                    .child(
                        TextButton::new("project-settings-save-name", "Save")
                            .style(ButtonStyle::Primary)
                            .disabled(!changed || blocked.is_some() || self.saving.is_some())
                            .when_some(blocked, |button, reason| button.tooltip(reason))
                            .on_click(move |_, _, cx| {
                                this.update(cx, |this, cx| this.rename(cx)).ok();
                            }),
                    ),
            );
        section(
            "Project",
            None,
            vec![row(
                "project-settings-row-name",
                "Name",
                "How the project shows in mustr and T3 Code's app.",
                control,
                true,
                cx,
            )],
        )
    }

    fn render_new_threads(
        &self,
        settings: &Value,
        project: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let project_id = project
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let blocked = self.blocked(Access::SettingsWrite);

        // The model.
        let providers = self
            .configs
            .read(cx)
            .providers()
            .map(<[_]>::to_vec)
            .unwrap_or_default();
        let agents = agent_choices(&providers);
        let own = project_value(settings, Some(project), project_id, "defaultModelSelection")
            .and_then(|value| serde_json::from_value::<ModelSelection>(value.clone()).ok());
        let global = settings
            .get("defaultModelSelection")
            .filter(|value| !value.is_null())
            .and_then(|value| serde_json::from_value::<ModelSelection>(value.clone()).ok());
        let model_name = |selection: &ModelSelection| {
            let instance = selection_instance_id(selection);
            let model = selection_model(selection);
            agents
                .iter()
                .find(|agent| Some(agent.instance_id.as_str()) == instance)
                .and_then(|agent| {
                    let found = agent
                        .models
                        .iter()
                        .find(|choice| Some(choice.slug.as_str()) == model)?;
                    Some(format!("{} · {}", agent.name, found.name))
                })
                .or_else(|| model.map(str::to_string))
                .unwrap_or_else(|| "a model".into())
        };
        let default_label = match &global {
            Some(global) => format!("T3 Code's default ({})", model_name(global)),
            None => "T3 Code's default".to_string(),
        };
        let mut entries: Vec<(String, Rc<dyn Fn(&mut Self, &mut Context<Self>)>)> = vec![(
            default_label.clone(),
            Rc::new(|this: &mut Self, cx: &mut Context<Self>| {
                this.set_override("defaultModelSelection", None, cx)
            }),
        )];
        let mut selected = own.is_none().then_some(0);
        for agent in &agents {
            for model in &agent.models {
                let label = format!("{} · {}", agent.name, model.name);
                if let Some(own) = &own
                    && selection_instance_id(own) == Some(agent.instance_id.as_str())
                    && selection_model(own) == Some(model.slug.as_str())
                {
                    selected = Some(entries.len());
                }
                let value = json!({ "instanceId": agent.instance_id, "model": model.slug });
                entries.push((
                    label,
                    Rc::new(move |this: &mut Self, cx: &mut Context<Self>| {
                        this.set_override("defaultModelSelection", Some(value.clone()), cx)
                    }),
                ));
            }
        }
        let label = match &own {
            Some(own) => model_name(own),
            None => default_label,
        };
        let model = self.dropdown(
            "project-settings-model",
            label,
            entries,
            selected,
            blocked.clone(),
            window,
            cx,
        );

        // The workspace.
        let own_mode = project_value(settings, Some(project), project_id, "defaultThreadEnvMode")
            .and_then(|value| serde_json::from_value::<ThreadEnvMode>(value.clone()).ok());
        let selected_mode = match own_mode {
            None => 0,
            Some(ThreadEnvMode::Worktree) => 2,
            Some(_) => 1,
        };
        let global_mode = match settings.get("defaultThreadEnvMode").and_then(Value::as_str) {
            Some("worktree") => "a new worktree",
            _ => "the checkout",
        };
        let this = cx.entity().downgrade();
        let disabled = blocked.is_some() || self.saving.is_some();
        let workspace = div()
            .id("project-settings-workspace-wrapper")
            .debug_selector(|| "project-settings-workspace".into())
            .when(disabled, |this| this.opacity(0.55))
            .when_some(blocked, |this, reason| this.tooltip(Tooltip::text(reason)))
            .child(
                SegmentedControl::new("project-settings-workspace")
                    .segment("T3's default")
                    .segment("Checkout")
                    .segment("Worktree")
                    .selected(selected_mode)
                    .on_select(move |index, _, cx| {
                        if disabled || index == selected_mode {
                            return;
                        }
                        let value = match index {
                            1 => Some(json!("local")),
                            2 => Some(json!("worktree")),
                            _ => None,
                        };
                        this.update(cx, |this, cx| {
                            this.set_override("defaultThreadEnvMode", value, cx)
                        })
                        .ok();
                    }),
            )
            .into_any_element();

        section(
            "New threads",
            Some("Override T3 Code's settings for this project"),
            vec![
                row(
                    "project-settings-row-model",
                    "Model",
                    "The model the project's new threads start with.",
                    model,
                    true,
                    cx,
                ),
                row(
                    "project-settings-row-workspace",
                    "Workspace",
                    format!(
                        "Where the project's new threads work. T3 Code's default is {global_mode}."
                    ),
                    workspace,
                    false,
                    cx,
                ),
            ],
        )
    }

    fn render_scripts(&self, project: &Value, cx: &mut Context<Self>) -> Option<AnyElement> {
        let scripts = project.get("scripts")?.as_array()?;
        if scripts.is_empty() {
            return None;
        }
        let palette = palette(cx);
        let rows = scripts
            .iter()
            .enumerate()
            .filter_map(|(index, script)| {
                let name = script.get("name")?.as_str()?.to_string();
                let command = script.get("command")?.as_str()?.to_string();
                Some(
                    h_flex()
                        .w_full()
                        .gap_3()
                        .px_2()
                        .py(scaled(px(7.)))
                        .when(index > 0, |this| {
                            this.border_t(tokens::BORDER_WIDTH)
                                .border_color(palette.border.hsla())
                        })
                        .child(
                            Icon::new(IconName::PlayOutlined)
                                .size(ui::IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(Label::new(name))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .font_family(tokens::CODE_FONT_FAMILY)
                                .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                                .text_color(palette.text_muted.hsla())
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(command),
                        )
                        .into_any_element(),
                )
            })
            .collect::<Vec<_>>();
        Some(
            v_flex()
                .w_full()
                .gap_1p5()
                .child(
                    h_flex()
                        .gap_2()
                        .items_baseline()
                        .child(Label::new("Scripts").weight(gpui::FontWeight::SEMIBOLD))
                        .child(
                            muted("Run them from the terminal panel's Run ▾")
                                .size(LabelSize::XSmall),
                        ),
                )
                .child(
                    Card::new()
                        .px(scaled(px(10.)))
                        .py(scaled(px(2.)))
                        .children(rows),
                )
                .into_any_element(),
        )
    }
}

impl EventEmitter<ItemEvent> for ProjectSettingsView {}

impl Focusable for ProjectSettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for ProjectSettingsView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Project settings".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Folder))
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for ProjectSettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let project = self.project(cx);
        let header = self.render_header(project.as_ref(), window, cx);
        let mut body: Vec<AnyElement> = Vec::new();
        match (&project, &self.settings) {
            (None, _) => body.push(
                Card::new()
                    .child(muted(if self.shells.read(cx).shell().is_some() {
                        "There's no such project in T3 Code. Choose one above."
                    } else {
                        "Loading the projects…"
                    }))
                    .into_any_element(),
            ),
            (Some(project), settings) => {
                body.push(self.render_name(project, cx));
                match settings {
                    Loadable::Loading => body.push(
                        Card::new()
                            .child(muted("Loading T3 Code's settings…"))
                            .into_any_element(),
                    ),
                    Loadable::Failed(error) => body.push(
                        Card::new()
                            .child(
                                Label::new(error.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Error),
                            )
                            .into_any_element(),
                    ),
                    Loadable::Loaded(settings) => {
                        let settings = settings.clone();
                        body.push(self.render_new_threads(&settings, project, window, cx));
                    }
                }
                body.extend(self.render_scripts(project, cx));
            }
        }
        let notice = self
            .blocked(Access::SettingsWrite)
            .map(|reason| format!("{reason}, so the new-thread settings are read-only here."));
        v_flex()
            .key_context("MustrProjectSettings")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(palette.background.hsla())
            .child(
                v_flex()
                    .id("project-settings-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(scaled(px(CONTENT_MAX_WIDTH)))
                            .mx_auto()
                            .px_6()
                            .py_6()
                            .gap_5()
                            .child(header)
                            .when_some(notice.filter(|_| project.is_some()), |this, notice| {
                                this.child(
                                    div()
                                        .debug_selector(|| "project-settings-read-only".into())
                                        .child(muted(notice)),
                                )
                            })
                            .when_some(self.error.clone(), |this, error| {
                                this.child(
                                    div()
                                        .debug_selector(|| "project-settings-error".into())
                                        .child(
                                            Label::new(error)
                                                .size(LabelSize::Small)
                                                .color(Color::Error),
                                        ),
                                )
                            })
                            .children(body),
                    ),
            )
    }
}

/// For tests: the error a write left, if any.
#[cfg(test)]
pub(crate) fn error_of(view: &ProjectSettingsView) -> Option<&str> {
    view.error.as_deref()
}
