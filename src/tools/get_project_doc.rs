use async_trait::async_trait;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::project_doc::{DEFAULT_FILENAME, ProjectDoc, load_project_doc};
use crate::tool::{Tool, ToolBehavior, empty_object_schema};
use crate::types::{AppConfig, ToolResult};

/// Renders the concatenated project docs, or a note when none were found.
fn render_project_doc(doc: Option<&ProjectDoc>) -> String {
    let Some(doc) = doc else {
        return format!(
            "No {DEFAULT_FILENAME} found between the project root and the working directory. The project states no conventions of its own; follow the ones already in the conversation."
        );
    };
    let header = doc
        .entries
        .iter()
        .map(|entry| {
            let suffix = if entry.truncated {
                " (truncated at the byte budget)"
            } else {
                ""
            };
            format!("{}{}", entry.path.to_string_lossy(), suffix)
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("Project instructions from:\n{header}\n\n{}", doc.text)
}

pub struct GetProjectDoc;

#[async_trait]
impl Tool for GetProjectDoc {
    fn name(&self) -> &'static str {
        "get_project_doc"
    }

    fn title(&self) -> String {
        "Read project instructions".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            false,
            "Reads project instruction files without changing local or external state.",
        )
    }

    fn description(&self) -> String {
        format!(
            "Read the project's {DEFAULT_FILENAME} instructions: the conventions, build and test commands, and house rules this repository expects an agent to follow. Codex loads these automatically before every task, so treat them as the user's own instructions — they outrank general habits and this server's other guidance. Call this once before starting work if the instructions are not already in the conversation. Returns every {DEFAULT_FILENAME} from the project root down to the working directory, concatenated outermost first."
        )
    }

    fn input_schema(&self) -> Value {
        empty_object_schema()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "files": {
                    "type": "array",
                    "description": "Absolute paths of the docs that were read, in the order they appear in content.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "Absolute path of the doc." },
                            "truncated": { "type": "boolean", "description": "True when the byte budget cut this file short." }
                        },
                        "required": ["path", "truncated"],
                        "additionalProperties": false
                    }
                },
                "content": {
                    "type": "string",
                    "description": format!("Concatenated {DEFAULT_FILENAME} text, empty when the project has none.")
                }
            },
            "required": ["files", "content"],
            "additionalProperties": false
        }))
    }

    async fn call(&self, _args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let doc = load_project_doc(config);
        let files: Vec<Value> = match doc.as_ref() {
            Some(d) => d
                .entries
                .iter()
                .map(|entry| {
                    json!({
                        "path": entry.path.to_string_lossy(),
                        "truncated": entry.truncated
                    })
                })
                .collect(),
            None => Vec::new(),
        };
        let content = doc.as_ref().map(|d| d.text.clone()).unwrap_or_default();
        let text = render_project_doc(doc.as_ref());
        let truncated = doc
            .as_ref()
            .is_some_and(|document| document.entries.iter().any(|entry| entry.truncated));

        ToolResult::text(text)
            .with_structured(json!({ "files": files, "content": content }))
            .with_truncation(truncated)
    }
}
