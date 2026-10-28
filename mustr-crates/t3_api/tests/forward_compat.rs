//! Values a newer T3 server may send, decoded by this build's types.

use serde_json::{Value, json};
use t3_api::tagged::{OrchestrationV2ThreadShellForkedFrom, ProjectProjectIcon, ServerConfigIssue};
use t3_api::types::ServerConfigKeybindingsUpdatedPayload;
use t3_api::types::{
    ExecutionEnvironmentCapabilities, ExecutionEnvironmentPlatform, KeybindingCommand,
    OrchestrationV2PendingRuntimeRequestSummaryKind, PullRequestAction, PullRequestCapabilities,
    ScheduledTaskWebhookFilter, ServerInstallation,
};
use t3_api::{ForwardCompatible as _, forward_compat};

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).expect("decodes")
}

#[test]
fn unknown_string_enum_values_are_kept_and_sent_back() {
    let action: PullRequestAction = decode(json!("archive"));
    assert_eq!(
        action,
        PullRequestAction::Unrecognized("archive".to_string())
    );
    assert!(action.is_unrecognized());
    assert_eq!(serde_json::to_value(&action).unwrap(), json!("archive"));

    let known: PullRequestAction = decode(json!("merge"));
    assert_eq!(known, PullRequestAction::Merge);
    assert_eq!(known.as_str(), "merge");
    assert_eq!(
        "merge".parse::<PullRequestAction>(),
        Ok(PullRequestAction::Merge)
    );
}

#[test]
fn forward_compatible_arrays_drop_what_t3_drops() {
    let capabilities: PullRequestCapabilities = decode(json!({
        "diff": true, "comment": true, "search": true,
        "actions": ["close", "archive", "merge"],
        "mergeMethods": ["squash", "fast-forward"],
        "updateMethods": ["rebase", "fast-forward"],
        "review": { "inlineComment": true, "reply": true, "resolve": true, "verdicts": ["approve", "block"] },
        "reviewers": { "request": true, "listCandidates": true }
    }));
    assert_eq!(
        capabilities.actions,
        [PullRequestAction::Close, PullRequestAction::Merge]
    );
    assert_eq!(capabilities.merge_methods.len(), 1);
    assert_eq!(capabilities.update_methods.as_ref().map(Vec::len), Some(1));
    assert_eq!(capabilities.review.verdicts.len(), 1);
}

#[test]
fn forward_compatible_optionals_become_none() {
    let capabilities: ExecutionEnvironmentCapabilities = decode(json!({
        "repositoryIdentity": true,
        "serverInstallation": { "kind": "future-manager" }
    }));
    assert_eq!(capabilities.server_installation, None);

    let capabilities: ExecutionEnvironmentCapabilities = decode(json!({
        "serverInstallation": { "kind": "npm-global", "prefix": "/opt/node" }
    }));
    assert!(matches!(
        capabilities.server_installation,
        Some(ServerInstallation::Variant1 { .. })
    ));

    let platform: ExecutionEnvironmentPlatform =
        decode(json!({ "os": "linux", "arch": "x64", "machine": "quantum" }));
    assert_eq!(platform.machine, None);
    let platform: ExecutionEnvironmentPlatform = decode(json!({ "os": "linux", "arch": "x64" }));
    assert_eq!(platform.machine, None);
}

#[test]
fn untagged_unions_prefer_an_exact_match() {
    // `KeybindingCommand` is "a known command" | "any string". Without the strict pass,
    // the known-command enum's fallback would capture every string.
    let known: KeybindingCommand = decode(json!("terminal.toggle"));
    assert!(matches!(known, KeybindingCommand::Variant0(_)), "{known:?}");
    let custom: KeybindingCommand = decode(json!("script.my-task.run"));
    assert_eq!(
        custom,
        KeybindingCommand::Variant1("script.my-task.run".to_string())
    );
    // `FromStr` agrees with decoding: "user_input" belongs to the second variant.
    assert!(matches!(
        "user_input".parse::<OrchestrationV2PendingRuntimeRequestSummaryKind>(),
        Ok(OrchestrationV2PendingRuntimeRequestSummaryKind::String(_))
    ));

    // Filters are told apart by `op`; `equals` must not capture `in`.
    let filter: ScheduledTaskWebhookFilter =
        decode(json!({ "op": "in", "path": "a", "value": ["x"] }));
    assert!(matches!(
        filter,
        ScheduledTaskWebhookFilter::Variant1 { .. }
    ));
    // An op this build doesn't know still decodes, through the lenient pass.
    let filter: ScheduledTaskWebhookFilter =
        decode(json!({ "op": "matches", "path": "a", "value": "x" }));
    let ScheduledTaskWebhookFilter::Variant0 { op, .. } = filter else {
        panic!("{filter:?}");
    };
    assert!(op.is_unrecognized());
}

#[test]
fn strict_mode_rejects_unknown_values() {
    assert!(
        forward_compat::strictly(|| serde_json::from_value::<PullRequestAction>(json!("archive")))
            .is_err()
    );
    assert!(!forward_compat::is_strict());
}

#[test]
fn lenient_arrays_of_unions_drop_unknown_tags() {
    // `ForwardCompatibleArray(ServerConfigIssue)`: T3 drops the issue kind it doesn't know.
    let payload: ServerConfigKeybindingsUpdatedPayload = decode(json!({
        "issues": [
            { "kind": "keybindings.malformed-config", "message": "bad" },
            { "kind": "keybindings.from-the-future", "message": "new" },
            { "kind": "keybindings.invalid-entry", "message": "broken" }
        ],
        "keybindings": []
    }));
    assert_eq!(payload.issues.len(), 1);
    assert!(matches!(
        payload.issues[0],
        ServerConfigIssue::KeybindingsMalformedConfig(_)
    ));
}

#[test]
fn nullable_unions_have_a_fallback() {
    // `forkedFrom: NullOr(Union([...]))` on every thread in the shell snapshot.
    let forked: Option<OrchestrationV2ThreadShellForkedFrom> =
        decode(json!({ "type": "workflow", "workflowId": "w1" }));
    let forked = forked.expect("not null");
    assert!(forked.is_unrecognized());
    assert_eq!(forked.tag(), Some("workflow"));
    assert_eq!(
        serde_json::to_value(&forked).unwrap(),
        json!({ "type": "workflow", "workflowId": "w1" })
    );

    // T3's `ReceivedProjectIcon` is a `ForwardCompatibleUnion`: open even when strict.
    let icon: ProjectProjectIcon =
        forward_compat::strictly(|| serde_json::from_value(json!({ "kind": "sticker", "id": 7 })))
            .expect("open unions accept unknown tags");
    assert!(icon.is_unrecognized());
}
