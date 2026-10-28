//! The composer's `/` and `$` menus, as T3's composer has them (`chat/ComposerCommandMenu.tsx`,
//! `chat/composerSlashCommandSearch.ts`, `providerSkillSearch.ts` and
//! `chat/useComposerMenuItems.ts` in T3's web app).
//!
//! `/` at the start of a message lists T3's own commands, `/model` (the model picker), `/plan`
//! and `/default` (the thread's interaction mode), then the agent's commands and skills from
//! `server.getConfig`, such as `/compact` or `/goal`, which go into the text for the agent.
//! `$` searches the agent's skills. A `/plan` or `/default` sent alone switches the mode instead
//! of sending.

use crate::{
    commands,
    mentions::{MatchBases, Trigger, score_query_match},
    settings_bar::ToggleModelPicker,
    thread_settings::selection_instance_id,
    thread_view::ThreadView,
};
use gpui::{App, Context, SharedString, Window};
use t3_api::types::{ProviderInteractionMode, ServerProviderSkill, ServerProviderSlashCommand};
use ui::IconName;

/// T3's own commands, which the composer runs itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Builtin {
    /// Opens the model picker.
    Model,
    /// Switches the thread into plan mode.
    Plan,
    /// Switches the thread back to building.
    Default,
}

impl Builtin {
    fn name(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Plan => "plan",
            Self::Default => "default",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Model => "Switch response model for this thread",
            Self::Plan => "Switch this thread into plan mode",
            Self::Default => "Switch this thread back to normal build mode",
        }
    }
}

/// One row of the `/` or `$` menu.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SlashItem {
    Builtin(Builtin),
    /// A command of the agent's, sent as text.
    Provider(ServerProviderSlashCommand),
    /// A skill, from `/` (as `/skill:name`) or from `$`.
    Skill {
        skill: ServerProviderSkill,
        from_slash: bool,
    },
}

/// Where a skill comes from, as T3 badges it (`resolveProviderSkillSourceKind`).
fn skill_source(skill: &ServerProviderSkill) -> &'static str {
    let path = skill.path.replace('\\', "/");
    if path.contains("/.codex/plugins/") || path.contains("/.agents/plugins/") {
        return "App";
    }
    match skill
        .scope
        .as_deref()
        .map(|scope| scope.trim().to_lowercase())
        .as_deref()
    {
        Some("repo" | "repository") => "Repo",
        Some("project" | "workspace" | "local") => "Project",
        Some("user" | "personal") => "Personal",
        Some("system") => "System",
        _ => "Provider",
    }
}

/// A skill's name as T3 shows it: its display name, or its name in title case
/// (`formatProviderSkillDisplayName`).
fn skill_display_name(skill: &ServerProviderSkill) -> String {
    if let Some(display_name) = skill
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        return display_name.to_string();
    }
    skill
        .name
        .split(|c: char| c.is_whitespace() || matches!(c, ':' | '_' | '-'))
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl SlashItem {
    pub(crate) fn id(&self) -> String {
        match self {
            Self::Builtin(builtin) => format!("slash:{}", builtin.name()),
            Self::Provider(command) => format!("provider-slash-command:{}", command.name),
            Self::Skill { skill, .. } => format!("skill:{}", skill.name),
        }
    }

    pub(crate) fn label(&self) -> SharedString {
        match self {
            Self::Builtin(builtin) => format!("/{}", builtin.name()).into(),
            Self::Provider(command) => format!("/{}", command.name).into(),
            Self::Skill {
                skill,
                from_slash: true,
            } => format!("/skill:{}", skill_display_name(skill)).into(),
            Self::Skill { skill, .. } => skill_display_name(skill).into(),
        }
    }

    pub(crate) fn description(&self) -> SharedString {
        match self {
            Self::Builtin(builtin) => builtin.description().into(),
            Self::Provider(command) => command
                .description
                .clone()
                .or_else(|| command.input.as_ref().map(|input| input.hint.clone()))
                .unwrap_or_else(|| "Run provider command".to_string())
                .into(),
            Self::Skill { skill, from_slash } => skill
                .short_description
                .clone()
                .or_else(|| skill.description.clone())
                .unwrap_or_else(|| match &skill.scope {
                    Some(scope) => format!("{scope} skill"),
                    None if *from_slash => String::new(),
                    None => "Run provider skill".to_string(),
                })
                .into(),
        }
    }

    pub(crate) fn icon(&self) -> IconName {
        match self {
            Self::Builtin(Builtin::Model) => IconName::Sparkle,
            Self::Builtin(_) => IconName::ListTree,
            Self::Provider(_) => IconName::Slash,
            Self::Skill { .. } => IconName::Book,
        }
    }

    /// Where a skill comes from (`SkillSourceBadge`).
    pub(crate) fn badge(&self) -> Option<SharedString> {
        match self {
            Self::Skill { skill, from_slash } => {
                let source = skill_source(skill);
                Some(if *from_slash {
                    source.into()
                } else {
                    format!("{source} Skill").into()
                })
            }
            _ => None,
        }
    }
}

