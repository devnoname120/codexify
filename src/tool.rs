//! The `Tool` trait every registered tool implements.
//!
//! Ports the `ToolDefinition` interface from `src/types.ts`. Tools are held as
//! `Box<dyn Tool>` in the registry and dispatched by name, so the trait is
//! object-safe via `async_trait`.

use std::sync::Arc;

use async_trait::async_trait;
use jsonschema::Validator;
use rmcp::model::{Icon, MetaObject, ToolAnnotations};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::artifact_egress::ArtifactEgressStore;
use crate::conversation_auth::ConversationAuthorizationStore;
use crate::diff::DiffCheckpointManager;
use crate::exec_sessions::SessionState;
use crate::project_bindings::ConversationIdentity;
use crate::types::{AppConfig, ToolResult};

#[derive(Clone)]
pub struct ToolRequestContext {
    pub conversation: Option<ConversationIdentity>,
    pub conversation_authorizations: Arc<ConversationAuthorizationStore>,
    pub diff_checkpoints: Arc<DiffCheckpointManager>,
    pub artifact_egress: Arc<ArtifactEgressStore>,
    /// Cancelled when the transport drops or a per-call deadline (e.g. the
    /// artifact-ingress idle timeout) fires, so long-running tools can abort.
    pub cancellation: CancellationToken,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallIdentity {
    pub downstream_tool: String,
    pub mcp_server: Option<String>,
    pub mcp_tool: Option<String>,
}

impl ToolCallIdentity {
    pub fn native(tool: impl Into<String>) -> Self {
        Self {
            downstream_tool: tool.into(),
            mcp_server: None,
            mcp_tool: None,
        }
    }

    pub fn mcp(
        downstream_tool: impl Into<String>,
        server: impl Into<String>,
        tool: Option<String>,
    ) -> Self {
        Self {
            downstream_tool: downstream_tool.into(),
            mcp_server: Some(server.into()),
            mcp_tool: tool,
        }
    }

    pub fn resolved_tool(&self) -> String {
        match (&self.mcp_server, &self.mcp_tool) {
            (Some(server), Some(tool)) => format!("mcp:{server}/{tool}"),
            (Some(server), None) => format!("mcp:{server}/<unresolved>"),
            _ => self.downstream_tool.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolBehavior {
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
    pub open_world: bool,
    pub justification: &'static str,
}

impl ToolBehavior {
    pub const fn new(
        read_only: bool,
        destructive: bool,
        idempotent: bool,
        open_world: bool,
        justification: &'static str,
    ) -> Self {
        Self {
            read_only,
            destructive,
            idempotent,
            open_world,
            justification,
        }
    }

    pub fn annotations(self) -> ToolAnnotations {
        ToolAnnotations::new()
            .read_only(self.read_only)
            .destructive(self.destructive)
            .idempotent(self.idempotent)
            .open_world(self.open_world)
    }
}

pub fn empty_object_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

pub fn text_output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "content": { "type": "string" }
        },
        "required": ["content"],
        "additionalProperties": false
    })
}

pub fn schema_for<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).expect("generated tool schema must serialize")
}

pub fn parse_tool_args<T: DeserializeOwned>(args: Value) -> Result<T, Box<ToolResult>> {
    serde_json::from_value(args).map_err(|error| {
        Box::new(ToolResult::error(format!(
            "Invalid tool arguments: {error}"
        )))
    })
}

#[async_trait]
pub trait Tool: Send + Sync {
    /// MCP tool name (e.g. `read_file`). Must match `^[a-zA-Z0-9_-]{1,64}$`.
    fn name(&self) -> &'static str;

    /// The static description advertised in `tools/list`.
    fn description(&self) -> String;

    /// Description to advertise instead of [`Self::description`], for tools whose
    /// wording depends on runtime configuration. `exec_command` uses it to name
    /// the shell it will actually launch, which is not knowable at load time.
    fn describe(&self, _config: &AppConfig) -> String {
        self.description()
    }

    /// Human-readable title for hosts that render tool cards.
    fn title(&self) -> String;

