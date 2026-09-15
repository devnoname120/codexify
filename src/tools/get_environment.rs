use async_trait::async_trait;
use serde_json::{Value, json};

use crate::environment::{describe_environment, render_environment};
use crate::exec_sessions::SessionState;
use crate::tool::{Tool, ToolBehavior, empty_object_schema};
use crate::types::{AppConfig, ToolResult};

pub struct GetEnvironment;

#[async_trait]
impl Tool for GetEnvironment {
    fn name(&self) -> &'static str {
        "get_environment"
    }

    fn title(&self) -> String {
        "Get environment".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            false,
            "Reads local environment and command-runtime metadata without changing state.",
        )
    }

    fn description(&self) -> String {
        "Report the machine this bridge is running on: operating system, the shell exec_command will use, the working directory, and the concurrent-session limit. Command execution is unrestricted. Call this before writing any shell command — the same command string behaves differently under PowerShell, cmd and POSIX sh, and guessing wrong wastes a turn.".into()
    }

    fn input_schema(&self) -> Value {
        empty_object_schema()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "os": { "type": "string", "description": "Friendly OS name: Windows, macOS, Linux." },
                "platform": { "type": "string", "description": "Node platform identifier, e.g. win32, darwin, linux." },
                "arch": { "type": "string", "description": "CPU architecture, e.g. x64, arm64." },
                "cwd": { "type": "string", "description": "Absolute path of the work directory all tools operate on." },
                "path_separator": { "type": "string", "description": "Native path separator on this host." },
                "shell": {
                    "type": "object",
                    "description": "The shell exec_command launches when a call names none.",
                    "properties": {
                        "bin": { "type": "string", "description": "Shell binary path." },
                        "type": { "type": "string", "enum": ["posix", "powershell", "cmd"], "description": "Syntax family the shell expects." },
                        "argv_prefix": { "type": "array", "items": { "type": "string" }, "description": "Arguments placed before the command string." }
                    },
                    "required": ["bin", "type", "argv_prefix"],
                    "additionalProperties": false
                },
                "exec": {
                    "type": "object",
                    "description": "Runtime resource controls for unrestricted exec_command sessions.",
                    "properties": {
                        "max_sessions": { "type": "integer", "minimum": 0, "description": "Cap on concurrent background exec sessions." }
                    },
                    "required": ["max_sessions"],
                    "additionalProperties": false
                }
            },
            "required": ["os", "platform", "arch", "cwd", "path_separator", "shell", "exec"],
            "additionalProperties": false
        }))
    }

    async fn call(&self, _args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let info = describe_environment(config);
        let text = render_environment(&info);
        let structured = serde_json::to_value(&info).unwrap_or(Value::Null);
        ToolResult::text(text).with_structured(structured)
    }
}
