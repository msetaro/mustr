//! A project's scripts (T3's "actions"), for the terminal panel's Run ▾ (`mustr-docs/PARITY.md`
//! §2.10): which apply, and adding, editing and removing them, as T3's
//! `ProjectScriptsControl.tsx` and `projectScriptEditor.tsx` do.
//!
//! - The scripts that apply are T3's `resolveProjectScripts`: the project's override in the
//!   server's settings (`projectSettingsOverrides[project].defaultProjectScripts`), then the
//!   legacy `projectScriptOverrides`, then the project's own (`scripts` in T3's shell), then the
//!   server's `defaultProjectScripts`.
//! - A change writes the whole list to the project's override (`server.updateSettings`, which
//!   needs `settings:write`), keeping the override's other settings, as T3's
//!   `useProjectScriptEditing.ts` does.
//! - One script runs when a worktree is made (optionally before the agent starts) and one when a
//!   thread settles (`runOnSettle`, which T3's server runs in the thread's worktree); saving one
//!   that claims a role takes it from the others (`releaseClaimedRoles`).
//! - A script's id is its name as a slug, numbered when it's taken (`nextProjectScriptId`), so
//!   T3's `script.<id>.run` keybindings keep working. mustr doesn't edit keybindings.

use anyhow::{Context as _, Result};
use editor::Editor;
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Global, IntoElement, ParentElement as _, Render, SharedString, Styled as _, Task, Window, div,
    px, rems,
};
use mustr_ui::{ButtonStyle, SegmentedControl, TextButton, palette, tokens, tokens::scaled};
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc};
use t3_client::t3_api::{
    RpcMethod as _, rpc,
    types::{OrchestrationProjectShell, ProjectScript, ProjectScriptIcon, ServerSettings},
};
use thread_chat::{Access, ThreadBackend};
use ui::{Color, Label, LabelCommon as _, LabelSize, Switch, ToggleState, prelude::*};
use workspace::ModalView;

/// T3's longest script id (`MAX_SCRIPT_ID_LENGTH`).
const MAX_SCRIPT_ID_LENGTH: usize = 24;

/// The scripts that apply to `project` (`resolveProjectScripts` in T3's
/// `packages/shared/src/projectScripts.ts`).
pub fn resolve_scripts(
    settings: &ServerSettings,
    project: &OrchestrationProjectShell,
) -> Vec<ProjectScript> {
    let defaults = || settings.default_project_scripts.clone().unwrap_or_default();
    if let Some(scripts) = settings
        .project_settings_overrides
        .as_ref()
        .and_then(|overrides| overrides.get(&project.id))
        .and_then(|project| project.default_project_scripts.clone())
    {
        return scripts;
    }
    if settings.project_settings_folded == Some(true) {
        return defaults();
    }
    match settings
        .project_script_overrides
        .as_ref()
        .and_then(|overrides| overrides.get(&project.id))
    {
        Some(None) => defaults(),
        Some(Some(scripts)) => scripts.clone(),
        None if !project.scripts.is_empty() => project.scripts.clone(),
        None => defaults(),
    }
}

