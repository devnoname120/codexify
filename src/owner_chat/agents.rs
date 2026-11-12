use super::*;
use crate::agent_automation::agents::{AgentControlRequest, AgentRuntime, SpawnWorkersRequest};

pub(super) async fn control(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(body): Json<AgentControlRequest>,
) -> ApiResult {
    let located = selected(&state, &id)?;
    let task = automation_task(&located)?;
    Ok(Json(
        runtime(&state)?
            .control(&task, body)
            .await
            .map_err(internal_error)?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub(super) struct StopTeamBody {request_id:String}

pub(super) async fn stop_team(State(state):State<OwnerState>,Path(id):Path<String>,Json(body):Json<StopTeamBody>)->ApiResult{
    validate_owner_request_id(&body.request_id)?;
    let located=selected(&state,&id)?;let task=automation_task(&located)?;
    Ok(Json(runtime(&state)?.stop_team(&task,&body.request_id).await.map_err(internal_error)?))
}

fn runtime(state: &OwnerState) -> Result<AgentRuntime, (StatusCode, String)> {
    Ok(AgentRuntime::new(
        state.config.clone(),
        state.chats.clone(),
        state.project_bindings.clone(),
        state.continuations.clone(),
        state
            .automation
            .clone()
            .ok_or_else(|| internal_error("Enable browser automation before creating agents"))?,
    ))
}

pub(super) async fn status(State(state): State<OwnerState>, Path(id): Path<String>) -> ApiResult {
    let located = selected(&state, &id)?;
    let task = automation_task(&located)?;
    Ok(Json(
        runtime(&state)?
            .status(&task)
            .await
            .map_err(internal_error)?,
    ))
}

pub(super) async fn spawn(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(body): Json<SpawnWorkersRequest>,
) -> ApiResult {
    let located = selected(&state, &id)?;
    let task = automation_task(&located)?;
    Ok(Json(
        runtime(&state)?
            .spawn(&task, body, true)
            .await
            .map_err(internal_error)?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct SettingsBody {
    request_id: String,
    enabled: Option<bool>,
    full_development_access: Option<bool>,
}

pub(super) async fn settings(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(body): Json<SettingsBody>,
) -> ApiResult {
    validate_owner_request_id(&body.request_id)?;
    let located = selected(&state, &id)?;
    let task = automation_task(&located)?;
    if body.enabled.is_none() && body.full_development_access.is_none(){return Err(internal_error("Choose a team setting"));}
    let runtime=runtime(&state)?;
    if let Some(access)=body.full_development_access {
        runtime.set_full_development_access(&task,access,&body.request_id).await.map_err(internal_error)?;
    }
    if let Some(enabled)=body.enabled {runtime.set_delegation(&task,enabled).await.map_err(internal_error)?;}
    Ok(Json(json!({"enabled":body.enabled,"fullDevelopmentAccess":body.full_development_access})))
}

pub(crate) const AUTO_MODE_PREFIX: &str = "Auto mode\n\nObjective:\n";

fn auto_mode_goal(prompt: &str) -> Result<String, String> {
    if prompt.trim().is_empty() || prompt.len() > 6000 || prompt.contains('\0') {
        return Err("Describe the result to achieve in 1–6000 bytes".into());
    }
    Ok(format!("{AUTO_MODE_PREFIX}{}\n\nWorking agreement:\n\
1. Read goal_read, rollout_read and the project instructions. Define concrete acceptance checks and a short prioritized plan. Preserve existing work and data.\n\
2. You own implementation, integration and verification. Do substantial development work yourself while agents handle distinct useful scopes. Do not become a passive dispatcher or wait for every worker before progressing.\n\
3. Start with two to four implementation workers for independent scopes, and reuse them before expanding the team. Add workers when useful work can start without browser, build or file contention; a queued reservation is not running work. Two worker levels and batches of up to eight remain available, with no fixed project active-worker limit. Every agent needs a concrete goal, owned files/responsibilities, acceptance checks and required evidence. Development access is authorized within the project; agents may implement, build, test and debug their assigned work. Respect existing file ownership and build locks. The owner full development access setting supersedes old audit-only instructions; otherwise reassign audit workers with an explicit development goal and scope.\n\
4. Keep a rolling backlog. Call agent_status after dispatch, at milestones and before waiting or completion. Review actual tool activity, reports, code and tests. Reassign finished workers with agent_control set_goal and their freshly read expectedRevision; confirm the new revision is active. Sending an instruction does not update a saved goal. Never leave useful agents idle while independent work remains. After reviewing a finished assignment, reuse the worker with a fresh set_goal or retire it through agent_control, preserving reports and history.\n\
5. Read chat_read before each work batch, after command waits and before handoff. Read agent_status for current team progress and full reports. Use agent_message for focused peer dependencies and handoffs, agent_report to save evidence and notify the coordinator directly, and rollout_update for decisions, current changes, verification and exact next steps. Resolve routine ownership and integration decisions yourself.\n\
6. Keep the user informed mainly about concrete changes, verified milestones and decisions or information only they can provide. Do not substitute plans or status chatter for implemented results.\n\
7. Iterate implementation, testing and fixes until all acceptance checks pass with fresh evidence. Investigate root causes, regressions, performance and stability. Audits, partial prototypes, successful sends and unrun tests do not establish completion.\n\
8. Continue across as many turns and replacement conversations as necessary. A short turn or context limit is a managed handoff: preserve progress, restore the journal/current goal and resume the next task automatically. Do not abandon work because time elapsed or a recoverable command/browser error occurred.\n\
9. Respect explicit user stops, pauses, approvals, actual retry deadlines and concrete persistent blockers requiring external input. Do independent work while waiting. Finish this goal only when the requested result is implemented and verified; report truthful results and remaining limits.\n", prompt.trim()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct AutoModeBody {
    request_id: String,
    expected_revision: u64,
    prompt: String,
}

pub(super) async fn auto_mode(
    State(state): State<OwnerState>, Path(id): Path<String>, Json(body): Json<AutoModeBody>,
) -> ApiResult {
    if !state.config.markdown_chat.features.auto_mode || !state.config.markdown_chat.features.agent_teams {
        return Err(internal_error("Enable Auto mode and Agent teams in settings before starting Auto mode"));
    }
    validate_owner_request_id(&body.request_id)?;
    let markdown = auto_mode_goal(&body.prompt).map_err(internal_error)?;
    require_connector_name(&state.config)?;
    let located = selected(&state, &id)?;
    let context = automation_context(&state, &located).await?;
    let saved = context.store.load(&context.task).map_err(internal_error)?;
    if saved.agents.parent_task_key.is_some() {
        return Err(internal_error("Start Auto mode from the parent chat"));
    }
    if state.project_bindings.selected_project_root(&state.config, &context.task).map_err(internal_error)?.is_none() {
        return Err(internal_error("Select a project before starting Auto mode"));
    }
    if pairing_status(&saved.pairing, context.owner_generation) != "verified"
        || saved.selection.model.is_none() || saved.selection.effort.is_none() {
        return Err(internal_error("Pair this chat and select a model before starting Auto mode"));
    }
    if !saved.automatic_retries_enabled {
        return Err(internal_error("Enable automatic retries in settings before starting Auto mode"));
    }
    let receipt = context.store.start_auto_mode(&context.task, &body.request_id,
        body.expected_revision, markdown, crate::markdown_chat::now_ms()).map_err(internal_error)?;
    let goal = receipt.goal.as_ref().ok_or_else(|| internal_error("Auto mode goal is missing"))?;
    // The durable goal receipt makes response-loss retries idempotent, while
    // ordinary unread-message delivery preserves existing accepted sends.
    context.chat.append_user(format!("auto-mode-{:x}", Sha256::digest(body.request_id.as_bytes())),
        format!("Auto mode goal revision {} is ready. Call goal_read and rollout_read, then implement the objective, assign concrete agent goals and keep the rolling backlog moving. Development access is enabled for your team; update prior read-only assignments with explicit new development goals when needed.", goal.revision))
        .await.map_err(internal_error)?;
    if !receipt.replayed {
        context.chat.resume_agent().await.map_err(internal_error)?;
    }
    if let Some(supervisor) = &state.automation { supervisor.notify(); }
    let current_goal = context.store.load(&context.task).map_err(internal_error)?.goal;
    Ok(Json(json!({"goal":current_goal,"replayed":receipt.replayed,"autoMode":true})))
}

#[cfg(test)]
mod auto_mode_tests {
    use super::*;
    #[test]
    fn prompt_requires_an_objective_and_stays_within_goal_budget() {
        assert!(auto_mode_goal("  ").is_err());
        assert!(auto_mode_goal(&"x".repeat(6001)).is_err());
        let prompt = auto_mode_goal(&"x".repeat(6000)).unwrap();
        assert!(prompt.len() <= crate::agent_automation::MAX_GOAL_BYTES);
        assert!(prompt.starts_with(AUTO_MODE_PREFIX));
    }
}
