//! Source control accounts (`mustr-docs/PARITY.md` §2.11, "Source control accounts"), as T3's
//! settings show them (`settings/SourceControlSettings.tsx`): for each forge T3 knows (GitHub,
//! GitLab, Forgejo, Azure DevOps, …), whether its CLI is on T3's machine, which account it's
//! signed in as, and if it isn't, T3's instructions for signing in there; and the version
//! control tools (git, jj).
//!
//! It all comes from `server.discoverSourceControl`, a read: T3 runs the forges' own status
//! commands (`gh auth status`, `glab auth status`, …) where it runs. mustr never signs in to a
//! forge itself, nor runs `gh auth`; the page gives the command to run on T3's machine. It's a
//! section of the T3 Code settings page (`crate::t3_settings`).

use gpui::{
    AnyElement, ClipboardItem, Context, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Task, Window, div, px,
};
use mustr_ui::{ButtonStyle, Card, Pill, TextButton, palette, tokens, tokens::scaled};
use serde_json::{Value, json};
use std::sync::Arc;
use thread_chat::{Access, ThreadBackend};
use ui::{Color, Label, LabelCommon as _, LabelSize, prelude::*};

/// One forge, as T3 found it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Forge {
    pub kind: String,
    pub label: String,
    pub executable: Option<String>,
    pub available: bool,
    pub version: Option<String>,
    pub install_hint: String,
    pub detail: Option<String>,
    pub auth: ForgeAuth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthStatus {
    Authenticated,
    Unauthenticated,
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ForgeAuth {
    pub status: AuthStatus,
    pub account: Option<String>,
    pub host: Option<String>,
    pub detail: Option<String>,
    pub accounts: Vec<ForgeAccount>,
}

/// One of a CLI's logins (`gh` can hold several, one active per host).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ForgeAccount {
    pub host: String,
    pub account: String,
    pub active: bool,
    pub authenticated: bool,
    pub error: Option<String>,
    pub environment_variable: Option<String>,
}

/// A version control tool, as T3 found it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct VersionControl {
    pub label: String,
    pub available: bool,
    pub version: Option<String>,
    pub install_hint: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Discovery {
    pub forges: Vec<Forge>,
    pub tools: Vec<VersionControl>,
}

/// A string T3 sends as an Effect `Option` (`{_tag: "Some", value}` or `{_tag: "None"}`), or
/// plainly.
fn option_string(value: Option<&Value>) -> Option<String> {
    let value = value?;
    let text = match value {
        Value::String(text) => Some(text.as_str()),
        Value::Object(object) if object.get("_tag").and_then(Value::as_str) == Some("Some") => {
            object.get("value").and_then(Value::as_str)
        }
        _ => None,
    }?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Reads `server.discoverSourceControl`'s answer, leniently, so newer servers' additions don't
/// lose the page.
pub(crate) fn parse_discovery(value: &Value) -> Discovery {
    let available = |item: &Value| item.get("status").and_then(Value::as_str) == Some("available");
    let forges = value
        .get("sourceControlProviders")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|item| {
            let auth = item.get("auth").cloned().unwrap_or(Value::Null);
            Forge {
                kind: string(item, "kind"),
                label: string(item, "label"),
                executable: option_string(item.get("executable")),
                available: available(item),
                version: option_string(item.get("version")),
                install_hint: string(item, "installHint"),
                detail: option_string(item.get("detail")),
                auth: ForgeAuth {
                    status: match auth.get("status").and_then(Value::as_str) {
                        Some("authenticated") => AuthStatus::Authenticated,
                        Some("unauthenticated") => AuthStatus::Unauthenticated,
                        _ => AuthStatus::Unknown,
                    },
                    account: option_string(auth.get("account")),
                    host: option_string(auth.get("host")),
                    detail: option_string(auth.get("detail")),
                    accounts: auth
                        .get("accounts")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .map(|account| ForgeAccount {
                            host: string(account, "host"),
                            account: string(account, "account"),
                            active: account
                                .get("active")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                            authenticated: account
                                .get("authenticated")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                            error: option_string(account.get("error")),
                            environment_variable: option_string(account.get("environmentVariable")),
                        })
                        .collect(),
                },
            }
        })
        .collect();
    let tools = value
        .get("versionControlSystems")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|item| VersionControl {
            label: string(item, "label"),
            available: available(item),
            version: option_string(item.get("version")),
            install_hint: string(item, "installHint"),
        })
        .collect();
    Discovery { forges, tools }
}

