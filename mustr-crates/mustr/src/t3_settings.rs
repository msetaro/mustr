//! T3 Code's settings for agent threads (`mustr-docs/PARITY.md` §2.17, "T3 settings"), as T3's
//! settings panels have them (`settings/ProjectDefaultsSettings.tsx`, the Follow-ups and
//! Auto-settle sections of `SettingsPanels.tsx`, and `BranchNamingSettings.tsx`):
//! - **New threads:** the permission mode, where a thread works (the project's checkout or a new
//!   worktree), whether new worktrees start from the remote, and the model.
//! - **Follow-ups:** whether Enter queues a message or steers the agent while it works. T3 keeps
//!   this one in its own app rather than on its server, so mustr keeps its own
//!   ([`FollowUpBehavior`]).
//! - **Auto-settle:** when a thread's pull request merges, after days without activity, and
//!   whether threads with a pull request are kept.
//! - **Branch naming:** a fixed prefix, a prefix the model picks, or a name from instructions.
//!
//! The settings come from `server.getSettings`, or from `server.getConfig` on servers that dropped
//! it, and then from T3's `settingsUpdated` events ([`ServerConfigs`]). A change is sent as a
//! patch of what changed with `server.updateSettings` at `settings:write`, so dry runs and
//! pairings without that scope refuse it; on a server without the call, the page is read-only.

#[cfg(test)]
mod tests;

use crate::nav_panel::register_rail_entry;
use anyhow::{Context as _, Result};
use editor::{Editor, EditorEvent};
use futures::{FutureExt as _, future::BoxFuture};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Task, Window, div, px,
};
use mustr_ui::{ButtonStyle, Card, SegmentedControl, TextButton, palette, tokens, tokens::scaled};
use serde_json::{Value, json};
use std::{rc::Rc, sync::Arc};
use t3_client::{
    RpcMethod as _,
    t3_api::{
        rpc,
        types::{BranchNamingMode, RuntimeMode, ServerSettings, ThreadEnvMode},
    },
};
use thread_chat::{
    Access, ServerConfigs, ThreadBackend,
    thread_settings::{
        DEFAULT_RUNTIME_MODE, FollowUpBehavior, RUNTIME_MODES, agent_choices, load_server_settings,
        runtime_mode_description, runtime_mode_label,
    },
};
use ui::{
    Color, ContextMenu, DropdownMenu, DropdownStyle, Headline, HeadlineSize, Icon, IconName,
    IconPosition, Label, LabelCommon as _, LabelSize, Switch, ToggleState, Tooltip, prelude::*,
};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

const CONTENT_MAX_WIDTH: f32 = 880.;
/// T3's default for settling inactive threads (`DEFAULT_SIDEBAR_AUTO_SETTLE_AFTER_DAYS`).
const AUTO_SETTLE_DEFAULT_DAYS: u32 = 3;
/// The waits offered for settling inactive threads, within T3's 1 to 90 days.
const AUTO_SETTLE_DAYS: [u32; 9] = [1, 2, 3, 5, 7, 14, 30, 60, 90];
/// T3's default branch prefix.
const DEFAULT_BRANCH_PREFIX: &str = "t3";

gpui::actions!(
    mustr,
    [
        /// Opens T3 Code's settings for agent threads: new-thread defaults, follow-ups,
        /// auto-settle, and branch naming.
        OpenT3Settings,
    ]
);

/// Adds the page's rail entry and action. Call once at startup, before windows open.
pub fn init(cx: &mut App) {
    register_rail_entry(
        "t3-settings",
        Icon::new(IconName::Server),
        "T3 Code settings",
        OpenT3Settings,
        |item| item.downcast::<T3SettingsView>().is_some(),
        cx,
    );
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenT3Settings, window, cx| {
            show(workspace, window, cx)
        });
    })
    .detach();
}