/// What the agent of a thread offers the `/` and `$` menus.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SlashContext {
    pub skills: Vec<ServerProviderSkill>,
    pub commands: Vec<ServerProviderSlashCommand>,
    /// The agent has a plan mode (T3's `showInteractionModeToggle`).
    pub plan_mode: bool,
    /// The `/` starts the message; agents expand their commands only there.
    pub at_prompt_start: bool,
    /// Nothing else is in the message, so `/compact` can be offered.
    pub compact_available: bool,
}

/// The skills a user can start: enabled, not agent-only, once per name (`isProviderSkillUserInvocable`,
/// `dedupeProviderSkillsByName`).
fn invocable_skills(skills: &[ServerProviderSkill]) -> Vec<ServerProviderSkill> {
    let mut seen = Vec::new();
    skills
        .iter()
        .filter(|skill| skill.enabled && skill.user_invocable != Some(false))
        .filter(|skill| {
            let name = skill.name.trim().to_lowercase();
            if seen.contains(&name) {
                false
            } else {
                seen.push(name);
                true
            }
        })
        .cloned()
        .collect()
}

/// How well `query` (lowercased) matches a skill (`scoreProviderSkill`).
fn score_skill(skill: &ServerProviderSkill, query: &str) -> Option<usize> {
    let default_markers = [' ', '-', '_', '/'];
    let name_markers = ['-', '_', '/'];
    let field = |value: Option<&str>, bases: MatchBases, markers: &[char]| {
        score_query_match(
            &value.unwrap_or_default().to_lowercase(),
            query,
            bases,
            markers,
        )
    };
    [
        field(
            Some(&skill.name),
            MatchBases {
                exact: 0,
                prefix: Some(2),
                boundary: Some(4),
                includes: Some(6),
                fuzzy: Some(100),
            },
            &name_markers,
        ),
        field(
            Some(&skill_display_name(skill)),
            MatchBases {
                exact: 1,
                prefix: Some(3),
                boundary: Some(5),
                includes: Some(7),
                fuzzy: Some(110),
            },
            &default_markers,
        ),
        field(
            skill.short_description.as_deref(),
            MatchBases {
                exact: 20,
                prefix: Some(22),
                boundary: Some(24),
                includes: Some(26),
                fuzzy: None,
            },
            &default_markers,
        ),
        field(
            skill.description.as_deref(),
            MatchBases {
                exact: 30,
                prefix: Some(32),
                boundary: Some(34),
                includes: Some(36),
                fuzzy: None,
            },
            &default_markers,
        ),
        field(
            skill.scope.as_deref(),
            MatchBases {
                exact: 40,
                prefix: Some(42),
                boundary: None,
                includes: Some(44),
                fuzzy: None,
            },
            &default_markers,
        ),
    ]
    .into_iter()
    .flatten()
    .min()
}

