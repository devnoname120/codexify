use serde_json::{Map, Value, json};

#[cfg(test)]
use super::USER_MESSAGE_FIELD;
use crate::types::{ToolContent, ToolResult};

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
        "Complete new user text from this conversation's CHAT.md, never truncated by Codexify. Omitted when no user text is pending. Ordinary tool results do not acknowledge it; call chat_read.",
    )
}

pub(crate) const USER_MESSAGE_DESCRIPTION: &str = "Complete new user text from this conversation's CHAT.md, never truncated by Codexify. Omitted when no user text is pending. Ordinary tool results do not acknowledge it; call chat_read.";

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
