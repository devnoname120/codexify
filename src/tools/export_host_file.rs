use async_trait::async_trait;
use rmcp::model::MetaObject;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::artifact_egress::ArtifactEgressStore;
use crate::exec_sessions::SessionState;
use crate::tool::{Tool, ToolBehavior, ToolRequestContext, parse_tool_args, schema_for};
use crate::types::{AppConfig, ToolContent, ToolResult};

pub struct ExportHostFile;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ExportHostFileArgs {
    /// Existing project-relative file path.
    #[schemars(length(min = 1))]
    path: String,
}

impl ExportHostFile {
    pub const NAME: &'static str = "export_host_file";

    async fn run(
        &self,
        args: Value,
        config: &AppConfig,
        store: &ArtifactEgressStore,
        cancellation: &CancellationToken,
    ) -> ToolResult {
        if !config.artifact_egress.enabled {
            return ToolResult::error(
                "artifact_egress_disabled: File export is disabled by configuration.",
            );
        }
        let ExportHostFileArgs { path } = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };
        let registered = match store
            .export_project_file(&config.work_dir, &path, cancellation)
            .await
        {
            Ok(registered) => registered,
            Err(error) => return ToolResult::error(error.to_string()),
        };
        let durability = if registered.snapshot_stored {
            "An immutable snapshot is retained in Codexify's durable artifact store."
        } else if registered.fallback_to_source {
            "No immutable snapshot was retained; the resource resolves to the latest safe version at the recorded project path."
        } else {
            "No immutable snapshot was retained and source fallback is disabled."
        };
        let text = format!(
            "Exported `{path}` as `{}` ({} bytes, SHA-256 {}). {durability}",
            registered.name, registered.byte_count, registered.sha256
        );
        ToolResult {
            content: vec![
                ToolContent::Text(text),
                ToolContent::ResourceLink(registered.resource),
            ],
            is_error: false,
            structured_content: Some(json!({
                "path": path,
                "name": registered.name,
                "bytes": registered.byte_count,
                "sha256": registered.sha256,
                "mimeType": registered.mime_type,
                "snapshotStored": registered.snapshot_stored,
                "fallbackToSource": registered.fallback_to_source
            })),
            meta: None,
            audit: Default::default(),
        }
    }
}

#[async_trait]
impl Tool for ExportHostFile {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn title(&self) -> String {
        "Download project file".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            false,
            "Reads a local file and creates only private durable return-resource bookkeeping without modifying project, user-owned, or external state.",
        )
    }

    fn meta(&self) -> Option<MetaObject> {
        Some(
            serde_json::from_value(json!({
                "openai/toolInvocation/invoking": "Preparing file download",
                "openai/toolInvocation/invoked": "File ready to download"
            }))
            .expect("static file-export tool metadata must be an object"),
        )
    }

    fn description(&self) -> String {
        "Export one existing file from the active project to ChatGPT as a downloadable MCP resource. The path is project-relative. Codexify retains an immutable snapshot when it fits the configured durable-store limits, otherwise resolves the resource from the latest safe source path, and exposes only an opaque capability rather than a local filesystem path."
            .to_string()
    }

    fn input_schema(&self) -> Value {
        schema_for::<ExportHostFileArgs>()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "name": { "type": "string" },
                "bytes": { "type": "integer", "minimum": 0 },
                "sha256": { "type": "string", "pattern": "^[0-9a-f]{64}$" },
                "mimeType": { "type": "string" },
                "snapshotStored": { "type": "boolean" },
                "fallbackToSource": { "type": "boolean" }
            },
            "required": [
                "path",
                "name",
                "bytes",
                "sha256",
                "mimeType",
                "snapshotStored",
                "fallbackToSource"
            ],
            "additionalProperties": false
        }))
    }

    fn fills_structured_content(&self) -> bool {
        false
    }

    async fn call(&self, _args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
        ToolResult::error(
            "artifact_egress_context_missing: File export requires a server request context.",
        )
    }

    async fn call_with_context(
        &self,
        args: Value,
        config: &AppConfig,
        _session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        self.run(
            args,
            config,
            &context.artifact_egress,
            &context.cancellation,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_a_resource_link_and_matching_structured_receipt() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("report.txt"), b"final report\n").unwrap();
        let config = crate::config::default_config(root.path().to_path_buf());
        let store = ArtifactEgressStore::new_at(
            config.artifact_egress.clone(),
            state.path().join("artifacts"),
        );
        let result = ExportHostFile
            .run(
                json!({ "path": "report.txt" }),
                &config,
                &store,
                &CancellationToken::new(),
            )
            .await;

        assert!(!result.is_error, "{}", result.joined_text());
        assert!(matches!(
            &result.content[1],
            ToolContent::ResourceLink(resource)
                if resource.name == "report.txt"
                    && resource.mime_type.as_deref() == Some("text/plain")
        ));
        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["path"], "report.txt");
        assert_eq!(structured["name"], "report.txt");
        assert_eq!(structured["bytes"], 13);
        assert_eq!(structured["mimeType"], "text/plain");
        assert_eq!(structured["snapshotStored"], true);
        assert_eq!(structured["fallbackToSource"], true);
        assert!(structured.get("expiresInMs").is_none());
        assert!(structured.get("resourceUri").is_none());
        assert!(!result.joined_text().contains("available for"));
    }

    #[test]
    fn schema_and_annotations_describe_read_only_file_egress() {
        let tool = ExportHostFile;
        assert_eq!(tool.input_schema()["required"], json!(["path"]));
        let annotations = tool.annotations().unwrap();
        assert_eq!(annotations.read_only_hint, Some(true));
        assert_eq!(annotations.destructive_hint, Some(false));
        assert_eq!(annotations.idempotent_hint, Some(true));
        assert_eq!(annotations.open_world_hint, Some(false));
    }
}