fn score_slash_item(item: &SlashItem, query: &str) -> Option<usize> {
    if let SlashItem::Skill { skill, .. } = item {
        if query == "skill" {
            return Some(0);
        }
        let skill_query = query.strip_prefix("skill:").unwrap_or(query);
        let score = if skill_query.is_empty() {
            Some(0)
        } else {
            score_skill(skill, skill_query)
        };
        return score.or_else(|| "skill".starts_with(query).then_some(usize::MAX));
    }
    let name = match item {
        SlashItem::Builtin(builtin) => builtin.name().to_string(),
        SlashItem::Provider(command) => command.name.to_lowercase(),
        SlashItem::Skill { .. } => unreachable!(),
    };
    [
        score_query_match(
            &name,
            query,
            MatchBases {
                exact: 0,
                prefix: Some(2),
                boundary: Some(4),
                includes: Some(6),
                fuzzy: Some(100),
            },
            &['-', '_', '/'],
        ),
        score_query_match(
            &item.description().to_lowercase(),
            query,
            MatchBases {
                exact: 20,
                prefix: Some(22),
                boundary: Some(24),
                includes: Some(26),
                fuzzy: None,
            },
            &[' ', '-', '_', '/'],
        ),
    ]
    .into_iter()
    .flatten()
    .min()
}

/// The `/` menu for `query`: T3's commands, the agent's commands at the start of a message, and
/// its skills, best first (`searchSlashCommandItems`).
pub(crate) fn slash_items(context: &SlashContext, query: &str) -> Vec<SlashItem> {
    let mut items = vec![SlashItem::Builtin(Builtin::Model)];
    if context.plan_mode {
        items.push(SlashItem::Builtin(Builtin::Plan));
        items.push(SlashItem::Builtin(Builtin::Default));
    }
    let skills = invocable_skills(&context.skills);
    if context.at_prompt_start {
        items.extend(
            context
                .commands
                .iter()
                .filter(|command| {
                    let name = command.name.trim().to_lowercase();
                    !skills
                        .iter()
                        .any(|skill| skill.name.trim().to_lowercase() == name)
                        && (command.name != "compact" || context.compact_available)
                })
                .cloned()
                .map(SlashItem::Provider),
        );
    }
    items.extend(skills.into_iter().map(|skill| SlashItem::Skill {
        skill,
        from_slash: true,
    }));
    let query = query.trim().trim_start_matches('/').to_lowercase();
    if query.is_empty() {
        return items;
    }
    let tie_breaker = |item: &SlashItem| match item {
        SlashItem::Builtin(builtin) => format!("0\0{}", builtin.name()),
        SlashItem::Provider(command) => format!("1\0{}", command.name),
        SlashItem::Skill { skill, .. } => format!("2\0{}", skill.name),
    };
    let mut ranked = items
        .into_iter()
        .filter_map(|item| Some((score_slash_item(&item, &query)?, tie_breaker(&item), item)))
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    ranked.into_iter().map(|(_, _, item)| item).collect()
}