/// The `server.updateSettings` patch that makes `scripts` the project's, keeping its override's
/// other settings.
pub fn scripts_patch(
    settings: &ServerSettings,
    project_id: &str,
    scripts: &[ProjectScript],
) -> Value {
    let mut project = settings
        .project_settings_overrides
        .as_ref()
        .and_then(|overrides| overrides.get(project_id))
        .and_then(|project| serde_json::to_value(project).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    project["defaultProjectScripts"] = json!(scripts);
    json!({ "projectSettingsOverrides": { project_id: project } })
}

/// What the editor asks for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScriptInput {
    pub name: String,
    pub command: String,
    pub icon: ScriptIcon,
    pub run_on_worktree_create: bool,
    /// The worktree's script finishes before the agent starts.
    pub wait_for_setup: bool,
    pub run_on_settle: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScriptIcon {
    #[default]
    Play,
    Test,
    Lint,
    Configure,
    Build,
    Debug,
}

impl ScriptIcon {
    pub const ALL: [Self; 6] = [
        Self::Play,
        Self::Test,
        Self::Lint,
        Self::Configure,
        Self::Build,
        Self::Debug,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Play => "Play",
            Self::Test => "Test",
            Self::Lint => "Lint",
            Self::Configure => "Configure",
            Self::Build => "Build",
            Self::Debug => "Debug",
        }
    }

    fn wire(self) -> ProjectScriptIcon {
        match self {
            Self::Play => ProjectScriptIcon::Play,
            Self::Test => ProjectScriptIcon::Test,
            Self::Lint => ProjectScriptIcon::Lint,
            Self::Configure => ProjectScriptIcon::Configure,
            Self::Build => ProjectScriptIcon::Build,
            Self::Debug => ProjectScriptIcon::Debug,
        }
    }

    fn of(icon: &ProjectScriptIcon) -> Self {
        match icon {
            ProjectScriptIcon::Test => Self::Test,
            ProjectScriptIcon::Lint => Self::Lint,
            ProjectScriptIcon::Configure => Self::Configure,
            ProjectScriptIcon::Build => Self::Build,
            ProjectScriptIcon::Debug => Self::Debug,
            ProjectScriptIcon::Play | ProjectScriptIcon::Unrecognized(_) => Self::Play,
        }
    }
}

impl ScriptInput {
    pub fn of(script: &ProjectScript) -> Self {
        Self {
            name: script.name.clone(),
            command: script.command.clone(),
            icon: ScriptIcon::of(&script.icon),
            run_on_worktree_create: script.run_on_worktree_create,
            wait_for_setup: script.async_ == Some(false),
            run_on_settle: script.run_on_settle == Some(true),
        }
    }
}

/// A script from the editor's input (`buildProjectScript`): `async: false` only for a worktree's
/// script that the agent waits for, and `runOnSettle` only when it's set. A script being edited
/// keeps its preview.
pub fn build_script(
    id: &str,
    input: &ScriptInput,
    previous: Option<&ProjectScript>,
) -> ProjectScript {
    ProjectScript {
        async_: (input.run_on_worktree_create && input.wait_for_setup).then_some(false),
        auto_open_preview: previous.and_then(|script| script.auto_open_preview),
        command: input.command.trim().to_string(),
        icon: input.icon.wire(),
        id: id.to_string(),
        name: input.name.trim().to_string(),
        preview_url: previous.and_then(|script| script.preview_url.clone()),
        run_on_settle: input.run_on_settle.then_some(true),
        run_on_worktree_create: input.run_on_worktree_create,
    }
}

/// `normalizeScriptId`: the name as a slug of at most 24 characters.
fn script_slug(name: &str) -> String {
    let mut slug = String::new();
    for character in name.trim().to_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        return "script".to_string();
    }
    if slug.len() <= MAX_SCRIPT_ID_LENGTH {
        return slug;
    }
    let cut = slug[..MAX_SCRIPT_ID_LENGTH].trim_end_matches('-');
    if cut.is_empty() {
        "script".to_string()
    } else {
        cut.to_string()
    }
}

/// `nextProjectScriptId`: the name's slug, or the first numbered one that isn't taken.
pub fn next_script_id<'a>(name: &str, existing: impl IntoIterator<Item = &'a str>) -> String {
    let taken = existing.into_iter().collect::<HashSet<_>>();
    let base = script_slug(name);
    if !taken.contains(base.as_str()) {
        return base;
    }
    for suffix in 2..10_000 {
        let candidate = format!("{base}-{suffix}");
        let candidate = if candidate.len() <= MAX_SCRIPT_ID_LENGTH {
            candidate
        } else {
            let keep = MAX_SCRIPT_ID_LENGTH
                .saturating_sub(suffix.to_string().len() + 1)
                .max(1);
            format!("{}-{suffix}", &base[..keep.min(base.len())])
        };
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
    }
    base
}

