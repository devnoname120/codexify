//! The `Tool` trait every registered tool implements.
//!
//! Ports the `ToolDefinition` interface from `src/types.ts`. Tools are held as
//! `Box<dyn Tool>` in the registry and dispatched by name, so the trait is
//! object-safe via `async_trait`.

use std::sync::Arc;

use async_trait::async_trait;
use rmcp::model::{Icon, MetaObject, ToolAnnotations};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::artifact_egress::ArtifactEgressStore;
use crate::conversation_auth::ConversationAuthorizationStore;
use crate::exec_sessions::SessionState;
use crate::project_bindings::ConversationIdentity;
use crate::review::ReviewCheckpointManager;
use crate::types::{AppConfig, ToolResult};

#[derive(Clone)]
pub struct ToolRequestContext {
    pub conversation: Option<ConversationIdentity>,
    pub conversation_authorizations: Arc<ConversationAuthorizationStore>,
    pub review_checkpoints: Arc<ReviewCheckpointManager>,
    pub artifact_egress: Arc<ArtifactEgressStore>,
    /// Cancelled when the transport drops or a per-call deadline (e.g. the
    /// artifact-ingress idle timeout) fires, so long-running tools can abort.
    pub cancellation: CancellationToken,
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

    /// Optional human-readable title for hosts that render tool cards.
    fn title(&self) -> Option<String> {
        None
    }

    /// Optional MCP tool annotations (read-only / destructive hints).
    fn annotations(&self) -> Option<ToolAnnotations> {
        None
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

    /// Whether dispatch must fail closed if the initial review checkpoint cannot
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