/// The `$` menu for `query`: the agent's skills, best first (`searchProviderSkills`).
pub(crate) fn skill_items(context: &SlashContext, query: &str) -> Vec<SlashItem> {
    let skills = invocable_skills(&context.skills);
    let query = query
        .trim()
        .trim_start_matches(|c: char| c == '$' || "€£¥¢₹₩₽₺₫₪".contains(c))
        .to_lowercase();
    let into_item = |skill| SlashItem::Skill {
        skill,
        from_slash: false,
    };
    if query.is_empty() {
        return skills.into_iter().map(into_item).collect();
    }
    let mut ranked = skills
        .into_iter()
        .filter_map(|skill| {
            let score = score_skill(&skill, &query)?;
            let tie_breaker = format!(
                "{}\0{}",
                skill_display_name(&skill).to_lowercase(),
                skill.name
            );
            Some((score, tie_breaker, skill))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    ranked
        .into_iter()
        .map(|(_, _, skill)| into_item(skill))
        .collect()
}

/// `/plan` or `/default` sent alone, which switches the mode instead of sending
/// (`parseStandaloneComposerSlashCommand`).
pub(crate) fn standalone_mode_command(text: &str) -> Option<Builtin> {
    let command = text.trim().strip_prefix('/')?.trim_end();
    if command.eq_ignore_ascii_case("plan") {
        Some(Builtin::Plan)
    } else if command.eq_ignore_ascii_case("default") {
        Some(Builtin::Default)
    } else {
        None
    }
}

impl ThreadView {
    /// The agent of the thread or draft, by instance id, and whether it has a plan mode.
    fn composer_agent(&self, cx: &App) -> Option<(String, bool)> {
        if let Some(form) = &self.new_thread {
            let form = form.read(cx);
            let (agent, _) = form.model()?;
            return Some((agent.instance_id.clone(), agent.plan_mode));
        }
        let thread = &self.thread_state()?.thread;
        let instance_id = selection_instance_id(&thread.model_selection)
            .unwrap_or(thread.provider_instance_id.as_str())
            .to_string();
        let plan_mode = self
            .composer_state
            .menu
            .server_provider(&instance_id)
            .is_none_or(|provider| provider.show_interaction_mode_toggle != Some(false));
        Some((instance_id, plan_mode))
    }

    /// What the `/` and `$` menus offer here: the agent's commands and skills for the thread's
    /// folder when T3 has scanned it, else the agent's own.
    pub(crate) fn slash_context(&self, trigger: &Trigger, cx: &App) -> SlashContext {
        let Some((instance_id, plan_mode)) = self.composer_agent(cx) else {
            return SlashContext::default();
        };
        let text = self.composer.read(cx).text(cx);
        let before = text.get(..trigger.range.start).unwrap_or_default();
        let after = text.get(trigger.range.end..).unwrap_or_default();
        let folder = self
            .thread_state()
            .and_then(|state| state.thread.worktree_path.clone());
        let provider = self.composer_state.menu.server_provider(&instance_id);
        let snapshot = provider.and_then(|provider| {
            let folder = folder.as_deref()?;
            provider
                .workspace_snapshots
                .as_ref()?
                .iter()
                .find(|snapshot| snapshot.cwd == folder)
        });
        SlashContext {
            skills: snapshot
                .map(|snapshot| snapshot.skills.clone())
                .or_else(|| provider.and_then(|provider| provider.skills.clone()))
                .unwrap_or_default(),
            commands: snapshot
                .map(|snapshot| snapshot.slash_commands.clone())
                .or_else(|| provider.and_then(|provider| provider.slash_commands.clone()))
                .unwrap_or_default(),
            plan_mode,
            at_prompt_start: trigger.range.start == 0,
            compact_available: before.trim().is_empty()
                && after.trim().is_empty()
                && self.images.composer.is_empty(),
        }
    }

    /// Runs or writes a picked command or skill.
    pub(crate) fn pick_slash_item(
        &mut self,
        trigger: &Trigger,
        item: SlashItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match item {
            SlashItem::Builtin(Builtin::Model) => {
                self.replace_in_composer(trigger.range.clone(), "", window, cx);
                window.dispatch_action(Box::new(ToggleModelPicker), cx);
            }
            SlashItem::Builtin(builtin) => {
                if !self
                    .composer_agent(cx)
                    .is_some_and(|(_, plan_mode)| plan_mode)
                {
                    return;
                }
                self.replace_in_composer(trigger.range.clone(), "", window, cx);
                self.switch_interaction_mode(builtin, cx);
            }
            SlashItem::Provider(command) => {
                self.replace_in_composer(
                    trigger.range.clone(),
                    &format!("/{} ", command.name),
                    window,
                    cx,
                );
            }
            SlashItem::Skill { skill, .. } => {
                self.replace_in_composer(
                    trigger.range.clone(),
                    &format!("${} ", skill.name),
                    window,
                    cx,
                );
            }
        }
        self.refresh_menu(cx);
        cx.notify();
    }

    /// `/plan` and `/default`: switches an open thread's mode with `thread.interaction-mode.set`,
    /// as the settings bar does, or a draft's.
    pub(crate) fn switch_interaction_mode(&mut self, builtin: Builtin, cx: &mut Context<Self>) {
        let mode = match builtin {
            Builtin::Plan => ProviderInteractionMode::Plan,
            Builtin::Default | Builtin::Model => ProviderInteractionMode::Default,
        };
        if let Some(form) = self.new_thread.clone() {
            form.update(cx, |form, cx| form.set_interaction_mode(mode, cx));
            return;
        }
        let Some(thread) = self.thread_state().map(|state| &state.thread) else {
            return;
        };
        if thread.interaction_mode == mode {
            return;
        }
        let command = commands::set_interaction_mode(&thread.id, mode, commands::new_id());
        let dispatch = self.backend.dispatch(command);
        self.error = None;
        self._tasks.push(cx.spawn(async move |this, cx| {
            if let Err(error) = dispatch.await {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("{error:#}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn skill(name: &str, fields: serde_json::Value) -> ServerProviderSkill {
        let mut skill = json!({ "name": name, "path": format!("/home/me/.claude/skills/{name}"), "enabled": true });
        if let (Some(skill), serde_json::Value::Object(fields)) = (skill.as_object_mut(), fields) {
            skill.extend(fields);
        }
        serde_json::from_value(skill).unwrap()
    }

    fn command(name: &str, description: Option<&str>) -> ServerProviderSlashCommand {
        serde_json::from_value(json!({ "name": name, "description": description })).unwrap()
    }

    fn context() -> SlashContext {
        SlashContext {
            skills: vec![
                skill(
                    "review-pr",
                    json!({ "shortDescription": "Review a pull request", "scope": "user" }),
                ),
                skill(
                    "deploy",
                    json!({ "scope": "project", "displayName": "Ship it" }),
                ),
                skill("hidden", json!({ "userInvocable": false })),
                skill("Review-PR", json!({})),
                skill("off", json!({ "enabled": false })),
            ],
            commands: vec![
                command("compact", Some("Summarize the conversation")),
                command("goal", Some("Set a goal for the agent")),
                command("deploy", Some("Same name as a skill")),
            ],
            plan_mode: true,
            at_prompt_start: true,
            compact_available: true,
        }
    }

    fn labels(items: &[SlashItem]) -> Vec<String> {
        items.iter().map(|item| item.label().to_string()).collect()
    }

    #[test]
    fn slash_lists_t3s_commands_then_the_agents() {
        assert_eq!(
            labels(&slash_items(&context(), "")),
            [
                "/model",
                "/plan",
                "/default",
                "/compact",
                "/goal",
                "/skill:Review Pr",
                "/skill:Ship it"
            ]
        );
        // The agent's commands work only at the start of a message, and `/compact` only alone.
        let mut later = context();
        later.at_prompt_start = false;
        assert_eq!(
            labels(&slash_items(&later, "")),
            [
                "/model",
                "/plan",
                "/default",
                "/skill:Review Pr",
                "/skill:Ship it"
            ]
        );
        let mut busy = context();
        busy.compact_available = false;
        busy.plan_mode = false;
        assert_eq!(
            labels(&slash_items(&busy, "")),
            ["/model", "/goal", "/skill:Review Pr", "/skill:Ship it"]
        );
    }

    #[test]
    fn slash_search_ranks_like_t3() {
        // Descriptions count too, after names.
        assert_eq!(
            labels(&slash_items(&context(), "/mo")),
            ["/model", "/plan", "/default"]
        );
        assert_eq!(labels(&slash_items(&context(), "go")), ["/goal"]);
        assert_eq!(labels(&slash_items(&context(), "plan")), ["/plan"]);
        assert_eq!(
            labels(&slash_items(&context(), "skill")),
            ["/skill:Ship it", "/skill:Review Pr"]
        );
        assert_eq!(
            labels(&slash_items(&context(), "skill:ship")),
            ["/skill:Ship it"]
        );
        assert_eq!(
            slash_items(&context(), "goal")[0].description(),
            "Set a goal for the agent"
        );
    }

    #[test]
    fn dollar_searches_skills() {
        let items = skill_items(&context(), "");
        assert_eq!(labels(&items), ["Review Pr", "Ship it"]);
        assert_eq!(
            items.iter().map(SlashItem::badge).collect::<Vec<_>>(),
            [Some("Personal Skill".into()), Some("Project Skill".into())]
        );
        assert_eq!(labels(&skill_items(&context(), "ship")), ["Ship it"]);
        assert_eq!(labels(&skill_items(&context(), "pull")), ["Review Pr"]);
        assert!(skill_items(&context(), "zzz").is_empty());
    }

    #[test]
    fn plan_and_default_alone_switch_modes() {
        assert_eq!(standalone_mode_command(" /plan "), Some(Builtin::Plan));
        assert_eq!(standalone_mode_command("/DEFAULT"), Some(Builtin::Default));
        assert_eq!(standalone_mode_command("/plan the work"), None);
        assert_eq!(standalone_mode_command("/model"), None);
    }
}
