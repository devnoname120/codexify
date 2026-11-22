use async_trait::async_trait;
use rmcp::model::{Resource, ResourceContents};
use serde_json::Value;

use crate::exec_sessions::SessionState;
use crate::tool::{Tool, ToolBehavior, empty_object_schema};
use crate::types::{AppConfig, ToolContent, ToolResult};

pub const URI: &str = "codexify://materialization/sentinel.txt";
pub struct MaterializeSentinel;

pub(crate) fn contents(enabled: bool, uri: &str) -> Option<ResourceContents> {
    (enabled && uri == URI)
        .then(|| ResourceContents::blob("c2VudGluZWwK", URI).with_mime_type("text/plain"))
}

#[async_trait]
impl Tool for MaterializeSentinel {
    fn name(&self) -> &'static str {
        "materialize_sentinel"
    }
    fn title(&self) -> String {
        "Materialize sentinel file".into()
    }
    fn description(&self) -> String {
        "Materialize the dummy sentinel.txt file once during conversation setup.".into()
    }
    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            false,
            "Returns a fixed dummy file without reading or modifying project files.",
        )
    }
    fn requires_project_root(&self) -> bool {
        false
    }
    fn input_schema(&self) -> Value {
        empty_object_schema()
    }
    async fn call(&self, _: Value, config: &AppConfig, _: &SessionState) -> ToolResult {
        if !config.markdown_chat.automation.prime_file_materialization {
            return ToolResult::error("Setup file materialization is disabled.");
        }
        let mut result = ToolResult::text("sentinel.txt");
        result.content.push(ToolContent::ResourceLink(
            Resource::new(URI, "sentinel.txt")
                .with_mime_type("text/plain")
                .with_size(9),
        ));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn sentinel_is_opt_in_and_never_creates_a_project_file() {
        let root = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(root.path().into());
        assert!(
            MaterializeSentinel
                .call(serde_json::json!({}), &config, &SessionState::new())
                .await
                .is_error
        );
        assert!(contents(false, URI).is_none());
        config.markdown_chat.automation.prime_file_materialization = true;
        let result = MaterializeSentinel
            .call(serde_json::json!({}), &config, &SessionState::new())
            .await;
        assert!(!result.is_error);
        assert!(
            matches!(&result.content[1], ToolContent::ResourceLink(resource) if resource.uri == URI)
        );
        assert!(contents(true, URI).is_some());
        assert!(contents(true, "file:///private/file").is_none());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}
