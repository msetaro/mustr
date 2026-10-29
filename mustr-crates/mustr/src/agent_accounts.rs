//! More agents on the agents page (`mustr-docs/PARITY.md` §2.17, "Provider instances, ACP
//! agents"), as T3's "Add provider" wizard does it (`settings/AddProviderInstanceDialog.tsx`,
//! `AcpRegistrySearchStep.tsx`):
//! - **Another account** of Codex or Claude Code: a provider instance with its own label, id
//!   (`codex_work` from "Work") and home folder (`CODEX_HOME` or `CLAUDE_CONFIG_DIR`), so its
//!   sign-in is kept apart from the default one's.
//! - **An agent from the ACP registry** (agentclientprotocol.com's list, which T3 reads):
//!   `server.searchAcpRegistry`, then `server.prepareAcpRegistryAgent` (T3 downloads it where it
//!   runs), then an `acpRegistry` instance for it. Signing in to it happens on the page.
//! - **Removing** an instance that isn't an agent's default (`{operation: "remove"}`), and for a
//!   registry agent, the copy T3 downloaded (`server.uninstallAcpRegistryManagedBinary`).
//!
//! Instances change with `server.updateSettings` `{patch: {}, providerInstanceMutation}`, which
//! needs `providers:manage`; preparing and uninstalling registry agents does too. Searching is a
//! read.

#[cfg(test)]
mod tests;

use anyhow::{Context as _, Result};
use editor::{Editor, EditorEvent};
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Task, Window, div, px, rems,
};
use mustr_ui::{ButtonStyle, SegmentedControl, TextButton, palette, tokens, tokens::scaled};
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc, time::Duration};
use t3_client::t3_api::types::{AcpRegistrySearchAgent, AcpRegistrySearchResult, ServerSettings};
use thread_chat::{Access, ServerConfigs, ThreadBackend};
use ui::{Color, Label, LabelCommon as _, LabelSize, prelude::*};
use workspace::ModalView;

/// T3's limit on an instance id, and the part of it a label's slug may take.
const MAX_INSTANCE_ID: usize = 64;
const MAX_SLUG: usize = 48;
/// How long the search waits after the last key, as T3's does.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);
/// The registry's agents T3 lists; it caps a search at 512.
const SEARCH_LIMIT: i64 = 512;
/// Registry agents that T3 runs with a driver of its own, so the registry step leaves them out
/// (each driver's `acpRegistryAgentIds`).
const FIRST_CLASS_AGENTS: [&str; 8] = [
    "codex-acp",
    "claude-acp",
    "antigravity-acp",
    "cursor",
    "grok-build",
    "opencode",
    "opencode-v2",
    "pi-acp",
];

/// The agents a second account can be added for, with T3's labels and home-folder settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountDriver {
    Codex,
    Claude,
}

impl AccountDriver {
    pub(crate) const ALL: [Self; 2] = [Self::Codex, Self::Claude];

    pub(crate) fn kind(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claudeAgent",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
        }
    }

    fn home_variable(self) -> &'static str {
        match self {
            Self::Codex => "CODEX_HOME",
            Self::Claude => "CLAUDE_CONFIG_DIR",
        }
    }

    fn home_example(self) -> &'static str {
        match self {
            Self::Codex => "~/.codex-work",
            Self::Claude => "~/.claude-work",
        }
    }
}

/// `"Work account"` → `"work_account"` (`slugifyLabel` in T3).
pub(crate) fn slugify(label: &str) -> String {
    let mut slug = String::new();
    let mut gap = false;
    for character in label.to_lowercase().chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            if gap && !slug.is_empty() {
                slug.push('_');
            }
            gap = false;
            slug.push(character);
        } else {
            gap = true;
        }
    }
    slug.chars()
        .take(MAX_SLUG)
        .collect::<String>()
        .trim_end_matches('_')
        .to_string()
}

/// The id T3's wizard gives an instance: `<driver>_<slug>`, or the driver alone for an empty
/// label, with `_2`, `_3`, … when it's taken (`deriveInstanceId`, `deriveAvailableInstanceId`).
pub(crate) fn instance_id(driver: &str, label: &str, taken: &HashSet<String>) -> String {
    let slug = slugify(label);
    let base = if slug.is_empty() {
        driver.to_string()
    } else {
        format!("{driver}_{slug}")
    };
    if !taken.contains(&base) {
        return base;
    }
    (2..)
        .map(|number| {
            let suffix = format!("_{number}");
            let room = MAX_INSTANCE_ID.saturating_sub(suffix.len());
            format!("{}{suffix}", base.chars().take(room).collect::<String>())
        })
        .find(|candidate| !taken.contains(candidate))
        .unwrap_or(base)
}