/// Activates the settings page, or opens it.
pub fn show(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let existing = workspace
        .panes()
        .iter()
        .find_map(|pane| pane.read(cx).items_of_type::<T3SettingsView>().next());
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let backend = thread_chat::backend(cx);
    let view = cx.new(|cx| T3SettingsView::new(backend, window, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

// The settings, as the page shows them: T3's value, or T3's default where it sends none.

pub(crate) fn runtime_mode(settings: &ServerSettings) -> RuntimeMode {
    settings
        .default_runtime_mode
        .clone()
        .unwrap_or(DEFAULT_RUNTIME_MODE)
}

pub(crate) fn auto_settle_days(settings: &ServerSettings) -> Option<u32> {
    settings
        .sidebar_auto_settle_after_days
        .map(|days| f64::from(days).round().clamp(1., 90.) as u32)
}

pub(crate) fn branch_naming_mode(settings: &ServerSettings) -> BranchNamingMode {
    settings
        .branch_naming_mode
        .clone()
        .unwrap_or(BranchNamingMode::Static)
}

pub(crate) fn branch_prefix(settings: &ServerSettings) -> String {
    settings
        .branch_name_prefix
        .clone()
        .unwrap_or_else(|| DEFAULT_BRANCH_PREFIX.to_string())
}

/// `settings` with `patch` applied, for showing a change before T3 confirms it. The patch's keys
/// are the settings' own, so it merges key by key.
pub(crate) fn patched(settings: &ServerSettings, patch: &Value) -> Option<ServerSettings> {
    let mut value = serde_json::to_value(settings).ok()?;
    let (Value::Object(target), Value::Object(changes)) = (&mut value, patch) else {
        return None;
    };
    for (key, change) in changes {
        target.insert(key.clone(), change.clone());
    }
    serde_json::from_value(value).ok()
}

fn wire(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

enum Loadable<T> {
    Loading,
    Failed(SharedString),
    Loaded(T),
}

impl<T> Loadable<T> {
    fn loaded(&self) -> Option<&T> {
        match self {
            Self::Loaded(value) => Some(value),
            _ => None,
        }
    }
}

/// A patch the page can send, by what it changes.
type Patch = (&'static str, Value);

pub struct T3SettingsView {
    focus_handle: FocusHandle,
    backend: Option<Arc<dyn ThreadBackend>>,
    configs: Entity<ServerConfigs>,
    settings: Loadable<ServerSettings>,
    /// The server doesn't have `server.updateSettings`.
    write_unsupported: bool,
    /// The setting being saved.
    saving: Option<&'static str>,
    error: Option<SharedString>,
    prefix: Entity<Editor>,
    instructions: Entity<Editor>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl T3SettingsView {
    pub(crate) fn new(
        backend: Option<Arc<dyn ThreadBackend>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let configs = ServerConfigs::global(cx);
        let prefix = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("No prefix", window, cx);
            editor
        });
        let instructions = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 8, window, cx);
            editor.set_placeholder_text(
                "Use julius/ followed by the issue ID and a short description.",
                window,
                cx,
            );
            editor
        });
        let subscriptions = vec![
            cx.observe_in(&configs, window, |this, configs, window, cx| {
                // T3's `settingsUpdated`, from this page or another client.
                if let Some(settings) = configs.read(cx).settings().cloned()
                    && matches!(this.settings, Loadable::Loaded(_))
                    && this.saving.is_none()
                {
                    this.settings_changed(settings, window, cx);
                }
            }),
            cx.subscribe(&prefix, |_, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    cx.notify();
                }
            }),
            cx.subscribe(&instructions, |_, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    cx.notify();
                }
            }),
        ];
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            backend,
            configs,
            settings: Loadable::Loading,
            write_unsupported: false,
            saving: None,
            error: None,
            prefix,
            instructions,
            _tasks: Vec::new(),
            _subscriptions: subscriptions,
        };
        this.load(window, cx);
        this
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            self.settings = Loadable::Failed("mustr isn't connected to T3 Code".into());
            return;
        };
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = load_server_settings(&backend).await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(settings) => {
                        this.fill_editors(&settings, window, cx);
                        this.settings = Loadable::Loaded(settings);
                    }
                    Err(error) => {
                        log::warn!("couldn't read T3's settings: {error:#}");
                        this.settings = Loadable::Failed(
                            format!("Couldn't read T3 Code's settings: {error:#}").into(),
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Takes settings from T3, refilling an editor unless the user has changed what it holds.
    fn settings_changed(
        &mut self,
        settings: ServerSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Loadable::Loaded(old) = &self.settings {
            let old_prefix = branch_prefix(old);
            if self.prefix.read(cx).text(cx).trim() == old_prefix.trim() {
                let prefix = branch_prefix(&settings);
                self.prefix.update(cx, |editor, cx| {
                    set_editor_text(editor, &prefix, window, cx)
                });
            }
            let old_instructions = old.branch_name_instructions.clone().unwrap_or_default();
            if self.instructions.read(cx).text(cx).trim() == old_instructions.trim() {
                let instructions = settings
                    .branch_name_instructions
                    .clone()
                    .unwrap_or_default();
                self.instructions.update(cx, |editor, cx| {
                    set_editor_text(editor, &instructions, window, cx)
                });
            }
        }
        self.settings = Loadable::Loaded(settings);
        cx.notify();
    }

    fn fill_editors(&mut self, settings: &ServerSettings, window: &mut Window, cx: &mut App) {
        let prefix = branch_prefix(settings);
        let instructions = settings
            .branch_name_instructions
            .clone()
            .unwrap_or_default();
        self.prefix
            .update(cx, |editor, cx| editor.set_text(prefix, window, cx));
        self.instructions
            .update(cx, |editor, cx| editor.set_text(instructions, window, cx));
    }

    #[cfg(test)]
    fn settings(&self) -> Option<&ServerSettings> {
        match &self.settings {
            Loadable::Loaded(settings) => Some(settings),
            _ => None,
        }
    }

    /// Why changes can't be saved, if they can't.
    fn blocked(&self) -> Option<String> {
        if self.write_unsupported {
            return Some("This T3 Code can't change its settings from mustr".into());
        }
        match &self.backend {
            Some(backend) => backend.blocked_reason(Access::SettingsWrite),
            None => Some("mustr isn't connected to T3 Code".into()),
        }
    }

    /// Sends `patch` with `server.updateSettings`, showing it at once and T3's settings once it
    /// answers; a failure puts the old settings back.
    pub(crate) fn write(&mut self, (key, patch): Patch, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        let Loadable::Loaded(before) = &self.settings else {
            return;
        };
        if self.saving.is_some() {
            return;
        }
        let before = before.clone();
        if let Some(after) = patched(&before, &patch) {
            self.settings = Loadable::Loaded(after);
        }
        self.saving = Some(key);
        self.error = None;
        let call = update_settings(backend.as_ref(), patch);
        cx.notify();
        let configs = self.configs.clone();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                this.saving = None;
                match result {
                    Ok(settings) => {
                        configs
                            .update(cx, |configs, cx| configs.set_settings(settings.clone(), cx));
                        this.settings = Loadable::Loaded(settings);
                    }
                    Err(error) => {
                        this.settings = Loadable::Loaded(before);
                        if thread_chat::is_unsupported_rpc(&error) {
                            this.write_unsupported = true;
                        } else {
                            log::warn!("couldn't change T3's settings: {error:#}");
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

    fn save_prefix(&mut self, cx: &mut Context<Self>) {
        let prefix = self.prefix.read(cx).text(cx).trim().to_string();
        self.write(
            ("branchNamePrefix", json!({ "branchNamePrefix": prefix })),
            cx,
        );
    }

    fn save_instructions(&mut self, cx: &mut Context<Self>) {
        let instructions = self.instructions.read(cx).text(cx).trim().to_string();
        self.write(
            (
                "branchNameInstructions",
                json!({ "branchNameInstructions": instructions }),
            ),
            cx,
        );
    }
}

/// `server.updateSettings` with `patch`, as JSON: T3's patch takes `null` to clear a setting,
/// which the generated patch type can't say.
fn update_settings(
    backend: &dyn ThreadBackend,
    patch: Value,
) -> BoxFuture<'static, Result<ServerSettings>> {
    let call = backend.call_raw(
        rpc::ServerUpdateSettings::TAG,
        json!({ "patch": patch }),
        Access::SettingsWrite,
    );
    async move {
        let settings = call.await?;
        serde_json::from_value(settings).context("couldn't read T3 Code's settings")
    }
    .boxed()
}

fn set_editor_text(editor: &mut Editor, text: &str, window: &mut Window, cx: &mut Context<Editor>) {
    if editor.text(cx) != text {
        editor.set_text(text, window, cx);
    }
}

// Rendering.

fn muted(text: impl Into<SharedString>) -> Label {
    Label::new(text.into())
        .size(LabelSize::Small)
        .color(Color::Muted)
}

/// A setting: its name and what it does, with its control beside them, or under them when the
/// page is narrow (`SettingsRow` in T3).
fn row(
    selector: &'static str,
    title: &'static str,
    description: impl Into<SharedString>,
    control: impl IntoElement,
    first: bool,
    cx: &App,
) -> AnyElement {
    let palette = palette(cx);
    h_flex()
        .debug_selector(move || selector.to_string())
        .w_full()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_x_4()
        .gap_y_1()
        .px_2()
        .py(scaled(px(9.)))
        .when(!first, |this| {
            this.border_t(tokens::BORDER_WIDTH)
                .border_color(palette.border.hsla())
        })
        .child(
            v_flex()
                .flex_1()
                .min_w(scaled(px(260.)))
                .gap_0p5()
                .child(Label::new(title).weight(FontWeight::MEDIUM))
                .child(muted(description)),
        )
        .child(div().flex_none().child(control))
        .into_any_element()
}

fn section(title: &'static str, note: Option<&'static str>, rows: Vec<AnyElement>) -> AnyElement {
    v_flex()
        .w_full()
        .gap_1p5()
        .child(
            h_flex()
                .gap_2()
                .items_baseline()
                .child(Label::new(title).weight(FontWeight::SEMIBOLD))
                .children(note.map(|note| muted(note).size(LabelSize::XSmall))),
        )
        .child(
            Card::new()
                .px(scaled(px(10.)))
                .py(scaled(px(2.)))
                .children(rows),
        )
        .into_any_element()
}

impl T3SettingsView {
    fn dropdown(
        &self,
        id: &'static str,
        label: String,
        entries: Vec<(String, Patch)>,
        selected: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let key = SharedString::from(format!(
            "{id}-{}-{}",
            entries.len(),
            selected.unwrap_or(usize::MAX)
        ));
        let menu = window.use_keyed_state(key, cx, move |window, cx| {
            ContextMenu::new(window, cx, move |mut menu, _, _| {
                for (index, (label, patch)) in entries.into_iter().enumerate() {
                    let this = this.clone();
                    menu = menu.toggleable_entry(
                        label,
                        selected == Some(index),
                        IconPosition::End,
                        None,
                        move |_, cx| {
                            let patch = patch.clone();
                            this.update(cx, |this, cx| this.write(patch, cx)).ok();
                        },
                    );
                }
                menu
            })
        });
        let blocked = self.blocked();
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

    fn segmented(
        &self,
        id: &'static str,
        segments: &[&'static str],
        selected: usize,
        on_select: impl Fn(&mut Self, usize, &mut Context<Self>) + 'static,
        blocked: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let on_select = Rc::new(on_select);
        let disabled = blocked.is_some();
        let control = segments
            .iter()
            .fold(SegmentedControl::new(id), |control, segment| {
                control.segment(*segment)
            })
            .selected(selected)
            .on_select(move |index, _, cx| {
                if disabled || index == selected {
                    return;
                }
                let on_select = on_select.clone();
                this.update(cx, |this, cx| on_select(this, index, cx)).ok();
            });
        div()
            .id(SharedString::from(format!("{id}-wrapper")))
            .debug_selector(move || id.to_string())
            .when(disabled, |this| this.opacity(0.55))
            .when_some(blocked, |this, reason| this.tooltip(Tooltip::text(reason)))
            .child(control)
            .into_any_element()
    }

    fn switch(
        &self,
        id: &'static str,
        on: bool,
        patch: impl Fn(bool) -> Patch + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let blocked = self.blocked();
        div()
            .id(SharedString::from(format!("{id}-wrapper")))
            .debug_selector(move || id.to_string())
            .when_some(blocked.clone(), |this, reason| {
                this.tooltip(Tooltip::text(reason))
            })
            .child(
                Switch::new(
                    id,
                    if on {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .disabled(blocked.is_some() || self.saving.is_some())
                .on_click(move |_, _, cx| {
                    let patch = patch(!on);
                    this.update(cx, |this, cx| this.write(patch, cx)).ok();
                }),
            )
            .into_any_element()
    }

    fn save_button(
        &self,
        id: &'static str,
        changed: bool,
        on_click: fn(&mut Self, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let blocked = self.blocked();
        div()
            .debug_selector(move || id.to_string())
            .child(
                TextButton::new(id, "Save")
                    .style(ButtonStyle::Primary)
                    .disabled(!changed || blocked.is_some() || self.saving.is_some())
                    .when_some(blocked, |button, reason| button.tooltip(reason))
                    .on_click(move |_, _, cx| {
                        this.update(cx, |this, cx| on_click(this, cx)).ok();
                    }),
            )
            .into_any_element()
    }

    fn render_new_threads(
        &self,
        settings: &ServerSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mode = runtime_mode(settings);
        let modes = RUNTIME_MODES
            .iter()
            .map(|mode| {
                (
                    runtime_mode_label(mode).to_string(),
                    (
                        "defaultRuntimeMode",
                        json!({ "defaultRuntimeMode": wire(mode) }),
                    ),
                )
            })
            .collect::<Vec<_>>();
        let permission = self.dropdown(
            "t3-settings-runtime-mode",
            runtime_mode_label(&mode).to_string(),
            modes,
            RUNTIME_MODES.iter().position(|choice| *choice == mode),
            window,
            cx,
        );

        let env_mode = settings.default_thread_env_mode.clone();
        let workspace = self.segmented(
            "t3-settings-workspace",
            &["Current checkout", "New worktree"],
            match env_mode {
                Some(ThreadEnvMode::Worktree) => 1,
                _ => 0,
            },
            |this, index, cx| {
                let mode = if index == 1 { "worktree" } else { "local" };
                this.write(
                    (
                        "defaultThreadEnvMode",
                        json!({ "defaultThreadEnvMode": mode }),
                    ),
                    cx,
                )
            },
            self.blocked()
                .or_else(|| self.saving.map(|_| "Saving".into())),
            cx,
        );

        let from_origin = settings.new_worktrees_start_from_origin.unwrap_or(true);
        let origin = self.switch(
            "t3-settings-from-origin",
            from_origin,
            |on| {
                (
                    "newWorktreesStartFromOrigin",
                    json!({ "newWorktreesStartFromOrigin": on }),
                )
            },
            cx,
        );

        let providers = self
            .configs
            .read(cx)
            .providers()
            .map(<[_]>::to_vec)
            .unwrap_or_default();
        let agents = agent_choices(&providers);
        let selection = settings.default_model_selection.as_ref();
        let selected_instance = selection
            .and_then(thread_chat::thread_settings::selection_instance_id)
            .map(str::to_string);
        let selected_model = selection
            .and_then(thread_chat::thread_settings::selection_model)
            .map(str::to_string);
        let mut models: Vec<(String, Patch)> = vec![(
            "Chosen by T3 Code".to_string(),
            (
                "defaultModelSelection",
                json!({ "defaultModelSelection": null }),
            ),
        )];
        let mut selected = selection.is_none().then_some(0);
        let mut label = "Chosen by T3 Code".to_string();
        for agent in &agents {
            for model in &agent.models {
                if selected_instance.as_deref() == Some(agent.instance_id.as_str())
                    && selected_model.as_deref() == Some(model.slug.as_str())
                {
                    selected = Some(models.len());
                    label = format!("{} · {}", agent.name, model.name);
                }
                models.push((
                    format!("{} · {}", agent.name, model.name),
                    (
                        "defaultModelSelection",
                        json!({ "defaultModelSelection": {
                            "instanceId": agent.instance_id,
                            "model": model.slug,
                        } }),
                    ),
                ));
            }
        }
        if selected.is_none()
            && let Some(model) = &selected_model
        {
            label = model.clone();
        }
        let model = self.dropdown("t3-settings-model", label, models, selected, window, cx);

        section(
            "New threads",
            None,
            vec![
                row(
                    "t3-settings-row-model",
                    "Model",
                    if selection.is_none() {
                        "The model new threads start with. T3 Code picks one when none is set; \
                         projects can choose their own."
                    } else {
                        "The model new threads start with. Projects can choose their own."
                    },
                    model,
                    true,
                    cx,
                ),
                row(
                    "t3-settings-row-runtime-mode",
                    "Permissions",
                    format!(
                        "What agents may do in new threads without asking. {}",
                        runtime_mode_description(&mode)
                    ),
                    permission,
                    false,
                    cx,
                ),
                row(
                    "t3-settings-row-workspace",
                    "Workspace",
                    "Where new threads work: the project's checkout, or a new worktree. Projects \
                     and their t3.json can choose their own.",
                    workspace,
                    false,
                    cx,
                ),
                row(
                    "t3-settings-row-from-origin",
                    "Start worktrees from the remote",
                    "New worktrees start from the base branch on the remote, not your local copy.",
                    origin,
                    false,
                    cx,
                ),
            ],
        )
    }

    fn render_follow_ups(&self, cx: &mut Context<Self>) -> AnyElement {
        let behavior = FollowUpBehavior::get(cx);
        let control = self.segmented(
            "t3-settings-follow-ups",
            &["Queue", "Steer"],
            match behavior {
                FollowUpBehavior::Queue => 0,
                FollowUpBehavior::Steer => 1,
            },
            |_, index, cx| {
                let behavior = if index == 1 {
                    FollowUpBehavior::Steer
                } else {
                    FollowUpBehavior::Queue
                };
                behavior.set(cx);
                cx.notify();
            },
            None,
            cx,
        );
        let other = if cfg!(target_os = "macos") {
            "⌘⇧Enter"
        } else {
            "Ctrl+Shift+Enter"
        };
        section(
            "Follow-ups",
            Some("Kept by mustr"),
            vec![row(
                "t3-settings-row-follow-ups",
                "While the agent works",
                format!(
                    "Enter queues a message for after the run, or steers the run with it. {other} \
                     does the other."
                ),
                control,
                true,
                cx,
            )],
        )
    }

    fn render_auto_settle(
        &self,
        settings: &ServerSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let on_merge = settings.sidebar_auto_settle_on_merge.unwrap_or(true);
        let days = auto_settle_days(settings);
        let mut rows = vec![
            row(
                "t3-settings-row-settle-on-merge",
                "When the pull request merges",
                "Settle a thread when its pull request merges. Closed pull requests still settle.",
                self.switch(
                    "t3-settings-settle-on-merge",
                    on_merge,
                    |on| {
                        (
                            "sidebarAutoSettleOnMerge",
                            json!({ "sidebarAutoSettleOnMerge": on }),
                        )
                    },
                    cx,
                ),
                true,
                cx,
            ),
            row(
                "t3-settings-row-settle-inactive",
                "When inactive",
                "Settle threads with no activity for a while.",
                self.switch(
                    "t3-settings-settle-inactive",
                    days.is_some(),
                    |on| {
                        (
                            "sidebarAutoSettleAfterDays",
                            json!({ "sidebarAutoSettleAfterDays":
                                on.then_some(AUTO_SETTLE_DEFAULT_DAYS) }),
                        )
                    },
                    cx,
                ),
                false,
                cx,
            ),
        ];
        if let Some(days) = days {
            let mut choices = AUTO_SETTLE_DAYS.to_vec();
            if !choices.contains(&days) {
                choices.push(days);
                choices.sort_unstable();
            }
            let label = |days: u32| {
                if days == 1 {
                    "1 day".to_string()
                } else {
                    format!("{days} days")
                }
            };
            let entries = choices
                .iter()
                .map(|choice| {
                    (
                        label(*choice),
                        (
                            "sidebarAutoSettleAfterDays",
                            json!({ "sidebarAutoSettleAfterDays": choice }),
                        ),
                    )
                })
                .collect();
            rows.push(row(
                "t3-settings-row-settle-days",
                "After",
                "Any new activity un-settles a thread.",
                self.dropdown(
                    "t3-settings-settle-days",
                    label(days),
                    entries,
                    choices.iter().position(|choice| *choice == days),
                    window,
                    cx,
                ),
                false,
                cx,
            ));
            rows.push(row(
                "t3-settings-row-keep-pull-requests",
                "Keep threads with a pull request",
                "Inactivity doesn't settle threads with a pull request. Merging and closing \
                 still do.",
                self.switch(
                    "t3-settings-keep-pull-requests",
                    settings
                        .sidebar_auto_settle_keep_pull_requests
                        .unwrap_or(false),
                    |on| {
                        (
                            "sidebarAutoSettleKeepPullRequests",
                            json!({ "sidebarAutoSettleKeepPullRequests": on }),
                        )
                    },
                    cx,
                ),
                false,
                cx,
            ));
        }
        section("Auto-settle", None, rows)
    }

    fn render_branch_naming(
        &self,
        settings: &ServerSettings,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mode = branch_naming_mode(settings);
        let modes = [
            BranchNamingMode::Static,
            BranchNamingMode::Semantic,
            BranchNamingMode::Custom,
        ];
        let control = self.segmented(
            "t3-settings-branch-naming",
            &["Static prefix", "Semantic prefix", "Custom"],
            modes.iter().position(|choice| *choice == mode).unwrap_or(0),
            move |this, index, cx| {
                this.write(
                    (
                        "branchNamingMode",
                        json!({ "branchNamingMode": wire(&modes[index]) }),
                    ),
                    cx,
                )
            },
            self.blocked()
                .or_else(|| self.saving.map(|_| "Saving".into())),
            cx,
        );
        let mut rows = vec![row(
            "t3-settings-row-branch-naming",
            "Worktree branches",
            match mode {
                BranchNamingMode::Semantic => {
                    "Named from your first message. The model picks a prefix that describes the \
                     work, such as feat/add-search or fix/login-timeout."
                }
                BranchNamingMode::Custom => {
                    "Named from your first message by the model, following your instructions."
                }
                _ => "Named from your first message, after a fixed prefix.",
            },
            control,
            true,
            cx,
        )];
        let palette = palette(cx);
        let field = |editor: &Entity<Editor>| {
            div()
                .w(scaled(px(240.)))
                .px_2()
                .py_1()
                .rounded(scaled(tokens::CONTROL_RADIUS))
                .border(tokens::BORDER_WIDTH)
                .border_color(palette.border.hsla())
                .bg(palette.background.hsla())
                .child(editor.clone())
        };
        match mode {
            BranchNamingMode::Static => {
                let changed =
                    self.prefix.read(cx).text(cx).trim() != branch_prefix(settings).trim();
                rows.push(row(
                    "t3-settings-row-branch-prefix",
                    "Prefix",
                    "t3 or t3/ names a branch t3/add-search. Leave it empty for no prefix.",
                    h_flex()
                        .gap_1()
                        .child(
                            field(&self.prefix)
                                .key_context("MustrBranchPrefix")
                                .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| {
                                    let changed = this.settings.loaded().is_some_and(|settings| {
                                        this.prefix.read(cx).text(cx).trim()
                                            != branch_prefix(settings).trim()
                                    });
                                    if changed && this.blocked().is_none() {
                                        this.save_prefix(cx)
                                    }
                                })),
                        )
                        .child(self.save_button(
                            "t3-settings-save-prefix",
                            changed,
                            Self::save_prefix,
                            cx,
                        )),
                    false,
                    cx,
                ));
            }
            BranchNamingMode::Custom => {
                let saved = settings
                    .branch_name_instructions
                    .clone()
                    .unwrap_or_default();
                let changed = self.instructions.read(cx).text(cx).trim() != saved.trim();
                rows.push(
                    v_flex()
                        .debug_selector(|| "t3-settings-row-branch-instructions".into())
                        .w_full()
                        .gap_1()
                        .px_2()
                        .py(scaled(px(9.)))
                        .border_t(tokens::BORDER_WIDTH)
                        .border_color(palette.border.hsla())
                        .child(Label::new("Instructions").weight(FontWeight::MEDIUM))
                        .child(muted(
                            "Added to the naming prompt. The model returns the whole branch name; \
                             nothing is added before or after it.",
                        ))
                        .child(
                            div()
                                .w_full()
                                .px_2()
                                .py_1()
                                .rounded(scaled(tokens::CONTROL_RADIUS))
                                .border(tokens::BORDER_WIDTH)
                                .border_color(palette.border.hsla())
                                .bg(palette.background.hsla())
                                .child(self.instructions.clone()),
                        )
                        .child(h_flex().child(self.save_button(
                            "t3-settings-save-instructions",
                            changed,
                            Self::save_instructions,
                            cx,
                        )))
                        .into_any_element(),
                );
            }
            _ => {}
        }
        section("Branch naming", None, rows)
    }
}

impl EventEmitter<ItemEvent> for T3SettingsView {}

impl Focusable for T3SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for T3SettingsView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "T3 Code settings".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Server))
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for T3SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let environment = self
            .configs
            .read(cx)
            .environment()
            .map(|environment| environment.label.trim().to_string())
            .filter(|label| !label.is_empty());
        let header = v_flex()
            .w_full()
            .gap_0p5()
            .child(Headline::new("T3 Code settings").size(HeadlineSize::Large))
            .child(muted(match &environment {
                Some(environment) => format!(
                    "How T3 Code on {environment} starts, settles, and names agent threads. \
                     T3 Code's app shares these settings."
                ),
                None => "How T3 Code starts, settles, and names agent threads. T3 Code's app \
                         shares these settings."
                    .to_string(),
            }));
        let notice = if self.write_unsupported {
            Some("This T3 Code can't change its settings from mustr. Update it, or use T3 Code's app.".to_string())
        } else {
            self.backend
                .as_ref()
                .and_then(|backend| backend.blocked_reason(Access::SettingsWrite))
                .map(|reason| format!("{reason}, so these settings are read-only here."))
        };
        let body: Vec<AnyElement> = match &self.settings {
            Loadable::Loading => vec![
                Card::new()
                    .child(muted("Loading T3 Code's settings…"))
                    .into_any_element(),
            ],
            Loadable::Failed(error) => vec![
                Card::new()
                    .child(
                        Label::new(error.clone())
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    )
                    .into_any_element(),
            ],
            Loadable::Loaded(settings) => {
                let settings = settings.clone();
                vec![
                    self.render_new_threads(&settings, window, cx),
                    self.render_follow_ups(cx),
                    self.render_auto_settle(&settings, window, cx),
                    self.render_branch_naming(&settings, cx),
                ]
            }
        };
        v_flex()
            .key_context("MustrT3Settings")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(palette.background.hsla())
            .child(
                v_flex()
                    .id("t3-settings-scroll")
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
                            .when_some(notice, |this, notice| {
                                this.child(
                                    div()
                                        .debug_selector(|| "t3-settings-read-only".into())
                                        .child(muted(notice)),
                                )
                            })
                            .when_some(self.error.clone(), |this, error| {
                                this.child(
                                    div().debug_selector(|| "t3-settings-error".into()).child(
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