    /// Complete host-facing side-effect classification and its rationale.
    fn behavior(&self) -> ToolBehavior;

    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(self.behavior().annotations())
    }

    /// Optional icons advertised by the tool.
    fn icons(&self) -> Option<Vec<Icon>> {
        None
    }

    /// Optional protocol metadata, including MCP Apps resource links.
    fn meta(&self) -> Option<MetaObject> {
        None
    }

    /// The JSON Schema object for the tool's arguments.
    fn input_schema(&self) -> Value;

    /// Native fixed-shape tools close their argument object. Directly bridged
    /// upstream schemas retain the upstream server's own additional-property policy.
    fn requires_closed_input_schema(&self) -> bool {
        true
    }

    fn requires_closed_output_schema(&self) -> bool {
        true
    }

    /// Resolve the operational identity logged for this call. MCP dispatchers
    /// override this so logs name the raw upstream server/tool rather than only
    /// the downstream proxy or gateway function.
    fn call_identity(&self, _args: &Value) -> ToolCallIdentity {
        ToolCallIdentity::native(self.name())
    }

    /// Optional JSON Schema object for the structured result. Tools that set one
    /// get the `structuredContent` default-fill in the server unless they build
    /// their own.
    fn output_schema(&self) -> Option<Value> {
        None
    }

    /// Whether the server should fill in a default `{ content: <text> }`
    /// structured result when this tool advertises an `outputSchema` but returns
    /// none. Native tools whose text *is* the structured form want this; bridged
    /// tools pass the upstream result through verbatim and opt out.
    fn fills_structured_content(&self) -> bool {
        true
    }

    /// Directly bridged upstream tools may advertise a schema yet return only
    /// unstructured content; native tools must satisfy their declared schema.
    fn permits_missing_structured_content(&self) -> bool {
        false
    }

    fn validate_arguments(&self, _args: &Value) -> Result<(), String> {
        Ok(())
    }

    fn validate_structured_output(&self, _value: Option<&Value>) -> Result<(), String> {
        Ok(())
    }

    /// Whether this tool applies the configured model-output policy internally.
    /// Structured command results need to budget their nested `output` field
    /// before serialising the surrounding receipt.
    fn manages_model_output_budget(&self) -> bool {
        false
    }

    /// Whether this tool needs an active project root for the current call.
    /// Upstream tools and project-independent clocks opt out.
    fn requires_project_root(&self) -> bool {
        true
    }

    /// Whether resident command state should follow a stable ChatGPT conversation
    /// across replacement MCP transports. Only the unified exec pair opts in;
    /// other mutable tool state retains transport-session ownership.
    fn uses_exec_session_state(&self) -> bool {
        false
    }

    /// Whether dispatch must fail closed if the initial diff checkpoint cannot
    /// be captured for a Git project.
    fn may_modify_project(&self) -> bool {
        false
    }

    /// Run the tool. `args` is the arguments object (or `Value::Null` when the
    /// call named none).
    async fn call(&self, args: Value, config: &AppConfig, session: &SessionState) -> ToolResult;

    async fn call_with_context(
        &self,
        args: Value,
        config: &AppConfig,
        session: &SessionState,
        _context: &ToolRequestContext,
    ) -> ToolResult {
        self.call(args, config, session).await
    }
}

struct ValidatedTool {
    inner: Box<dyn Tool>,
    input_schema: Value,
    output_schema: Option<Value>,
    input_validator: Validator,
    output_validator: Option<Validator>,
}

impl ValidatedTool {
    fn new(inner: Box<dyn Tool>) -> Result<Self, String> {
        let name = inner.name();
        if inner.title().trim().is_empty() {
            return Err(format!("tool `{name}` has an empty title"));
        }
        if inner.description().trim().is_empty() {
            return Err(format!("tool `{name}` has an empty description"));
        }
        if inner.behavior().justification.trim().is_empty() {
            return Err(format!("tool `{name}` has no annotation justification"));
        }

        let input_schema = inner.input_schema();
        validate_schema_shape(name, "input", &input_schema)?;
        if inner.requires_closed_input_schema()
            && input_schema.get("additionalProperties") != Some(&Value::Bool(false))
        {
            return Err(format!(
                "tool `{name}` must set inputSchema.additionalProperties to false"
            ));
        }
        let input_validator = compile_schema(name, "input", &input_schema)?;

        let output_schema = inner.output_schema();
        let output_validator = output_schema
            .as_ref()
            .map(|schema| {
                validate_schema_shape(name, "output", schema)?;
                if inner.requires_closed_output_schema()
                    && schema.get("additionalProperties") != Some(&Value::Bool(false))
                {
                    return Err(format!(
                        "tool `{name}` must set outputSchema.additionalProperties to false"
                    ));
                }
                compile_schema(name, "output", schema)
            })
            .transpose()?;

        Ok(Self {
            inner,
            input_schema,
            output_schema,
            input_validator,
            output_validator,
        })
    }
}

fn validate_schema_shape(name: &str, kind: &str, schema: &Value) -> Result<(), String> {
    let Some(object) = schema.as_object() else {
        return Err(format!("tool `{name}` {kind} schema must be an object"));
    };
    if object.get("type").and_then(Value::as_str) != Some("object") {
        return Err(format!(
            "tool `{name}` {kind} schema must declare type=object"
        ));
    }
    Ok(())
}

fn compile_schema(name: &str, kind: &str, schema: &Value) -> Result<Validator, String> {
    jsonschema::options()
        .build(schema)
        .map_err(|error| format!("tool `{name}` has an invalid {kind} schema: {error}"))
}