/// The ids in use: every instance T3 has, and the agents' defaults (`codex`, `claudeAgent`).
pub(crate) fn taken_ids(
    settings: Option<&ServerSettings>,
    providers: &[String],
) -> HashSet<String> {
    let mut taken = providers.iter().cloned().collect::<HashSet<_>>();
    taken.extend(["codex".to_string(), "claudeAgent".to_string()]);
    if let Some(instances) = settings.and_then(|settings| settings.provider_instances.as_ref()) {
        taken.extend(instances.keys().cloned());
    }
    taken
}

/// `server.updateSettings` creating another account of `driver`, with its own home folder.
pub(crate) fn account_payload(
    driver: AccountDriver,
    instance_id: &str,
    label: &str,
    home: &str,
) -> Value {
    let mut config = json!({ "homePath": home });
    if driver == AccountDriver::Codex {
        // An account on the Codex CLI's own setup, as T3's wizard makes it.
        config["setupMode"] = json!("existing");
    }
    let mut instance = json!({ "driver": driver.kind(), "enabled": true, "config": config });
    if !label.trim().is_empty() {
        instance["displayName"] = json!(label.trim());
    }
    json!({
        "patch": {},
        "providerInstanceMutation": {
            "operation": "create", "instanceId": instance_id, "instance": instance
        }
    })
}

/// `server.updateSettings` adding a registry agent T3 has prepared.
pub(crate) fn registry_payload(agent: &AcpRegistrySearchAgent, taken: &HashSet<String>) -> Value {
    let instance_id = instance_id("acpRegistry", &agent.name, taken);
    let mut config = json!({ "agentId": agent.id, "distribution": "auto" });
    if let Some(icon) = &agent.icon {
        config["registryIconUrl"] = json!(icon);
    }
    json!({
        "patch": {},
        "providerInstanceMutation": {
            "operation": "create",
            "instanceId": instance_id,
            "instance": {
                "driver": "acpRegistry", "enabled": true, "displayName": agent.name,
                "config": config
            }
        }
    })
}

/// `server.updateSettings` removing an instance.
pub(crate) fn remove_payload(instance_id: &str) -> Value {
    json!({
        "patch": {},
        "providerInstanceMutation": { "operation": "remove", "instanceId": instance_id }
    })
}

