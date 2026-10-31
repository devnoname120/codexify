use serde_json::{Map, Value, json};

#[cfg(test)]
use super::USER_MESSAGE_FIELD;
use crate::types::{ToolContent, ToolResult};

pub(crate) fn timestamped_user_message(markdown: &str, at_ms: Option<u64>) -> String {
    let Some(at_ms) = at_ms else {
        return markdown.to_owned();
    };
    let time = i64::try_from(at_ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| format!("Unix time {at_ms} ms"));
    format!("[{time}]\n{markdown}")
}

fn needs_envelope(schema: Option<&Value>, field: &str) -> bool {
    let Some(schema) = schema.and_then(Value::as_object) else {
        return false;
    };
    schema.get("additionalProperties") != Some(&Value::Bool(false))
        || schema
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|properties| properties.contains_key(field))
        || schema.keys().any(|key| {
            !matches!(
                key.as_str(),
                "$schema"
                    | "$id"
                    | "$defs"
                    | "definitions"
                    | "$comment"
                    | "title"
                    | "description"
                    | "type"
                    | "properties"
                    | "required"
                    | "additionalProperties"
            )
        })
}

#[cfg(test)]
pub(crate) fn schema(original: Option<Value>) -> Value {
    schema_with_field(
        original,
        USER_MESSAGE_FIELD,
        "Complete new user text from this Codexify task's CHAT.md, never truncated by Codexify. Omitted when no user text is pending. Ordinary tool results do not acknowledge it; call chat_read.",
    )
}

pub(crate) const USER_MESSAGE_DESCRIPTION: &str = "Complete new user text from this Codexify task's CHAT.md, never truncated by Codexify. Omitted when no user text is pending. Ordinary tool results do not acknowledge it; call chat_read.";

#[cfg(test)]
pub(crate) fn schema_with_field(original: Option<Value>, field: &str, description: &str) -> Value {
    schema_with_fields(original, &[(field, description)])
}

pub(crate) fn schema_with_fields(original: Option<Value>, fields: &[(&str, &str)]) -> Value {
    let mut augmented = if fields
        .iter()
        .any(|(field, _)| needs_envelope(original.as_ref(), field))
    {
        // The original contract is validated before wrapping; its root-local refs must not move.
        json!({"type":"object", "properties":{"upstream_result":{}}, "additionalProperties":false})
    } else {
        original.unwrap_or_else(
            || json!({"type":"object", "properties":{}, "additionalProperties":true}),
        )
    };
    let object = augmented
        .as_object_mut()
        .expect("registered output schema is an object");
    let properties = object
        .entry("properties")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("output properties must be an object");
    for (field, description) in fields {
        properties.insert(
            (*field).into(),
            json!({
                "type":"string",
                "description":description
            }),
        );
    }
    augmented
}

#[cfg(test)]
pub(crate) fn attach(result: &mut ToolResult, original_schema: Option<&Value>) {
    let message = result.new_chat_message_from_user.take();
    attach_field(result, original_schema, USER_MESSAGE_FIELD, message);
}

#[cfg(test)]
pub(crate) fn attach_field(
    result: &mut ToolResult,
    original_schema: Option<&Value>,
    field: &str,
    message: Option<String>,
) {
    attach_fields(result, original_schema, vec![(field, message)]);
}

