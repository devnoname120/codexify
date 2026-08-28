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
            "Reads local environment and command-policy metadata without changing state.",
        )
    }

    fn description(&self) -> String {
        "Report the machine this bridge is running on: operating system, the shell exec_command will use, the working directory, and which commands the policy allows. Call this before writing any shell command — the same command string behaves differently under PowerShell, cmd and POSIX sh, and guessing wrong wastes a turn.".into()
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
                    "description": "Policy applied to exec_command.",
                    "properties": {
                        "mode": { "type": "string", "enum": ["allowlist", "unrestricted"], "description": "Whether commands are checked against an allowlist." },
                        "max_sessions": { "type": "integer", "minimum": 0, "description": "Cap on concurrent background exec sessions." },
                        "allowed_commands": { "type": "array", "items": { "type": "string" }, "description": "Commands exec_command accepts under allowlist mode." }
                    },
                    "required": ["mode", "max_sessions", "allowed_commands"],
                    "additionalProperties": false
                },
                "run_command_allowed": { "type": "array", "items": { "type": "string" }, "description": "Commands run_command accepts, which is the narrower list." }
            },
            "required": ["os", "platform", "arch", "cwd", "path_separator", "shell", "exec", "run_command_allowed"],
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