/// `releaseClaimedRoles`: another script gives up the worktree or settle role the saved one
/// claims.
fn release_roles(mut script: ProjectScript, saved: &ScriptInput) -> ProjectScript {
    if saved.run_on_worktree_create && script.run_on_worktree_create {
        script.run_on_worktree_create = false;
    }
    if saved.run_on_settle && script.run_on_settle == Some(true) {
        script.run_on_settle = Some(false);
    }
    script
}

/// The list after saving `input` as a new script, or over the script `editing`.
pub fn saved_scripts(
    scripts: &[ProjectScript],
    editing: Option<&str>,
    input: &ScriptInput,
) -> Vec<ProjectScript> {
    let previous = editing.and_then(|id| scripts.iter().find(|script| script.id == id));
    let id = match previous {
        Some(previous) => previous.id.clone(),
        None => next_script_id(&input.name, scripts.iter().map(|script| script.id.as_str())),
    };
    let saved = build_script(&id, input, previous);
    let mut next = scripts
        .iter()
        .map(|script| {
            if script.id == id {
                saved.clone()
            } else {
                release_roles(script.clone(), input)
            }
        })
        .collect::<Vec<_>>();
    if previous.is_none() {
        next.push(saved);
    }
    next
}

/// Why `input` can't be saved, if it can't.
pub fn invalid(input: &ScriptInput) -> Option<&'static str> {
    if input.name.trim().is_empty() {
        Some("Name the action.")
    } else if input.command.trim().is_empty() {
        Some("Give it a command to run.")
    } else {
        None
    }
}

// ---------------------------------------------------------------------------------------------
// T3's settings, for the scripts

/// T3's server settings, read when the scripts are first wanted and after each change.
pub struct ProjectScripts {
    settings: Option<ServerSettings>,
    _load: Option<Task<()>>,
}

struct GlobalProjectScripts(Entity<ProjectScripts>);

impl Global for GlobalProjectScripts {}