pub(crate) fn attach_fields(
    result: &mut ToolResult,
    original_schema: Option<&Value>,
    fields: Vec<(&str, Option<String>)>,
) {
    let original = result.structured_content.take();
    // Unstructured MCP text tools (catalog/gateway) have no output schema.
    // Do not turn their existing model-visible text into an empty JSON object
    // when optional Markdown-chat notice fields are absent.
    let text_fallback = if original_schema.is_none()
        && original
            .as_ref()
            .and_then(Value::as_object)
            .is_none_or(|object| !object.contains_key("content"))
    {
        let text = result.joined_text();
        (!text.is_empty()).then_some(text)
    } else {
        None
    };
    let wrap = fields
        .iter()
        .any(|(field, _)| needs_envelope(original_schema, field))
        || original.as_ref().is_some_and(|value| {
            !value.is_object() || fields.iter().any(|(field, _)| value.get(*field).is_some())
        });
    let mut object = if wrap {
        let mut object = Map::new();
        if let Some(original) = original {
            object.insert("upstream_result".into(), original);
        }
        object
    } else {
        original
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default()
    };
    if let Some(text) = text_fallback {
        object.insert("content".into(), Value::String(text));
    }
    for (field, message) in fields {
        if let Some(message) = message.filter(|message| !message.is_empty()) {
            object.insert(field.into(), Value::String(message.clone()));
            // Some MCP hosts only deliver content blocks to the model.
            result
                .content
                .push(ToolContent::Text(json!({field:message}).to_string()));
        }
    }
    result.structured_content = Some(Value::Object(object));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_times_are_exact_and_unknown_times_are_not_invented() {
        assert_eq!(
            timestamped_user_message("  body\n", Some(0)),
            "[1970-01-01T00:00:00.000Z]\n  body\n"
        );
        assert_eq!(timestamped_user_message("  body\n", None), "  body\n");
        assert_eq!(
            timestamped_user_message("body", Some(u64::MAX)),
            format!("[Unix time {} ms]\nbody", u64::MAX)
        );
    }

    #[test]
    fn unstructured_tool_text_survives_empty_notice_envelope() {
        let mut result = ToolResult::text("workspace inspection: OK");
        attach_fields(
            &mut result,
            None,
            vec![(USER_MESSAGE_FIELD, None), ("workspace_changed", None)],
        );
        assert_eq!(
            result.structured_content,
            Some(json!({"content": "workspace inspection: OK"}))
        );
        assert_eq!(result.joined_text(), "workspace inspection: OK");
        assert!(jsonschema::is_valid(
            &schema(None),
            result.structured_content.as_ref().unwrap()
        ));
    }

    #[test]
    fn unstructured_tool_text_and_user_notice_are_both_visible() {
        let mut result = ToolResult::text("root=C:/approved");
        attach_fields(
            &mut result,
            None,
            vec![(USER_MESSAGE_FIELD, Some("user says hello".into()))],
        );
        assert_eq!(
            result.structured_content.as_ref().unwrap()["content"],
            "root=C:/approved"
        );
        assert_eq!(
            result.structured_content.as_ref().unwrap()[USER_MESSAGE_FIELD],
            "user says hello"
        );
    }

    #[test]
    fn upstream_structured_content_is_unchanged_by_notice_envelope() {
        let mut result =
            ToolResult::text("upstream text").with_structured(json!({"owner": "upstream"}));
        attach_fields(&mut result, None, vec![(USER_MESSAGE_FIELD, None)]);
        assert_eq!(
            result.structured_content,
            Some(json!({"owner": "upstream", "content": "upstream text"}))
        );
        assert_eq!(result.joined_text(), "upstream text");
    }

    #[test]
    fn upstream_metadata_does_not_hide_text_in_chat_result() {
        let mut result = ToolResult::text("nodeRepl.write: inspection worked")
            .with_structured(json!({"execution_duration_ms": 0}));
        attach_fields(&mut result, None, vec![(USER_MESSAGE_FIELD, None)]);
        assert_eq!(
            result.structured_content,
            Some(json!({
                "execution_duration_ms": 0,
                "content": "nodeRepl.write: inspection worked"
            }))
        );
    }

    #[test]
    fn upstream_content_field_is_not_overwritten() {
        let mut result = ToolResult::text("extra text")
            .with_structured(json!({"content": "original upstream content"}));
        attach_fields(&mut result, None, vec![(USER_MESSAGE_FIELD, None)]);
        assert_eq!(
            result.structured_content,
            Some(json!({"content": "original upstream content"}))
        );
        assert_eq!(result.joined_text(), "extra text");
    }

    #[test]
    fn unstructured_error_is_not_hidden_as_empty_json() {
        let mut result = ToolResult::error("permission denied");
        attach_fields(&mut result, None, vec![(USER_MESSAGE_FIELD, None)]);
        assert!(result.is_error);
        assert_eq!(
            result.structured_content,
            Some(json!({"content": "permission denied"}))
        );
    }

    #[test]
    fn native_object_keeps_its_shape_and_optional_chat_field() {
        let original = crate::tool::text_output_schema();
        let augmented = schema(Some(original.clone()));
        assert!(jsonschema::is_valid(
            &augmented,
            &json!({"content":"original"})
        ));
        let mut result =
            ToolResult::text("original").with_structured(json!({"content":"original"}));
        result.new_chat_message_from_user = Some("User\n".repeat(100_000));
        attach(&mut result, Some(&original));
        assert!(jsonschema::is_valid(
            &augmented,
            result.structured_content.as_ref().unwrap()
        ));
        assert_eq!(
            result.structured_content.as_ref().unwrap()["content"],
            "original"
        );
        assert_eq!(
            result.structured_content.as_ref().unwrap()[USER_MESSAGE_FIELD]
                .as_str()
                .unwrap()
                .len(),
            500_000
        );
    }

    #[test]
    fn chat_and_workspace_notices_share_one_top_level_envelope() {
        for original in [
            None,
            Some(json!({"type":"object","additionalProperties":true})),
            Some(crate::tool::text_output_schema()),
        ] {
            let schema = schema_with_fields(
                original.clone(),
                &[
                    (USER_MESSAGE_FIELD, "user"),
                    ("workspace_changed", "workspace"),
                ],
            );
            let mut result =
                ToolResult::text("original").with_structured(json!({"content":"original"}));
            attach_fields(
                &mut result,
                original.as_ref(),
                vec![
                    (USER_MESSAGE_FIELD, Some("full user text".into())),
                    ("workspace_changed", Some("new workspace".into())),
                ],
            );
            let value = result.structured_content.unwrap();
            assert_eq!(value[USER_MESSAGE_FIELD], "full user text");
            assert_eq!(value["workspace_changed"], "new workspace");
            assert!(jsonschema::is_valid(&schema, &value));
        }
    }

    #[test]
    fn absent_message_is_omitted_and_unstructured_content_is_preserved() {
        let mut result = ToolResult::image("image-data", "image/png");
        attach(&mut result, None);
        assert_eq!(result.structured_content, Some(json!({})));
        assert!(
            matches!(&result.content[0], ToolContent::Image { data, .. } if data == "image-data")
        );
        assert!(jsonschema::is_valid(
            &schema(None),
            result.structured_content.as_ref().unwrap()
        ));
    }

    #[test]
    fn arbitrary_upstream_values_and_reserved_fields_are_not_overwritten() {
        for original in [
            json!([1, 2]),
            json!("scalar"),
            Value::Null,
            json!({USER_MESSAGE_FIELD:17}),
        ] {
            let mut result = ToolResult::text("upstream").with_structured(original.clone());
            result.new_chat_message_from_user = Some("real user".into());
            attach(&mut result, None);
            let value = result.structured_content.unwrap();
            assert_eq!(value["upstream_result"], original);
            assert_eq!(value[USER_MESSAGE_FIELD], "real user");
            assert!(jsonschema::is_valid(&schema(None), &value));
        }
    }

    #[test]
    fn closed_upstream_combinators_and_collisions_use_a_valid_envelope() {
        for original in [
            json!({"type":"object", "properties":{"content":{"type":"string"}}, "required":["content"], "additionalProperties":true}),
            json!({"type":"object", "allOf":[{"type":"object", "properties":{"value":{"type":"integer"}}, "additionalProperties":false}]}),
            json!({"type":"object", "properties":{USER_MESSAGE_FIELD:{"type":"integer"}}, "additionalProperties":false}),
        ] {
            let mut result = ToolResult::text("upstream").with_structured(json!({"value":1}));
            result.new_chat_message_from_user = Some("user".into());
            attach(&mut result, Some(&original));
            let value = result.structured_content.unwrap();
            assert_eq!(value["upstream_result"], json!({"value":1}));
            assert!(jsonschema::is_valid(&schema(Some(original)), &value));
        }
    }
}
