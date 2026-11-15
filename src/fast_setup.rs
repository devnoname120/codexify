//! Shared fast-setup policy for the wizard and the owner settings UI.
//!
//! The preset stays conservative: browser simplification (Simplified/Bones),
//! infinite retries, turn renewal and the conversation-load interval are
//! separate explicit opt-ins and keep their defaults here.
use serde_json::{Map, Value, json};

/// Settings that fast setup and quickstart must never choose for the user.
#[cfg(test)]
const SEPARATE_OPT_INS: [&str; 6] = [
    "agentChat.automation.browserSimplicity",
    "agentChat.automation.minimalUi",
    "agentChat.automation.retryTimings.infiniteRetries",
    "agentChat.automation.retryTimings.conversationLoadIntervalSeconds",
    "agentChat.automation.browserActionLanes",
    "agentChat.turnRenewal.enabled",
];

pub(crate) fn changes() -> Map<String, Value> {
    serde_json::from_value(json!({
        "agentChat.enabled": true,
        "agentChat.defaultMessageMode": "steer",
        "agentChat.rolloutEnabled": true,
        "agentChat.extras.animations": false,
        "agentChat.extras.sounds": false,
        "agentChat.extras.weeklyRecap": false,
        "agentChat.automation.enabled": true,
        "agentChat.automation.recoveryEnhancements": true,
        "agentChat.automation.automaticRetries": true,
        "agentChat.features.agentTeams": true,
        "agentChat.features.autoMode": true,
        "agentChat.features.conversationRecovery": true
    }))
    .expect("fast setup fields")
}

pub(crate) fn snapshot(document: &Value, defaults: &Value) -> Value {
    let changes = changes();
    let applied = changes.iter().all(|(path, expected)| {
        let pointer = format!("/{}", path.replace('.', "/"));
        document
            .pointer(&pointer)
            .filter(|value| !value.is_null())
            .or_else(|| defaults.pointer(&pointer))
            == Some(expected)
    });
    json!({"changes": changes, "applied": applied})
}

/// Quickstart supplies good initial choices, without rewriting existing ones.
/// The button in Settings is an explicit opt-in and applies the whole preset.
pub(crate) fn apply_missing(document: &mut Map<String, Value>) -> Result<(), String> {
    for (path, value) in changes() {
        let parts = path.split('.').collect::<Vec<_>>();
        insert_missing(document, &parts, value)?;
    }
    Ok(())
}

fn insert_missing(
    object: &mut Map<String, Value>,
    parts: &[&str],
    value: Value,
) -> Result<(), String> {
    if parts.len() == 1 {
        object.entry(parts[0]).or_insert(value);
        return Ok(());
    }
    let child = object.entry(parts[0]).or_insert_with(|| json!({}));
    let child = child
        .as_object_mut()
        .ok_or_else(|| format!("{} must be an object", parts[0]))?;
    insert_missing(child, &parts[1..], value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_setup_is_fast_without_changing_models_or_exposing_external_control() {
        let mut document = Map::new();
        apply_missing(&mut document).unwrap();
        let document = Value::Object(document);
        assert_eq!(document["agentChat"]["defaultMessageMode"], "steer");
        for path in SEPARATE_OPT_INS {
            assert!(!changes().contains_key(path), "{path}");
            let pointer = format!("/{}", path.replace('.', "/"));
            assert!(document.pointer(&pointer).is_none(), "{path}");
        }
        assert!(
            snapshot(&document, &json!({}))["applied"]
                .as_bool()
                .unwrap()
        );
        let encoded = document.to_string();
        assert!(!encoded.contains("externalAgentControl"));
        assert!(!encoded.contains("autoApproveToolCalls"));
        assert!(!encoded.contains("model"));
        assert!(!encoded.contains("effort"));
    }

    #[test]
    fn quickstart_preserves_user_choices_limits_and_legacy_presentation() {
        let mut document = json!({"agentChat": {
            "defaultMessageMode": "queue", "rolloutEnabled": false,
            "automation": {"minimalUi": false, "automaticRetries": false,
                "retryTimings": {"conversationLoadIntervalSeconds": 37, "infiniteRetries": false}},
            "features": {"agentTeams": false},
            "turnRenewal": {"enabled": false, "limits": [
                {"model":"gpt-6-sol", "effort":"high", "minutes":41, "enabled":false}
            ]}
        }})
        .as_object()
        .unwrap()
        .clone();
        let before = Value::Object(document.clone());
        apply_missing(&mut document).unwrap();
        let document = Value::Object(document);
        for pointer in [
            "/agentChat/defaultMessageMode",
            "/agentChat/rolloutEnabled",
            "/agentChat/automation/minimalUi",
            "/agentChat/automation/automaticRetries",
            "/agentChat/automation/retryTimings/conversationLoadIntervalSeconds",
            "/agentChat/automation/retryTimings/infiniteRetries",
            "/agentChat/features/agentTeams",
            "/agentChat/turnRenewal",
        ] {
            assert_eq!(
                document.pointer(pointer),
                before.pointer(pointer),
                "{pointer}"
            );
        }
        assert!(
            document
                .pointer("/agentChat/automation/browserSimplicity")
                .is_none()
        );
        assert_eq!(snapshot(&document, &json!({}))["applied"], false);
    }

    #[test]
    fn preset_uses_effective_values_and_leaves_turn_renewal_untouched() {
        let defaults = Value::Object({
            let mut document = Map::new();
            apply_missing(&mut document).unwrap();
            document
        });
        assert_eq!(snapshot(&json!({}), &defaults)["applied"], true);
        let mut document = json!({"agentChat":{"turnRenewal":{
            "limits":[{"model":"custom", "effort":"high", "minutes":73, "enabled":false}]
        }}})
        .as_object()
        .unwrap()
        .clone();
        apply_missing(&mut document).unwrap();
        assert_eq!(
            document["agentChat"]["turnRenewal"]["limits"][0]["minutes"],
            73
        );
        assert_eq!(
            document["agentChat"]["turnRenewal"]["limits"][0]["enabled"],
            false
        );
        assert!(
            document["agentChat"]["turnRenewal"]
                .get("enabled")
                .is_none()
        );
        assert!(!changes().contains_key("agentChat.turnRenewal.limits"));
    }
}
