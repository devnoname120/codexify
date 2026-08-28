use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::exec_sessions::SessionState;
use crate::memory::save_plan;
use crate::tool::{Tool, ToolBehavior, parse_tool_args, schema_for, text_output_schema};
use crate::types::{AppConfig, PlanItem, PlanState, PlanStepStatus, ToolResult};

fn marker(status: PlanStepStatus) -> &'static str {
    match status {
        PlanStepStatus::Pending => "[ ]",
        PlanStepStatus::InProgress => "[~]",
        PlanStepStatus::Completed => "[x]",
    }
}

/// Codex shows the plan in its TUI. Here the tool result is the only channel back
/// to the caller, so the stored plan is rendered in full on every update.
fn render_plan(plan: &PlanState) -> String {
    let mut lines: Vec<String> = Vec::new();
    // `if (plan.explanation)` in JS is falsy for the empty string, so an empty
    // explanation is stored but not rendered.
    if let Some(explanation) = plan.explanation.as_deref()
        && !explanation.is_empty()
    {
        lines.push(explanation.to_string());
        lines.push(String::new());
    }
    for item in &plan.plan {
        lines.push(format!("{} {}", marker(item.status), item.step));
    }
    let done = plan
        .plan
        .iter()
        .filter(|i| i.status == PlanStepStatus::Completed)
        .count();
    lines.push(String::new());
    lines.push(format!("{done}/{} steps completed", plan.plan.len()));
    lines.join("\n")
}

pub struct UpdatePlan;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpdatePlanArgs {
    /// Optional explanation for this update.
    explanation: Option<String>,
    /// Complete replacement plan.
    plan: Vec<PlanItem>,
}

#[async_trait]
impl Tool for UpdatePlan {
    fn name(&self) -> &'static str {
        "update_plan"
    }

    fn title(&self) -> String {
        "Update plan".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            false,
            true,
            true,
            false,
            "Replaces the persisted task plan, overwriting any previous plan state.",
        )
    }

    fn description(&self) -> String {
        "Updates the task plan.\nProvide an optional explanation and a list of plan items, each with a step and status.\nAt most one step can be in_progress at a time.\nUse this to track multi-step work: post the full plan up front, then re-send the whole list with updated statuses as you go. The plan is echoed back on each update, and saved so that recall can hand it back in a later conversation.".into()
    }

    fn input_schema(&self) -> Value {
        schema_for::<UpdatePlanArgs>()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(text_output_schema())
    }

    async fn call(&self, args: Value, config: &AppConfig, session: &SessionState) -> ToolResult {
        let UpdatePlanArgs { explanation, plan } = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };
        for (index, item) in plan.iter().enumerate() {
            if item.step.trim().is_empty() {
                return ToolResult::error(format!("plan[{index}].step must be a non-empty string"));
            }
        }

        let in_progress: Vec<&PlanItem> = plan
            .iter()
            .filter(|i| i.status == PlanStepStatus::InProgress)
            .collect();
        if in_progress.len() > 1 {
            let steps = in_progress
                .iter()
                .map(|i| serde_json::to_string(&i.step).unwrap_or_default())
                .collect::<Vec<_>>()
                .join(", ");
            return ToolResult::error(format!(
                "At most one step can be in_progress at a time (got {}: {})",
                in_progress.len(),
                steps
            ));
        }

        let state = PlanState { explanation, plan };

        let rendered = render_plan(&state);
        *session.plan.lock().unwrap() = Some(state.clone());

        // Best effort: a plan the model can see is worth more than a plan that
        // failed to persist, so a read-only state directory must not fail the call.
        save_plan(config, Some(state));

        ToolResult::text(rendered)
    }
}