fn validation_error(validator: &Validator, value: &Value) -> Option<String> {
    let errors = validator
        .iter_errors(value)
        .take(3)
        .map(|error| {
            let path = error.instance_path().to_string();
            if path.is_empty() {
                error.masked().to_string()
            } else {
                format!("{path}: {}", error.masked())
            }
        })
        .collect::<Vec<_>>();
    (!errors.is_empty()).then(|| errors.join("; "))
}

pub fn validate_and_wrap_tools(tools: Vec<Box<dyn Tool>>) -> Result<Vec<Box<dyn Tool>>, String> {
    let mut seen = std::collections::HashSet::new();
    let mut validated = Vec::with_capacity(tools.len());
    for tool in tools {
        if !seen.insert(tool.name()) {
            return Err(format!("duplicate tool name: {}", tool.name()));
        }
        validated.push(validate_and_wrap_tool(tool)?);
    }
    Ok(validated)
}

pub fn validate_and_wrap_tool(tool: Box<dyn Tool>) -> Result<Box<dyn Tool>, String> {
    Ok(Box::new(ValidatedTool::new(tool)?) as Box<dyn Tool>)
}

#[async_trait]
impl Tool for ValidatedTool {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn description(&self) -> String {
        self.inner.description()
    }

    fn describe(&self, config: &AppConfig) -> String {
        self.inner.describe(config)
    }

    fn title(&self) -> String {
        self.inner.title()
    }

    fn behavior(&self) -> ToolBehavior {
        self.inner.behavior()
    }

    fn annotations(&self) -> Option<ToolAnnotations> {
        self.inner.annotations()
    }

    fn icons(&self) -> Option<Vec<Icon>> {
        self.inner.icons()
    }

    fn meta(&self) -> Option<MetaObject> {
        self.inner.meta()
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    fn requires_closed_input_schema(&self) -> bool {
        self.inner.requires_closed_input_schema()
    }

    fn requires_closed_output_schema(&self) -> bool {
        self.inner.requires_closed_output_schema()
    }

    fn call_identity(&self, args: &Value) -> ToolCallIdentity {
        self.inner.call_identity(args)
    }

    fn output_schema(&self) -> Option<Value> {
        self.output_schema.clone()
    }

    fn fills_structured_content(&self) -> bool {
        self.inner.fills_structured_content()
    }

    fn permits_missing_structured_content(&self) -> bool {
        self.inner.permits_missing_structured_content()
    }

    fn validate_arguments(&self, args: &Value) -> Result<(), String> {
        validation_error(&self.input_validator, args).map_or(Ok(()), Err)
    }

    fn validate_structured_output(&self, value: Option<&Value>) -> Result<(), String> {
        let Some(validator) = &self.output_validator else {
            return Ok(());
        };
        let Some(value) = value else {
            return if self.inner.permits_missing_structured_content() {
                Ok(())
            } else {
                Err("missing structuredContent required by outputSchema".to_string())
            };
        };
        validation_error(validator, value).map_or(Ok(()), Err)
    }

    fn manages_model_output_budget(&self) -> bool {
        self.inner.manages_model_output_budget()
    }

    fn requires_project_root(&self) -> bool {
        self.inner.requires_project_root()
    }

    fn uses_exec_session_state(&self) -> bool {
        self.inner.uses_exec_session_state()
    }

    fn may_modify_project(&self) -> bool {
        self.inner.may_modify_project()
    }

    async fn call(&self, args: Value, config: &AppConfig, session: &SessionState) -> ToolResult {
        self.inner.call(args, config, session).await
    }

    async fn call_with_context(
        &self,
        args: Value,
        config: &AppConfig,
        session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        self.inner
            .call_with_context(args, config, session, context)
            .await
    }
}

/// Read a string argument by key, or `None` when absent or not a string.
pub fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

/// Read a bool argument by key, defaulting to `false` when absent.
pub fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Read a `u64` argument by key when present and numeric. Integer-valued JSON
/// floats (e.g. `5.0`, common from non-JS clients) are accepted, matching the
/// TS which treats every JSON number uniformly.
pub fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    let v = args.get(key)?;
    v.as_u64().or_else(|| {
        v.as_f64()
            .filter(|f| f.is_finite() && *f >= 0.0 && f.fract() == 0.0)
            .map(|f| f as u64)
    })
}

/// Read an `f64` argument by key when present and numeric.
pub fn arg_f64(args: &Value, key: &str) -> Option<f64> {
    args.get(key).and_then(|v| v.as_f64())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::compile_schema;

    #[test]
    fn schema_compilation_honors_an_explicit_upstream_dialect() {
        let schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {
                "pair": {
                    "type": "array",
                    "items": [
                        { "type": "string" },
                        { "type": "integer" }
                    ],
                    "additionalItems": false
                }
            },
            "required": ["pair"],
            "additionalProperties": false
        });
        let validator = compile_schema("fixture", "input", &schema).unwrap();
        assert!(validator.is_valid(&json!({ "pair": ["value", 7] })));
        assert!(!validator.is_valid(&json!({ "pair": [7, "value"] })));
    }
}