impl ProjectScripts {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<GlobalProjectScripts>() {
            return global.0.clone();
        }
        let entity = cx.new(|_| Self {
            settings: None,
            _load: None,
        });
        cx.set_global(GlobalProjectScripts(entity.clone()));
        entity
    }

    fn backend(cx: &App) -> Option<Arc<dyn ThreadBackend>> {
        thread_chat::backend(cx)
    }

    /// Reads T3's settings, unless they've been read or are being read.
    pub fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        if self.settings.is_none() && self._load.is_none() {
            self.load(cx);
        }
    }

    pub fn load(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = Self::backend(cx) else {
            return;
        };
        self._load = Some(cx.spawn(async move |this, cx| {
            let settings = thread_chat::thread_settings::load_server_settings(&backend).await;
            this.update(cx, |this, cx| {
                match settings {
                    Ok(settings) => this.settings = Some(settings),
                    Err(error) => log::log!(
                        thread_chat::retry_log_level(&error, log::Level::Warn),
                        "couldn't read T3's script settings: {error:#}"
                    ),
                }
                this._load = None;
                cx.notify();
            })
            .ok();
        }));
    }

    /// The project's scripts: T3's settings' when they've loaded, else the project's own.
    pub fn scripts(&self, project: &OrchestrationProjectShell) -> Vec<ProjectScript> {
        match &self.settings {
            Some(settings) => resolve_scripts(settings, project),
            None => project.scripts.clone(),
        }
    }

    /// Why scripts can't be changed now, if they can't.
    pub fn blocked_reason(&self, cx: &App) -> Option<String> {
        match Self::backend(cx) {
            Some(backend) => backend.blocked_reason(Access::SettingsWrite),
            None => Some("mustr isn't connected to T3".to_string()),
        }
    }

    /// Makes `scripts` the project's.
    pub fn save(
        &mut self,
        project_id: &str,
        scripts: Vec<ProjectScript>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(backend) = Self::backend(cx) else {
            return Task::ready(Err(anyhow::anyhow!("mustr isn't connected to T3")));
        };
        let Some(settings) = &self.settings else {
            return Task::ready(Err(anyhow::anyhow!("T3's settings haven't loaded yet")));
        };
        let call = backend.call_raw(
            rpc::ServerUpdateSettings::TAG,
            json!({ "patch": scripts_patch(settings, project_id, &scripts) }),
            Access::SettingsWrite,
        );
        cx.spawn(async move |this, cx| {
            let settings = call.await?;
            let settings = serde_json::from_value::<ServerSettings>(settings)
                .context("couldn't read T3 Code's settings")?;
            this.update(cx, |this, cx| {
                this.settings = Some(settings);
                cx.notify();
            })?;
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The editor

/// Adds or edits one of a project's scripts (T3's Add Action and Edit Action).
pub struct ScriptEditor {
    project: OrchestrationProjectShell,
    /// The script being edited; `None` adds one.
    editing: Option<String>,
    name: Entity<Editor>,
    command: Entity<Editor>,
    input: ScriptInput,
    confirm_delete: bool,
    saving: bool,
    error: Option<SharedString>,
    _task: Option<Task<()>>,
}

impl ScriptEditor {
    pub fn new(
        project: OrchestrationProjectShell,
        editing: Option<&ProjectScript>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = editing.map(ScriptInput::of).unwrap_or_default();
        let name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Test", window, cx);
            editor.set_text(input.name.clone(), window, cx);
            editor
        });
        let command = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("bun test", window, cx);
            editor.set_text(input.command.clone(), window, cx);
            editor
        });
        Self {
            project,
            editing: editing.map(|script| script.id.clone()),
            name,
            command,
            input,
            confirm_delete: false,
            saving: false,
            error: None,
            _task: None,
        }
    }

    fn typed(&self, cx: &App) -> ScriptInput {
        ScriptInput {
            name: self.name.read(cx).text(cx),
            command: self.command.read(cx).text(cx),
            ..self.input.clone()
        }
    }

    #[cfg(test)]
    pub fn set_input(&mut self, input: ScriptInput, window: &mut Window, cx: &mut Context<Self>) {
        self.name.update(cx, |editor, cx| {
            editor.set_text(input.name.clone(), window, cx)
        });
        self.command.update(cx, |editor, cx| {
            editor.set_text(input.command.clone(), window, cx)
        });
        self.input = input;
    }

    fn write(&mut self, scripts: Vec<ProjectScript>, cx: &mut Context<Self>) {
        let store = ProjectScripts::global(cx);
        let save = store.update(cx, |store, cx| store.save(&self.project.id, scripts, cx));
        self.saving = true;
        self.error = None;
        cx.notify();
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = save.await;
            this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    Ok(()) => cx.emit(DismissEvent),
                    Err(error) => {
                        log::error!("couldn't save the project's actions: {error:#}");
                        this.error = Some(format!("Couldn't save: {error:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub fn save(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let input = self.typed(cx);
        if let Some(reason) = invalid(&input) {
            self.error = Some(reason.into());
            cx.notify();
            return;
        }
        let store = ProjectScripts::global(cx);
        if let Some(reason) = store.read(cx).blocked_reason(cx) {
            self.error = Some(reason.into());
            cx.notify();
            return;
        }
        let scripts = store.read(cx).scripts(&self.project);
        let next = saved_scripts(&scripts, self.editing.as_deref(), &input);
        self.write(next, cx);
    }

    pub fn delete(&mut self, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.clone() else {
            return;
        };
        if !self.confirm_delete {
            self.confirm_delete = true;
            cx.notify();
            return;
        }
        let store = ProjectScripts::global(cx);
        if let Some(reason) = store.read(cx).blocked_reason(cx) {
            self.error = Some(reason.into());
            cx.notify();
            return;
        }
        let next = store
            .read(cx)
            .scripts(&self.project)
            .into_iter()
            .filter(|script| script.id != editing)
            .collect();
        self.write(next, cx);
    }

    fn switch(
        &self,
        id: &'static str,
        label: &'static str,
        on: bool,
        set: fn(&mut ScriptInput, bool),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .gap_2()
            .child(
                Switch::new(
                    id,
                    if on {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    set(&mut this.input, !on);
                    cx.notify();
                })),
            )
            .child(Label::new(label).size(LabelSize::Small))
    }
}

impl EventEmitter<DismissEvent> for ScriptEditor {}

impl ModalView for ScriptEditor {}

impl Focusable for ScriptEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

fn field(label: &'static str, editor: &Entity<Editor>, cx: &App) -> impl IntoElement {
    let palette = palette(cx);
    v_flex()
        .gap_1()
        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
        .child(
            div()
                .px_2()
                .py_1p5()
                .rounded(scaled(tokens::CONTROL_RADIUS))
                .border(tokens::BORDER_WIDTH)
                .border_color(palette.border.hsla())
                .bg(palette.background.hsla())
                .child(editor.clone()),
        )
}

impl Render for ScriptEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let blocked = ProjectScripts::global(cx).read(cx).blocked_reason(cx);
        let icons = ScriptIcon::ALL
            .iter()
            .fold(SegmentedControl::new("script-icon"), |control, icon| {
                control.segment(icon.label())
            });
        let selected = ScriptIcon::ALL
            .iter()
            .position(|icon| *icon == self.input.icon)
            .unwrap_or(0);
        let this = cx.weak_entity();
        let icons = icons.selected(selected).on_select(move |index, _, cx| {
            this.update(cx, |this, cx| {
                this.input.icon = ScriptIcon::ALL[index];
                cx.notify();
            })
            .ok();
        });
        let editing = self.editing.is_some();
        v_flex()
            .key_context("ProjectScriptEditor")
            .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| this.save(cx)))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .w(tokens::modal_width(rems(34.), window))
            .p_4()
            .gap_3()
            .rounded(scaled(tokens::CORNER_RADIUS))
            .border(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .bg(palette.surface.hsla())
            .shadow_lg()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        Label::new(if editing { "Edit Action" } else { "Add Action" })
                            .size(LabelSize::Large)
                            .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new(format!(
                            "Actions are {}'s commands, which Run ▾ runs in a thread's terminal.",
                            self.project.title
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .child(field("Name", &self.name, cx))
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Label::new("Icon")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(icons),
            )
            .child(field("Command", &self.command, cx))
            .child(
                v_flex()
                    .gap(scaled(px(6.)))
                    .child(self.switch(
                        "script-on-worktree",
                        "Run automatically on worktree creation",
                        self.input.run_on_worktree_create,
                        |input, on| input.run_on_worktree_create = on,
                        cx,
                    ))
                    .when(self.input.run_on_worktree_create, |this| {
                        this.child(div().pl(scaled(px(24.))).child(self.switch(
                            "script-wait",
                            "Wait for it to finish before the agent starts",
                            self.input.wait_for_setup,
                            |input, on| input.wait_for_setup = on,
                            cx,
                        )))
                    })
                    .child(self.switch(
                        "script-on-settle",
                        "Run in the thread's worktree when the thread settles",
                        self.input.run_on_settle,
                        |input, on| input.run_on_settle = on,
                        cx,
                    )),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                h_flex()
                    .gap_2()
                    .when(editing, |this| {
                        this.child(
                            TextButton::new(
                                "script-delete",
                                if self.confirm_delete {
                                    format!("Delete “{}”?", self.input.name.trim())
                                } else {
                                    "Delete".to_string()
                                },
                            )
                            .disabled(self.saving || blocked.is_some())
                            .on_click(cx.listener(|this, _, _, cx| this.delete(cx))),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        TextButton::new("script-cancel", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        TextButton::new(
                            "script-save",
                            match (self.saving, editing) {
                                (true, _) => "Saving…",
                                (false, true) => "Save changes",
                                (false, false) => "Save action",
                            },
                        )
                        .style(ButtonStyle::Primary)
                        .disabled(self.saving || blocked.is_some())
                        .when_some(blocked, |button, reason| button.tooltip(reason))
                        .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            )
    }
}

#[cfg(test)]
#[path = "project_scripts_tests.rs"]
mod tests;