/// The command that signs a forge's CLI in, where T3 runs: T3's `loginCommand` for GitHub, and
/// the CLIs' own for the others.
pub(crate) fn login_command(forge: &Forge) -> Option<&'static str> {
    match forge.executable.as_deref().unwrap_or(forge.kind.as_str()) {
        "gh" | "github" => Some("gh auth login"),
        "glab" | "gitlab" => Some("glab auth login"),
        "tea" | "forgejo" => Some("tea login add"),
        "az" | "azure-devops" => Some("az login"),
        _ => None,
    }
}

/// What a forge's row says (`DiscoveryItemRow`'s summary in T3).
pub(crate) fn summary(forge: &Forge) -> String {
    if !forge.available {
        return if forge.install_hint.is_empty() {
            "Not available on T3 Code's machine.".into()
        } else {
            format!("Not available on T3 Code's machine. {}", forge.install_hint)
        };
    }
    let Some(executable) = &forge.executable else {
        return format!("Available. {}", forge.install_hint)
            .trim()
            .to_string();
    };
    match forge.auth.status {
        AuthStatus::Authenticated => {
            let mut text = match &forge.auth.account {
                Some(account) => format!("Signed in as {account}"),
                None => "Signed in".to_string(),
            };
            if let Some(detail) = &forge.auth.detail {
                text.push_str(" · ");
                text.push_str(detail);
            }
            text
        }
        AuthStatus::Unauthenticated => format!(
            "{} isn't signed in on T3 Code's machine. Sign in there with {executable}, or give it \
             credentials, so agents can open and update pull requests.",
            forge.label
        ),
        AuthStatus::Unknown => format!(
            "Couldn't check {}. {}",
            forge.label,
            forge.detail.as_deref().unwrap_or(&forge.install_hint)
        )
        .trim()
        .to_string(),
    }
}

enum Loadable {
    Loading,
    Loaded(Discovery),
    Failed(SharedString),
    Unsupported,
}

/// The forges and version control tools on T3's machine, for the settings page.
pub struct SourceControlAccounts {
    backend: Option<Arc<dyn ThreadBackend>>,
    discovery: Loadable,
    /// The command just copied, by forge.
    copied: Option<String>,
    load: Option<Task<()>>,
}

impl SourceControlAccounts {
    pub(crate) fn new(backend: Option<Arc<dyn ThreadBackend>>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            backend,
            discovery: Loadable::Loading,
            copied: None,
            load: None,
        };
        this.rescan(cx);
        this
    }

    /// Asks T3 what's on its machine (`server.discoverSourceControl`).
    pub(crate) fn rescan(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            self.discovery = Loadable::Failed("mustr isn't connected to T3 Code".into());
            return;
        };
        if !matches!(self.discovery, Loadable::Loaded(_)) {
            self.discovery = Loadable::Loading;
        }
        cx.notify();
        self.load = Some(cx.spawn(async move |this, cx| {
            let result = backend
                .call_raw("server.discoverSourceControl", json!({}), Access::Read)
                .await;
            this.update(cx, |this, cx| {
                this.discovery = match result {
                    Ok(value) => Loadable::Loaded(parse_discovery(&value)),
                    Err(error) if thread_chat::is_unsupported_rpc(&error) => Loadable::Unsupported,
                    Err(error) => {
                        log::warn!("couldn't read T3's source control: {error:#}");
                        Loadable::Failed(
                            format!("Couldn't read T3 Code's source control: {error:#}").into(),
                        )
                    }
                };
                this.load = None;
                cx.notify();
            })
            .ok();
        }));
    }

    #[cfg(test)]
    pub(crate) fn discovery(&self) -> Option<&Discovery> {
        match &self.discovery {
            Loadable::Loaded(discovery) => Some(discovery),
            _ => None,
        }
    }

    fn render_forge(&self, index: usize, forge: &Forge, cx: &mut Context<Self>) -> AnyElement {
        let palette = palette(cx);
        let (pill, color) = match (forge.available, forge.auth.status) {
            (false, _) => ("Not installed", palette.text_muted.hsla()),
            (true, AuthStatus::Authenticated) => ("Signed in", palette.ok.hsla()),
            (true, AuthStatus::Unauthenticated) => ("Not signed in", palette.warning.hsla()),
            (true, AuthStatus::Unknown) => ("Unknown", palette.text_muted.hsla()),
        };
        let login = (forge.available && forge.auth.status == AuthStatus::Unauthenticated)
            .then(|| login_command(forge))
            .flatten();
        let kind = forge.kind.clone();
        let selector = format!("source-control-{kind}");
        v_flex()
            .debug_selector(move || selector)
            .w_full()
            .gap_1()
            .px_2()
            .py(scaled(px(9.)))
            .when(index > 0, |this| {
                this.border_t(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
            })
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(Label::new(forge.label.clone()).weight(FontWeight::MEDIUM))
                    .child(Pill::tinted(pill, color))
                    .children(forge.version.clone().map(|version| {
                        Label::new(version)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate()
                    })),
            )
            .child(
                Label::new(summary(forge))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when_some(login, |this, command| {
                let copied = self.copied.as_deref() == Some(kind.as_str());
                let kind = kind.clone();
                this.child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .px_2()
                                .py_1()
                                .rounded(scaled(tokens::CONTROL_RADIUS))
                                .bg(palette.background.hsla())
                                .font_family(tokens::CODE_FONT_FAMILY)
                                .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                                .text_color(palette.text.hsla())
                                .child(command),
                        )
                        .child(
                            div()
                                .debug_selector({
                                    let kind = kind.clone();
                                    move || format!("source-control-copy-{kind}")
                                })
                                .child(
                                    TextButton::new(
                                        SharedString::from(format!("source-control-copy-{kind}")),
                                        if copied { "Copied" } else { "Copy" },
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                command.to_string(),
                                            ));
                                            this.copied = Some(kind.clone());
                                            cx.notify();
                                        },
                                    )),
                                ),
                        ),
                )
            })
            .children(forge.auth.accounts.iter().map(|account| {
                let mut line = format!("{} · {}", account.host, account.account);
                if account.active {
                    line.push_str(" · active");
                }
                if !account.authenticated {
                    line.push_str(" · signed out");
                }
                if let Some(variable) = &account.environment_variable {
                    line.push_str(&format!(" · from {variable}"));
                }
                if let Some(error) = &account.error {
                    line.push_str(&format!(" · {error}"));
                }
                h_flex()
                    .gap_1p5()
                    .pl(scaled(px(2.)))
                    .child(div().flex_none().size(scaled(px(6.))).rounded_full().bg(
                        if account.authenticated {
                            palette.ok.hsla()
                        } else {
                            palette.border.hsla()
                        },
                    ))
                    .child(Label::new(line).size(LabelSize::XSmall).color(Color::Muted))
            }))
            .into_any_element()
    }
}

