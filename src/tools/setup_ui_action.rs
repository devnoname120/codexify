use async_trait::async_trait;
use rmcp::model::MetaObject;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::tool::{Tool, ToolBehavior, ToolRequestContext};
use crate::types::{AppConfig, ToolResult};

pub const SELECT_NAME: &str = "setup_ui_select_project";

pub enum SetupUiAction {
    Projects,
    Select,
    Update,
}

impl SetupUiAction {
    fn inner(&self) -> &dyn Tool {
        match self {
            Self::Projects => &super::list_projects::ListProjects,
            Self::Select => &super::set_project_root::SetProjectRoot,
            Self::Update => &super::self_update::SelfUpdate,
        }
    }
}

#[async_trait]
impl Tool for SetupUiAction {
    fn name(&self) -> &'static str {
        match self {
            Self::Projects => "setup_ui_list_projects",
            Self::Select => SELECT_NAME,
            Self::Update => "setup_ui_update",
        }
    }

    fn title(&self) -> String {
        self.inner().title()
    }

    fn description(&self) -> String {
        format!("App-only setup action. {}", self.inner().description())
    }

    fn behavior(&self) -> ToolBehavior {
        self.inner().behavior()
    }

    fn meta(&self) -> Option<MetaObject> {
        let mut meta = self.inner().meta().unwrap_or_default();
        let ui = meta.0.entry("ui").or_insert_with(|| json!({}));
        ui["visibility"] = json!(["app"]);
        meta.0.insert("openai/visibility".into(), json!("private"));
        meta.0.insert("openai/widgetAccessible".into(), json!(true));
        Some(meta)
    }

    fn input_schema(&self) -> Value {
        self.inner().input_schema()
    }

    fn output_schema(&self) -> Option<Value> {
        self.inner().output_schema()
    }

    fn fills_structured_content(&self) -> bool {
        self.inner().fills_structured_content()
    }

    fn requires_project_root(&self) -> bool {
        self.inner().requires_project_root()
    }

    async fn call(&self, args: Value, config: &AppConfig, session: &SessionState) -> ToolResult {
        self.inner().call(args, config, session).await
    }

    async fn call_with_context(
        &self,
        args: Value,
        config: &AppConfig,
        session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        self.inner()
            .call_with_context(args, config, session, context)
            .await
    }
}