/// The registry agents T3 already has an instance for, by their registry id.
pub(crate) fn added_agents(settings: Option<&ServerSettings>) -> HashSet<String> {
    settings
        .and_then(|settings| settings.provider_instances.as_ref())
        .map(|instances| {
            instances
                .values()
                .filter(|instance| instance.driver == "acpRegistry")
                .filter_map(|instance| {
                    let config = serde_json::to_value(instance.config.as_ref()?).ok()?;
                    Some(config.get("agentId")?.as_str()?.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The registry agent an instance runs, if T3 downloaded it from the registry (not a local
/// command), for removing that copy with the instance.
pub(crate) fn registry_agent_of(
    settings: Option<&ServerSettings>,
    instance_id: &str,
) -> Option<String> {
    let instance = settings?.provider_instances.as_ref()?.get(instance_id)?;
    if instance.driver != "acpRegistry" {
        return None;
    }
    let config = serde_json::to_value(instance.config.as_ref()?).ok()?;
    if config.get("source").and_then(Value::as_str) == Some("local") {
        return None;
    }
    Some(config.get("agentId")?.as_str()?.to_string())
}

/// Whether an instance is one an agent has by default (its id is its driver), which T3's app
/// resets rather than removes.
pub(crate) fn is_default_instance(instance_id: &str, driver: &str) -> bool {
    instance_id == driver
}

/// Sends a provider-instance change and takes T3's settings from the answer.
pub(crate) async fn mutate_instances(
    backend: Arc<dyn ThreadBackend>,
    payload: Value,
) -> Result<ServerSettings> {
    let settings = backend
        .call_raw("server.updateSettings", payload, Access::ProvidersManage)
        .await?;
    serde_json::from_value(settings).context("couldn't read T3 Code's settings")
}

// The dialog.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Account,
    Registry,
}

enum Search {
    Idle,
    Searching,
    Found(Vec<AcpRegistrySearchAgent>),
    Failed(SharedString),
}

pub struct AddAgentDialog {
    backend: Option<Arc<dyn ThreadBackend>>,
    configs: Entity<ServerConfigs>,
    tab: Tab,
    driver: AccountDriver,
    label: Entity<Editor>,
    home: Entity<Editor>,
    query: Entity<Editor>,
    search: Search,
    /// The registry agent being prepared, by its id.
    adding: Option<String>,
    saving: bool,
    error: Option<SharedString>,
    _search: Option<Task<()>>,
    _task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AddAgentDialog {
    pub(crate) fn new(
        backend: Option<Arc<dyn ThreadBackend>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let configs = ServerConfigs::global(cx);
        let label = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Work", window, cx);
            editor
        });
        let home = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(AccountDriver::Codex.home_example(), window, cx);
            editor
        });
        let query = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search the ACP registry", window, cx);
            editor
        });
        let subscriptions = vec![
            cx.subscribe(&label, |_, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    cx.notify();
                }
            }),
            cx.subscribe(&query, |this, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    this.search(true, cx);
                }
            }),
        ];
        Self {
            backend,
            configs,
            tab: Tab::Account,
            driver: AccountDriver::Codex,
            label,
            home,
            query,
            search: Search::Idle,
            adding: None,
            saving: false,
            error: None,
            _search: None,
            _task: None,
            _subscriptions: subscriptions,
        }
    }

    fn blocked(&self) -> Option<String> {
        match &self.backend {
            Some(backend) => backend.blocked_reason(Access::ProvidersManage),
            None => Some("mustr isn't connected to T3 Code".into()),
        }
    }

    fn taken(&self, cx: &App) -> HashSet<String> {
        let configs = self.configs.read(cx);
        let providers = configs
            .providers()
            .map(|providers| {
                providers
                    .iter()
                    .map(|provider| provider.instance_id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        taken_ids(configs.settings(), &providers)
    }

    fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        self.error = None;
        if tab == Tab::Registry && matches!(self.search, Search::Idle) {
            self.search(false, cx);
        }
        cx.notify();
    }

    /// Searches the registry for what's typed (`server.searchAcpRegistry`).
    fn search(&mut self, debounce: bool, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        let query = self.query.read(cx).text(cx).trim().to_string();
        self.search = Search::Searching;
        cx.notify();
        self._search = Some(cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            }
            let result = backend
                .call_raw(
                    "server.searchAcpRegistry",
                    json!({ "query": query, "limit": SEARCH_LIMIT }),
                    Access::Read,
                )
                .await
                .and_then(|value| {
                    serde_json::from_value::<AcpRegistrySearchResult>(value)
                        .context("couldn't read the registry's agents")
                });
            this.update(cx, |this, cx| {
                this.search = match result {
                    Ok(result) => Search::Found(
                        result
                            .agents
                            .into_iter()
                            .filter(|agent| !FIRST_CLASS_AGENTS.contains(&agent.id.as_str()))
                            .collect(),
                    ),
                    Err(error) if thread_chat::is_unsupported_rpc(&error) => Search::Failed(
                        "This T3 Code doesn't have the ACP registry. Update it to add agents \
                         from it."
                            .into(),
                    ),
                    Err(error) => Search::Failed(format!("{error:#}").into()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Adds another account of the chosen agent.
    pub(crate) fn add_account(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        if self.saving {
            return;
        }
        let label = self.label.read(cx).text(cx).trim().to_string();
        let home = self.home.read(cx).text(cx).trim().to_string();
        if home.is_empty() {
            self.error = Some(
                format!(
                    "Give the account a folder of its own, such as {}, so its sign-in stays \
                     apart from the default one's.",
                    self.driver.home_example()
                )
                .into(),
            );
            cx.notify();
            return;
        }
        let id = instance_id(self.driver.kind(), &label, &self.taken(cx));
        let payload = account_payload(self.driver, &id, &label, &home);
        self.run(mutate_instances(backend, payload), cx);
    }

    /// Prepares a registry agent where T3 runs, then adds it.
    pub(crate) fn add_registry_agent(
        &mut self,
        agent: AcpRegistrySearchAgent,
        cx: &mut Context<Self>,
    ) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        if self.saving {
            return;
        }
        let payload = registry_payload(&agent, &self.taken(cx));
        self.adding = Some(agent.id.clone());
        let agent_id = agent.id;
        let call = async move {
            backend
                .call_raw(
                    "server.prepareAcpRegistryAgent",
                    json!({ "agentId": agent_id }),
                    Access::ProvidersManage,
                )
                .await
                .context("T3 Code couldn't download the agent")?;
            mutate_instances(backend, payload).await
        };
        self.run(call, cx);
    }

    fn run(
        &mut self,
        call: impl std::future::Future<Output = Result<ServerSettings>> + 'static,
        cx: &mut Context<Self>,
    ) {
        self.saving = true;
        self.error = None;
        cx.notify();
        let configs = self.configs.clone();
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                this.saving = false;
                this.adding = None;
                match result {
                    Ok(settings) => {
                        configs.update(cx, |configs, cx| configs.set_settings(settings, cx));
                        cx.emit(DismissEvent);
                    }
                    Err(error) => {
                        log::warn!("couldn't add the agent: {error:#}");
                        this.error = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    #[cfg(test)]
    pub(crate) fn found(&self) -> Option<Vec<String>> {
        match &self.search {
            Search::Found(agents) => Some(agents.iter().map(|agent| agent.id.clone()).collect()),
            _ => None,
        }
    }
}

impl EventEmitter<DismissEvent> for AddAgentDialog {}

impl ModalView for AddAgentDialog {}

impl Focusable for AddAgentDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.tab {
            Tab::Account => self.label.focus_handle(cx),
            Tab::Registry => self.query.focus_handle(cx),
        }
    }
}

fn field(editor: &Entity<Editor>, cx: &App) -> impl IntoElement {
    let palette = palette(cx);
    div()
        .flex_1()
        .min_w(scaled(px(180.)))
        .px_2()
        .py_1()
        .rounded(scaled(tokens::CONTROL_RADIUS))
        .border(tokens::BORDER_WIDTH)
        .border_color(palette.border.hsla())
        .bg(palette.background.hsla())
        .child(editor.clone())
}

fn labeled(label: impl Into<SharedString>, control: impl IntoElement) -> impl IntoElement {
    h_flex()
        .w_full()
        .flex_wrap()
        .gap_x_2()
        .gap_y_1()
        .child(
            div().w(scaled(px(96.))).flex_none().child(
                Label::new(label.into())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            ),
        )
        .child(div().flex_1().min_w(scaled(px(180.))).flex().child(control))
}

fn muted(text: impl Into<SharedString>) -> Label {
    Label::new(text.into())
        .size(LabelSize::Small)
        .color(Color::Muted)
}

impl AddAgentDialog {
    fn render_account(&self, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let drivers = AccountDriver::ALL
            .iter()
            .fold(
                SegmentedControl::new("add-agent-driver"),
                |control, driver| control.segment(driver.label()),
            )
            .selected(
                AccountDriver::ALL
                    .iter()
                    .position(|driver| *driver == self.driver)
                    .unwrap_or(0),
            )
            .on_select(move |index, window, cx| {
                this.update(cx, |this, cx| {
                    this.driver = AccountDriver::ALL[index];
                    let example = this.driver.home_example();
                    this.home.update(cx, |editor, cx| {
                        editor.set_placeholder_text(example, window, cx)
                    });
                    cx.notify();
                })
                .ok();
            });
        let label = self.label.read(cx).text(cx);
        let id = instance_id(self.driver.kind(), &label, &self.taken(cx));
        v_flex()
            .w_full()
            .gap_2()
            .child(muted(
                "Another account of an agent, signed in on its own. T3 Code keeps its sign-in \
                 in its own folder.",
            ))
            .child(labeled(
                "Agent",
                div()
                    .debug_selector(|| "add-agent-driver".into())
                    .child(drivers),
            ))
            .child(labeled("Label", field(&self.label, cx)))
            .child(labeled(
                format!("{} folder", self.driver.home_variable()),
                field(&self.home, cx),
            ))
            .child(
                div()
                    .debug_selector(|| "add-agent-instance-id".into())
                    .child(muted(format!("T3 Code saves it as {id}.")).size(LabelSize::XSmall)),
            )
            .into_any_element()
    }

    fn render_registry(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = palette(cx);
        let added = added_agents(self.configs.read(cx).settings());
        let blocked = self.blocked();
        let results: AnyElement = match &self.search {
            Search::Idle | Search::Searching => muted("Searching the registry…").into_any_element(),
            Search::Failed(error) => Label::new(error.clone())
                .size(LabelSize::Small)
                .color(Color::Error)
                .into_any_element(),
            Search::Found(agents) if agents.is_empty() => {
                muted("No agents match.").into_any_element()
            }
            Search::Found(agents) => v_flex()
                .id("add-agent-results")
                .w_full()
                .max_h(scaled(px(200.)))
                .overflow_y_scroll()
                .children(agents.iter().enumerate().map(|(index, agent)| {
                    let is_added = added.contains(&agent.id);
                    let adding = self.adding.as_deref() == Some(agent.id.as_str());
                    let agent_for_click = agent.clone();
                    let this = cx.entity().downgrade();
                    h_flex()
                        .w_full()
                        .gap_2()
                        .py(scaled(px(6.)))
                        .when(index > 0, |this| {
                            this.border_t(tokens::BORDER_WIDTH)
                                .border_color(palette.border.hsla())
                        })
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .child(Label::new(agent.name.clone()).truncate())
                                        .child(
                                            muted(agent.version.clone()).size(LabelSize::XSmall),
                                        ),
                                )
                                .child(muted(agent.description.clone()).truncate()),
                        )
                        .child(if is_added {
                            muted("Added").into_any_element()
                        } else {
                            let selector = format!("add-agent-registry-{}", agent.id);
                            div()
                                .debug_selector(move || selector)
                                .child(
                                    TextButton::new(
                                        ("add-agent-registry", index),
                                        if adding { "Downloading…" } else { "Add" },
                                    )
                                    .disabled(self.saving || blocked.is_some())
                                    .when_some(blocked.clone(), |button, reason| {
                                        button.tooltip(reason)
                                    })
                                    .on_click(
                                        move |_, _, cx| {
                                            let agent = agent_for_click.clone();
                                            this.update(cx, |this, cx| {
                                                this.add_registry_agent(agent, cx)
                                            })
                                            .ok();
                                        },
                                    ),
                                )
                                .into_any_element()
                        })
                }))
                .into_any_element(),
        };
        v_flex()
            .w_full()
            .gap_2()
            .child(muted(
                "Agents that speak the Agent Client Protocol, from agentclientprotocol.com's \
                 registry. T3 Code downloads the one you add where it runs.",
            ))
            .child(field(&self.query, cx))
            .child(results)
            .into_any_element()
    }
}

impl Render for AddAgentDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let this = cx.entity().downgrade();
        let tabs = SegmentedControl::new("add-agent-tab")
            .segment("Another account")
            .segment("ACP registry")
            .selected(match self.tab {
                Tab::Account => 0,
                Tab::Registry => 1,
            })
            .on_select(move |index, window, cx| {
                this.update(cx, |this, cx| {
                    this.set_tab(
                        if index == 0 {
                            Tab::Account
                        } else {
                            Tab::Registry
                        },
                        cx,
                    );
                    // Typing goes to the tab's first field.
                    window.focus(&this.focus_handle(cx), cx);
                })
                .ok();
            });
        let blocked = self.blocked();
        let body = match self.tab {
            Tab::Account => self.render_account(cx),
            Tab::Registry => self.render_registry(cx),
        };
        v_flex()
            .key_context("MustrAddAgent")
            .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| {
                if this.tab == Tab::Account {
                    this.add_account(cx)
                }
            }))
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
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        Label::new("Add an agent")
                            .size(LabelSize::Large)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(div().flex_1())
                    .child(div().debug_selector(|| "add-agent-tab".into()).child(tabs)),
            )
            .child(body)
            .when_some(blocked.clone(), |this, reason| this.child(muted(reason)))
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .debug_selector(|| "add-agent-error".into())
                        .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
                )
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        TextButton::new("add-agent-close", "Close")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .when(self.tab == Tab::Account, |this| {
                        this.child(
                            div().debug_selector(|| "add-agent-save".into()).child(
                                TextButton::new(
                                    "add-agent-save",
                                    if self.saving {
                                        "Adding…"
                                    } else {
                                        "Add account"
                                    },
                                )
                                .style(ButtonStyle::Primary)
                                .disabled(self.saving || blocked.is_some())
                                .on_click(cx.listener(|this, _, _, cx| this.add_account(cx))),
                            ),
                        )
                    }),
            )
    }
}