impl Render for SourceControlAccounts {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let muted =
            |text: SharedString| Label::new(text).size(LabelSize::Small).color(Color::Muted);
        let rescan = div()
            .debug_selector(|| "source-control-rescan".into())
            .child(
                TextButton::new(
                    "source-control-rescan",
                    if self.load.is_some() {
                        "Checking…"
                    } else {
                        "Check again"
                    },
                )
                .style(ButtonStyle::Default)
                .disabled(self.load.is_some())
                .on_click(cx.listener(|this, _, _, cx| this.rescan(cx))),
            );
        let body: AnyElement = match &self.discovery {
            Loadable::Loading => Card::new()
                .child(muted("Checking T3 Code's machine…".into()))
                .into_any_element(),
            Loadable::Unsupported => div()
                .debug_selector(|| "source-control-unsupported".into())
                .child(Card::new().child(muted(
                    "This T3 Code doesn't report its source control accounts. Update it to see \
                     them here."
                        .into(),
                )))
                .into_any_element(),
            Loadable::Failed(error) => Card::new()
                .child(Label::new(error.clone()).size(LabelSize::Small).color(Color::Error))
                .into_any_element(),
            Loadable::Loaded(discovery) => {
                let discovery = discovery.clone();
                let forges = discovery
                    .forges
                    .iter()
                    .enumerate()
                    .map(|(index, forge)| self.render_forge(index, forge, cx))
                    .collect::<Vec<_>>();
                let tools = discovery.tools.iter().map(|tool| {
                    let text = match (&tool.version, tool.available) {
                        (Some(version), true) => format!("{} · {version}", tool.label),
                        (None, true) => tool.label.clone(),
                        (_, false) => format!("{} · not installed", tool.label),
                    };
                    h_flex()
                        .gap_1p5()
                        .child(
                            div()
                                .flex_none()
                                .size(scaled(px(6.)))
                                .rounded_full()
                                .bg(if tool.available {
                                    palette.ok.hsla()
                                } else {
                                    palette.border.hsla()
                                }),
                        )
                        .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
                });
                v_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        Card::new()
                            .px(scaled(px(10.)))
                            .py(scaled(px(2.)))
                            .children(forges),
                    )
                    .child(h_flex().flex_wrap().gap_x_4().gap_y_1().children(tools))
                    .into_any_element()
            }
        };
        v_flex()
            .w_full()
            .gap_1p5()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(Label::new("Source control").weight(FontWeight::SEMIBOLD))
                    .child(
                        Label::new("Accounts T3 Code's agents use for pull requests")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(rescan),
            )
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_t3::{Call, FakeT3};
    use gpui::TestAppContext;
    use pretty_assertions::assert_eq;

    fn some(value: &str) -> Value {
        json!({ "_id": "Option", "_tag": "Some", "value": value })
    }

    fn none() -> Value {
        json!({ "_id": "Option", "_tag": "None" })
    }

    fn discovery_json() -> Value {
        json!({
            "versionControlSystems": [
                { "kind": "git", "label": "Git", "implemented": true, "executable": "git",
                  "status": "available", "version": some("git version 2.47.1"),
                  "installHint": "Install git", "detail": none() },
                { "kind": "jj", "label": "Jujutsu", "implemented": true, "executable": "jj",
                  "status": "missing", "version": none(), "installHint": "Install jj",
                  "detail": none() }
            ],
            "sourceControlProviders": [
                { "kind": "github", "label": "GitHub", "executable": "gh", "status": "available",
                  "version": some("gh version 2.83.0"), "installHint": "Install the GitHub CLI",
                  "detail": none(),
                  "auth": {
                      "status": "authenticated", "account": some("octocat"),
                      "host": some("github.com"),
                      "detail": some("Using GH_TOKEN from the server environment"),
                      "accounts": [
                          { "host": "github.com", "account": "octocat", "active": true,
                            "authenticated": true, "environmentVariable": "GH_TOKEN" }
                      ]
                  } },
                { "kind": "gitlab", "label": "GitLab", "executable": "glab",
                  "status": "available", "version": some("glab 1.50.0"),
                  "installHint": "Install glab", "detail": none(),
                  "auth": { "status": "unauthenticated", "account": none(), "host": none(),
                            "detail": none() } },
                { "kind": "bitbucket", "label": "Bitbucket", "status": "missing",
                  "version": none(), "installHint": "Add a Bitbucket token in T3 Code's settings.",
                  "detail": none(),
                  "auth": { "status": "unknown", "account": none(), "host": none(),
                            "detail": none() } }
            ]
        })
    }

    #[test]
    fn discovery_reads_t3s_options() {
        let discovery = parse_discovery(&discovery_json());
        assert_eq!(discovery.tools.len(), 2);
        assert_eq!(
            discovery.tools[0].version.as_deref(),
            Some("git version 2.47.1")
        );
        assert!(!discovery.tools[1].available);
        let [github, gitlab, bitbucket] = discovery.forges.as_slice() else {
            panic!("three forges");
        };
        assert_eq!(github.auth.status, AuthStatus::Authenticated);
        assert_eq!(
            github.auth.accounts[0].environment_variable.as_deref(),
            Some("GH_TOKEN")
        );
        assert_eq!(
            summary(github),
            "Signed in as octocat · Using GH_TOKEN from the server environment"
        );
        assert_eq!(login_command(github), Some("gh auth login"));
        assert_eq!(
            summary(gitlab),
            "GitLab isn't signed in on T3 Code's machine. Sign in there with glab, or give it \
             credentials, so agents can open and update pull requests."
        );
        assert_eq!(login_command(gitlab), Some("glab auth login"));
        assert_eq!(
            summary(bitbucket),
            "Not available on T3 Code's machine. Add a Bitbucket token in T3 Code's settings."
        );
        assert_eq!(login_command(bitbucket), None);
        // Plain strings and missing fields read too.
        let plain = parse_discovery(&json!({ "sourceControlProviders": [
            { "kind": "github", "label": "GitHub", "executable": "gh", "status": "available",
              "auth": { "status": "authenticated", "account": "octocat" } }
        ] }));
        assert_eq!(summary(&plain.forges[0]), "Signed in as octocat");
    }

    #[gpui::test]
    async fn accounts_are_read_and_older_servers_say_so(cx: &mut TestAppContext) {
        let backend = FakeT3::new();
        backend.answer("server.discoverSourceControl", discovery_json());
        let accounts = cx.new(|cx| {
            SourceControlAccounts::new(Some(backend.clone() as Arc<dyn ThreadBackend>), cx)
        });
        cx.run_until_parked();
        accounts.read_with(cx, |accounts, _| {
            assert_eq!(accounts.discovery().unwrap().forges.len(), 3);
        });
        accounts.update(cx, |accounts, cx| accounts.rescan(cx));
        cx.run_until_parked();
        assert_eq!(
            backend.calls(),
            vec![Call::new("server.discoverSourceControl", json!({}), Access::Read); 2]
        );

        let older = FakeT3::new();
        older.unsupported("server.discoverSourceControl");
        let accounts =
            cx.new(|cx| SourceControlAccounts::new(Some(older as Arc<dyn ThreadBackend>), cx));
        cx.run_until_parked();
        accounts.read_with(cx, |accounts, _| {
            assert!(matches!(accounts.discovery, Loadable::Unsupported))
        });
    }
}
